//! Rudder Image store (Cloudflare R2 / S3) settings, per-service auto-deploy/retention
//! policy, image upload/push with R2 retention pruning, and image deployment.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::Json;
use base64::Engine;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rudder_core::{
    image_object_key, select_images_to_prune_protecting, validate_image_ref, validate_image_tag,
    validate_prefix, Compression, ImageVersionEntry,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::audit;
use crate::auth::AuthUser;
use crate::backup::pipeline;
use crate::deploy::{self, DeploymentView};
use crate::error::ApiError;
use crate::events::{self, Event};
use crate::s3;
use crate::state::AppState;

use super::catalog::{bad, conflict, core_error, not_found};

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ImageStoreSettingsView {
    pub destination_id: Option<Uuid>,
    pub destination_name: Option<String>,
    pub bucket: Option<String>,
    pub path_prefix: String,
    pub r2_keep_count: Option<i32>,
    pub r2_keep_days: Option<i32>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateImageStoreSettings {
    pub destination_id: Option<Uuid>,
    pub path_prefix: Option<String>,
    pub r2_keep_count: Option<i32>,
    pub r2_keep_days: Option<i32>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ServiceImagePolicyView {
    pub service_id: Uuid,
    pub image_auto_deploy: bool,
    pub image_r2_keep_count: Option<i32>,
    pub image_r2_keep_days: Option<i32>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateServiceImagePolicy {
    pub image_auto_deploy: bool,
    pub image_r2_keep_count: Option<i32>,
    pub image_r2_keep_days: Option<i32>,
}

#[derive(Serialize, Clone, ToSchema, sqlx::FromRow)]
pub struct RudderImageView {
    pub id: Uuid,
    pub service_id: Uuid,
    pub destination_id: Option<Uuid>,
    pub tag: String,
    pub image_ref: String,
    pub git_sha: Option<String>,
    pub object_key: String,
    pub compression: String,
    pub size_bytes: i64,
    pub sha256: String,
    pub r2_present: bool,
    pub pushed_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize, Default, ToSchema)]
pub struct PushImageRequest {
    pub tag: String,
    pub image_ref: Option<String>,
    pub git_sha: Option<String>,
    pub compression: Option<String>,
    pub expected_sha256: Option<String>,
    pub archive_base64: Option<String>,
    pub deploy: Option<bool>,
    pub r2_keep_count: Option<i32>,
}

#[derive(Deserialize, Default)]
pub struct PushImageQuery {
    pub tag: Option<String>,
    pub image_ref: Option<String>,
    pub git_sha: Option<String>,
    pub compression: Option<String>,
    pub expected_sha256: Option<String>,
    pub deploy: Option<bool>,
    pub r2_keep_count: Option<i32>,
}

#[derive(Serialize, ToSchema)]
pub struct PushedImageResponse {
    pub image: RudderImageView,
    pub auto_deploy: bool,
    pub deployed: bool,
    pub deployment: Option<DeploymentView>,
    pub deploy_error: Option<String>,
    pub pruned_tags: Vec<String>,
}

#[derive(Deserialize, Default, ToSchema)]
pub struct DeployRudderImageRequest {
    pub tag: Option<String>,
    pub image_id: Option<Uuid>,
}

#[derive(Deserialize, Default)]
pub struct DeleteImageQuery {
    pub r2_only: Option<bool>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route(
            "/api/v1/image-store/settings",
            get(get_image_store_settings).put(update_image_store_settings),
        )
        .route(
            "/api/v1/services/{id}/image-policy",
            get(get_service_image_policy).put(update_service_image_policy),
        )
        .route(
            "/api/v1/services/{id}/images",
            get(list_service_images).post(push_service_image),
        )
        .route(
            "/api/v1/services/{id}/images/deploy",
            post(deploy_service_image_by_selector),
        )
        .route(
            "/api/v1/services/{id}/images/{image_id}/deploy",
            post(deploy_service_image_by_id),
        )
        .route(
            "/api/v1/services/{id}/images/{image_id}",
            delete(delete_service_image),
        )
}

pub(super) async fn load_store_settings(
    pool: &sqlx::PgPool,
) -> Result<ImageStoreSettingsView, ApiError> {
    sqlx::query_as::<_, ImageStoreSettingsView>(
        r#"
        SELECT s.destination_id,
               d.name AS destination_name,
               d.bucket AS bucket,
               s.path_prefix,
               s.r2_keep_count,
               s.r2_keep_days,
               s.updated_at
        FROM image_store_settings s
        LEFT JOIN s3_destinations d ON d.id = s.destination_id
        WHERE s.id = TRUE
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)
}

async fn get_image_store_settings(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<ImageStoreSettingsView>, ApiError> {
    let view = load_store_settings(&state.pool).await?;
    Ok(Json(view))
}

async fn update_image_store_settings(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<UpdateImageStoreSettings>,
) -> Result<Json<ImageStoreSettingsView>, ApiError> {
    auth.require_write()?;
    if let Some(dest_id) = body.destination_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM s3_destinations WHERE id = $1)")
                .bind(dest_id)
                .fetch_one(&state.pool)
                .await
                .map_err(ApiError::database)?;
        if !exists {
            return Err(not_found("S3 destination not found"));
        }
    }
    let current = load_store_settings(&state.pool).await?;
    let path_prefix = match body.path_prefix.as_deref() {
        Some(raw) => {
            let trimmed = raw.trim().trim_matches('/');
            if trimmed.is_empty() {
                "rudder-images".to_owned()
            } else {
                validate_prefix(trimmed).map_err(core_error)?;
                trimmed.to_owned()
            }
        }
        None => current.path_prefix,
    };
    let r2_keep_count = body.r2_keep_count.filter(|v| *v >= 1);
    let r2_keep_days = body.r2_keep_days.filter(|v| *v >= 1);

    sqlx::query(
        r#"
        UPDATE image_store_settings
        SET destination_id = $1,
            path_prefix = $2,
            r2_keep_count = $3,
            r2_keep_days = $4,
            updated_at = now()
        WHERE id = TRUE
        "#,
    )
    .bind(body.destination_id)
    .bind(&path_prefix)
    .bind(r2_keep_count)
    .bind(r2_keep_days)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;

    audit::record(
        &state.pool,
        Some(auth.id),
        "image_store.update",
        "image_store_settings",
        None,
        json!({
            "destination_id": body.destination_id,
            "path_prefix": path_prefix,
            "r2_keep_count": r2_keep_count,
            "r2_keep_days": r2_keep_days,
        }),
        None,
    )
    .await;

    let updated = load_store_settings(&state.pool).await?;
    Ok(Json(updated))
}

async fn get_service_image_policy(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(service_id): Path<Uuid>,
) -> Result<Json<ServiceImagePolicyView>, ApiError> {
    let row = sqlx::query_as::<_, ServiceImagePolicyView>(
        r#"
        SELECT id AS service_id, image_auto_deploy, image_r2_keep_count, image_r2_keep_days
        FROM services
        WHERE id = $1
        "#,
    )
    .bind(service_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("service not found"))?;
    Ok(Json(row))
}

async fn update_service_image_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(service_id): Path<Uuid>,
    Json(body): Json<UpdateServiceImagePolicy>,
) -> Result<Json<ServiceImagePolicyView>, ApiError> {
    auth.require_write()?;
    let keep_count = body.image_r2_keep_count.filter(|v| *v >= 1);
    let keep_days = body.image_r2_keep_days.filter(|v| *v >= 1);

    let row = sqlx::query_as::<_, ServiceImagePolicyView>(
        r#"
        UPDATE services
        SET image_auto_deploy = $2,
            image_r2_keep_count = $3,
            image_r2_keep_days = $4,
            updated_at = now()
        WHERE id = $1
        RETURNING id AS service_id, image_auto_deploy, image_r2_keep_count, image_r2_keep_days
        "#,
    )
    .bind(service_id)
    .bind(body.image_auto_deploy)
    .bind(keep_count)
    .bind(keep_days)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("service not found"))?;

    audit::record(
        &state.pool,
        Some(auth.id),
        "service.image_policy",
        "service",
        Some(&service_id.to_string()),
        json!({
            "image_auto_deploy": row.image_auto_deploy,
            "image_r2_keep_count": row.image_r2_keep_count,
            "image_r2_keep_days": row.image_r2_keep_days,
        }),
        None,
    )
    .await;

    Ok(Json(row))
}

async fn list_service_images(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(service_id): Path<Uuid>,
) -> Result<Json<Vec<RudderImageView>>, ApiError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM services WHERE id = $1)")
        .bind(service_id)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if !exists {
        return Err(not_found("service not found"));
    }
    let rows = sqlx::query_as::<_, RudderImageView>(
        r#"
        SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
               compression, size_bytes, sha256, r2_present, pushed_by, created_at
        FROM rudder_images
        WHERE service_id = $1
        ORDER BY created_at DESC
        "#,
    )
    .bind(service_id)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(rows))
}

#[derive(sqlx::FromRow)]
struct ServiceImageContext {
    id: Uuid,
    name: String,
    kind: String,
    current_image: Option<String>,
    image_auto_deploy: bool,
    image_r2_keep_count: Option<i32>,
    image_r2_keep_days: Option<i32>,
}

fn parse_compression(name: &str) -> Result<(&'static str, Compression), ApiError> {
    match name.trim().to_ascii_lowercase().as_str() {
        "zstd" | "" => Ok(("zstd", Compression::Zstd)),
        "gzip" | "gz" => Ok(("gzip", Compression::Gzip)),
        "none" | "tar" => Ok(("none", Compression::None)),
        _ => Err(bad(
            "invalid_compression",
            "compression must be zstd, gzip, or none",
        )),
    }
}

async fn push_service_image(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(service_id): Path<Uuid>,
    Query(query): Query<PushImageQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<PushedImageResponse>), ApiError> {
    auth.require_write()?;
    let service = sqlx::query_as::<_, ServiceImageContext>(
        r#"
        SELECT id, name, kind, current_image, image_auto_deploy, image_r2_keep_count, image_r2_keep_days
        FROM services
        WHERE id = $1
        "#,
    )
    .bind(service_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("service not found"))?;
    if service.kind != "application" {
        return Err(bad(
            "invalid_kind",
            "Rudder Image push is only available for application services",
        ));
    }

    let settings = load_store_settings(&state.pool).await?;
    let Some(destination_id) = settings.destination_id else {
        return Err(conflict(
            "image_store_unconfigured",
            "configure a Cloudflare R2 / S3 destination in Image Store settings before pushing images",
        ));
    };
    let destination = s3::load_destination(&state, destination_id).await?;

    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));

    enum PayloadSource {
        Bytes(Bytes),
        Stream(Body),
    }

    let (
        tag,
        image_ref_opt,
        git_sha,
        compression_str,
        expected_sha256,
        deploy_opt,
        keep_override,
        payload_source,
    ) = if is_json {
        let bytes = axum::body::to_bytes(body, 256 * 1024 * 1024)
            .await
            .map_err(|err| {
                bad(
                    "invalid_body",
                    format!("could not read request body: {err}"),
                )
            })?;
        let parsed: PushImageRequest = serde_json::from_slice(&bytes)
            .map_err(|err| bad("invalid_json", format!("invalid JSON payload: {err}")))?;
        let raw = match parsed.archive_base64.as_deref() {
            Some(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|err| bad("invalid_base64", format!("invalid archive_base64: {err}")))?,
            None => Vec::new(),
        };
        (
            parsed.tag,
            parsed.image_ref,
            parsed.git_sha,
            parsed.compression.unwrap_or_else(|| "zstd".into()),
            parsed.expected_sha256,
            parsed.deploy,
            parsed.r2_keep_count,
            PayloadSource::Bytes(Bytes::from(raw)),
        )
    } else {
        let tag = query
            .tag
            .clone()
            .ok_or_else(|| bad("missing_tag", "provide ?tag=<tag> when streaming an image"))?;
        (
            tag,
            query.image_ref.clone(),
            query.git_sha.clone(),
            query.compression.clone().unwrap_or_else(|| "zstd".into()),
            query.expected_sha256.clone(),
            query.deploy,
            query.r2_keep_count,
            PayloadSource::Stream(body),
        )
    };

    let tag = tag.trim().to_owned();
    validate_image_tag(&tag).map_err(core_error)?;
    let (compression_name, _) = parse_compression(&compression_str)?;
    let image_ref = image_ref_opt
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("rudder-image/{}:{tag}", service.name));
    validate_image_ref(&image_ref).map_err(core_error)?;
    let object_key = image_object_key(
        &settings.path_prefix,
        &service.id.to_string(),
        &tag,
        compression_name,
    )
    .map_err(core_error)?;

    let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(4);
    let store_clone = destination.store.clone();
    let key_clone = object_key.clone();
    let upload_task = tokio::spawn(async move { s3::upload(store_clone, &key_clone, rx).await });

    let mut hasher = Sha256::new();
    let mut total_bytes: i64 = 0;

    match payload_source {
        PayloadSource::Bytes(bytes) => {
            hasher.update(&bytes);
            total_bytes = bytes.len() as i64;
            let _ = tx.send(Ok(bytes)).await;
        }
        PayloadSource::Stream(stream_body) => {
            let mut stream = stream_body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        hasher.update(&bytes);
                        total_bytes += bytes.len() as i64;
                        if tx.send(Ok(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(format!("stream interrupted: {err}"))).await;
                        break;
                    }
                }
            }
        }
    }
    drop(tx);

    let uploaded_size = upload_task
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_failed",
                format!("upload task failed: {err}"),
            )
        })?
        .map_err(|err| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "r2_upload_failed",
                format!("Cloudflare R2 upload failed: {err}"),
            )
        })?;
    let _ = uploaded_size;
    let sha256 = hex::encode(hasher.finalize());

    if let Some(expected) = expected_sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !sha256.eq_ignore_ascii_case(expected) {
            let _ = s3::delete(destination.store.as_ref(), &object_key).await;
            return Err(bad(
                "checksum_mismatch",
                format!("uploaded archive SHA-256 ({sha256}) did not match expected ({expected})"),
            ));
        }
    }

    let git_sha = git_sha
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(64).collect::<String>());

    let image_row = sqlx::query_as::<_, RudderImageView>(
        r#"
        INSERT INTO rudder_images (
            service_id, destination_id, tag, image_ref, git_sha, object_key,
            compression, size_bytes, sha256, r2_present, pushed_by
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, TRUE, $10)
        ON CONFLICT (service_id, tag) DO UPDATE
        SET destination_id = EXCLUDED.destination_id,
            image_ref = EXCLUDED.image_ref,
            git_sha = EXCLUDED.git_sha,
            object_key = EXCLUDED.object_key,
            compression = EXCLUDED.compression,
            size_bytes = EXCLUDED.size_bytes,
            sha256 = EXCLUDED.sha256,
            r2_present = TRUE,
            pushed_by = EXCLUDED.pushed_by,
            created_at = now()
        RETURNING id, service_id, destination_id, tag, image_ref, git_sha, object_key,
                  compression, size_bytes, sha256, r2_present, pushed_by, created_at
        "#,
    )
    .bind(service.id)
    .bind(Some(destination.id))
    .bind(&tag)
    .bind(&image_ref)
    .bind(&git_sha)
    .bind(&object_key)
    .bind(compression_name)
    .bind(total_bytes)
    .bind(&sha256)
    .bind(Some(auth.id))
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;

    // Apply Cloudflare R2 retention pruning (delete old R2 objects beyond keep_count / keep_days)
    let effective_keep_count = keep_override
        .filter(|v| *v >= 1)
        .or(service.image_r2_keep_count)
        .or(settings.r2_keep_count);
    let effective_keep_days = service.image_r2_keep_days.or(settings.r2_keep_days);

    let all_images = sqlx::query_as::<_, RudderImageView>(
        r#"
        SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
               compression, size_bytes, sha256, r2_present, pushed_by, created_at
        FROM rudder_images
        WHERE service_id = $1
        ORDER BY created_at DESC
        "#,
    )
    .bind(service.id)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;

    let entries: Vec<ImageVersionEntry> = all_images
        .iter()
        .filter(|row| row.r2_present)
        .map(|row| ImageVersionEntry {
            id: row.id.to_string(),
            tag: row.tag.clone(),
            created_at: row.created_at,
        })
        .collect();

    let deployed_tag = service
        .current_image
        .as_deref()
        .and_then(|img| {
            all_images
                .iter()
                .find(|row| row.image_ref == img)
                .map(|row| row.tag.as_str())
                .or_else(|| img.rsplit_once(':').map(|(_, tag)| tag))
        });

    let to_prune = select_images_to_prune_protecting(
        &entries,
        effective_keep_count.and_then(|v| u32::try_from(v).ok()),
        effective_keep_days.and_then(|v| u32::try_from(v).ok()),
        deployed_tag,
        Utc::now(),
    );

    let mut pruned_tags = Vec::new();
    for prune_id_str in to_prune {
        if let Some(target) = all_images.iter().find(|img| {
            img.id.to_string() == prune_id_str
                && img.id != image_row.id
                && service.current_image.as_deref() != Some(img.image_ref.as_str())
        }) {
            let _ = s3::delete(destination.store.as_ref(), &target.object_key).await;
            let _ = sqlx::query("UPDATE rudder_images SET r2_present = FALSE WHERE id = $1")
                .bind(target.id)
                .execute(&state.pool)
                .await;
            pruned_tags.push(target.tag.clone());
        }
    }

    audit::record(
        &state.pool,
        Some(auth.id),
        "service.image_push",
        "service",
        Some(&service.id.to_string()),
        json!({
            "image_id": image_row.id,
            "tag": image_row.tag,
            "image_ref": image_row.image_ref,
            "sha256": image_row.sha256,
            "size_bytes": image_row.size_bytes,
            "pruned_tags": pruned_tags,
        }),
        None,
    )
    .await;

    let should_deploy = deploy_opt.unwrap_or(service.image_auto_deploy);
    let (deployed, deployment, deploy_error) = if should_deploy {
        let trigger = if deploy_opt == Some(true) {
            "cli_deploy"
        } else {
            "auto_deploy"
        };
        match trigger_r2_deploy(&state, &image_row, trigger, Some(auth.id)).await {
            Ok(view) => (true, Some(view), None),
            Err(err) => (false, None, Some(err.public_message())),
        }
    } else {
        events::record(
            &state.pool,
            Event::info(
                "image.pushed",
                format!("New image pushed for {}: {}", service.name, image_row.tag),
            )
            .service(service.id)
            .message(format!(
                "Image {} (tag {}) was uploaded to Cloudflare R2 and is ready to deploy.",
                image_row.image_ref, image_row.tag
            ))
            .payload(json!({
                "image_id": image_row.id,
                "tag": image_row.tag,
                "image_ref": image_row.image_ref,
                "sha256": image_row.sha256,
            })),
        )
        .await;
        (false, None, None)
    };

    Ok((
        StatusCode::CREATED,
        Json(PushedImageResponse {
            image: image_row,
            auto_deploy: service.image_auto_deploy,
            deployed,
            deployment,
            deploy_error,
            pruned_tags,
        }),
    ))
}

async fn trigger_r2_deploy(
    state: &AppState,
    image: &RudderImageView,
    trigger: &str,
    actor: Option<Uuid>,
) -> Result<DeploymentView, ApiError> {
    if !image.r2_present {
        return Err(conflict(
            "image_pruned_from_r2",
            "this image version was pruned from Cloudflare R2 to save storage; re-push it from your local machine with 'rudder push'",
        ));
    }
    let settings = load_store_settings(&state.pool).await?;
    let dest_id = image
        .destination_id
        .or(settings.destination_id)
        .ok_or_else(|| {
            conflict(
                "image_store_unconfigured",
                "no Cloudflare R2 / S3 destination is configured for this image",
            )
        })?;
    let destination = s3::load_destination(state, dest_id).await?;
    let (_, compression) = parse_compression(&image.compression)?;
    let raw_stream = s3::download(destination.store, &image.object_key)
        .await
        .map_err(core_error)?;
    let tar_stream = pipeline::decompress(raw_stream, compression);
    deploy::deploy_r2_stream(
        state,
        image.service_id,
        image.image_ref.clone(),
        tar_stream,
        trigger.to_owned(),
        actor,
    )
    .await
}

async fn deploy_service_image_by_selector(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(service_id): Path<Uuid>,
    Json(body): Json<DeployRudderImageRequest>,
) -> Result<(StatusCode, Json<DeploymentView>), ApiError> {
    auth.require_write()?;
    let row = if let Some(image_id) = body.image_id {
        sqlx::query_as::<_, RudderImageView>(
            r#"
            SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
                   compression, size_bytes, sha256, r2_present, pushed_by, created_at
            FROM rudder_images
            WHERE id = $1 AND service_id = $2
            "#,
        )
        .bind(image_id)
        .bind(service_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?
    } else if let Some(tag) = body.tag.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query_as::<_, RudderImageView>(
            r#"
            SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
                   compression, size_bytes, sha256, r2_present, pushed_by, created_at
            FROM rudder_images
            WHERE service_id = $1 AND tag = $2
            "#,
        )
        .bind(service_id)
        .bind(tag)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?
    } else {
        sqlx::query_as::<_, RudderImageView>(
            r#"
            SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
                   compression, size_bytes, sha256, r2_present, pushed_by, created_at
            FROM rudder_images
            WHERE service_id = $1 AND r2_present = TRUE
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(service_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?
    };

    let Some(image) = row else {
        return Err(not_found(
            "no matching Rudder Image found in Cloudflare R2 for this service",
        ));
    };

    let view = trigger_r2_deploy(&state, &image, "r2_deploy", Some(auth.id)).await?;
    Ok((StatusCode::ACCEPTED, Json(view)))
}

async fn deploy_service_image_by_id(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((service_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<(StatusCode, Json<DeploymentView>), ApiError> {
    auth.require_write()?;
    let image = sqlx::query_as::<_, RudderImageView>(
        r#"
        SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
               compression, size_bytes, sha256, r2_present, pushed_by, created_at
        FROM rudder_images
        WHERE id = $1 AND service_id = $2
        "#,
    )
    .bind(image_id)
    .bind(service_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("Rudder Image not found"))?;

    let view = trigger_r2_deploy(&state, &image, "r2_deploy", Some(auth.id)).await?;
    Ok((StatusCode::ACCEPTED, Json(view)))
}

async fn delete_service_image(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((service_id, image_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<DeleteImageQuery>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    let image = sqlx::query_as::<_, RudderImageView>(
        r#"
        SELECT id, service_id, destination_id, tag, image_ref, git_sha, object_key,
               compression, size_bytes, sha256, r2_present, pushed_by, created_at
        FROM rudder_images
        WHERE id = $1 AND service_id = $2
        "#,
    )
    .bind(image_id)
    .bind(service_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("Rudder Image not found"))?;

    let current_image: Option<String> =
        sqlx::query_scalar("SELECT current_image FROM services WHERE id = $1")
            .bind(service_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(ApiError::database)?
            .flatten();
    if current_image.as_deref() == Some(image.image_ref.as_str()) {
        return Err(conflict(
            "image_in_use",
            "cannot delete the image version currently deployed on this service",
        ));
    }

    if image.r2_present {
        let settings = load_store_settings(&state.pool).await?;
        if let Some(dest_id) = image.destination_id.or(settings.destination_id) {
            if let Ok(dest) = s3::load_destination(&state, dest_id).await {
                let _ = s3::delete(dest.store.as_ref(), &image.object_key).await;
            }
        }
    }

    if query.r2_only.unwrap_or(false) {
        sqlx::query("UPDATE rudder_images SET r2_present = FALSE WHERE id = $1")
            .bind(image.id)
            .execute(&state.pool)
            .await
            .map_err(ApiError::database)?;
    } else {
        sqlx::query("DELETE FROM rudder_images WHERE id = $1")
            .bind(image.id)
            .execute(&state.pool)
            .await
            .map_err(ApiError::database)?;
    }

    audit::record(
        &state.pool,
        Some(auth.id),
        "service.image_delete",
        "service",
        Some(&service_id.to_string()),
        json!({ "image_id": image.id, "tag": image.tag, "r2_only": query.r2_only }),
        None,
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}
