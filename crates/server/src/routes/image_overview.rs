//! The Rudder Image dashboard: setup status, push and build statistics, connected
//! command-line clients, and a live check of the Cloudflare R2 / S3 bucket.

use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::s3;
use crate::state::AppState;

use super::backups::TestResult;
use super::catalog::conflict;
use super::images::ImageStoreSettingsView;

/// How many days of daily history the dashboard charts.
const HISTORY_DAYS: i32 = 30;

#[derive(Serialize, ToSchema)]
pub struct ImageSetupStep {
    /// `bucket`, `retention`, `client`, `first_push`, or `auto_deploy`.
    pub key: String,
    /// `ok`, `todo`, or `optional` (not done, but nothing is broken).
    pub state: String,
    pub detail: String,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ImageStoreDestination {
    pub id: Uuid,
    pub name: String,
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: String,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ImageTotals {
    /// Versions on record, on R2 or already pruned.
    pub images: i64,
    /// Versions that can still be deployed from the bucket.
    pub r2_images: i64,
    /// Versions pruned from the bucket by retention.
    pub pruned_images: i64,
    /// Bytes the bucket holds for Rudder Images.
    pub r2_bytes: i64,
    pub services: i64,
    pub pushes_24h: i64,
    pub pushes_7d: i64,
    pub pushes_30d: i64,
    pub pushed_bytes_30d: i64,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct BuildTotals {
    /// Builds the panel ran from Git in the last 30 days.
    pub builds_30d: i64,
    pub succeeded_30d: i64,
    pub failed_30d: i64,
    pub running: i64,
    pub queued: i64,
    /// Deployments rolled out from a Rudder Image in the last 30 days.
    pub image_deploys_30d: i64,
    pub image_deploy_failures_30d: i64,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ImageDay {
    pub day: NaiveDate,
    pub pushes: i64,
    pub bytes: i64,
    pub builds: i64,
    pub failed_builds: i64,
    pub image_deploys: i64,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ImageServiceStat {
    pub service_id: Uuid,
    /// `project/environment/service`.
    pub path: String,
    pub images: i64,
    pub r2_images: i64,
    pub r2_bytes: i64,
    pub auto_deploy: bool,
    pub last_tag: Option<String>,
    pub last_push_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct RecentImagePush {
    pub id: Uuid,
    pub service_id: Uuid,
    pub path: String,
    pub tag: String,
    pub git_sha: Option<String>,
    pub size_bytes: i64,
    pub compression: String,
    pub r2_present: bool,
    pub pushed_by: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ImageClient {
    pub id: Uuid,
    pub name: String,
    /// `browser` or `device_code`.
    pub auth_method: String,
    pub user_email: String,
    pub scopes: Vec<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct ImageStoreOverview {
    pub settings: ImageStoreSettingsView,
    pub destination: Option<ImageStoreDestination>,
    pub setup: Vec<ImageSetupStep>,
    pub totals: ImageTotals,
    pub builds: BuildTotals,
    pub daily: Vec<ImageDay>,
    pub services: Vec<ImageServiceStat>,
    pub recent_pushes: Vec<RecentImagePush>,
    pub clients: Vec<ImageClient>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/v1/image-store/overview", get(overview))
        .route("/api/v1/image-store/test", post(test_store))
}

async fn overview(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<ImageStoreOverview>, ApiError> {
    let pool = &state.pool;
    let settings = super::images::load_store_settings(pool).await?;

    let destination = match settings.destination_id {
        Some(id) => sqlx::query_as::<_, ImageStoreDestination>(
            "SELECT id, name, bucket, endpoint, region FROM s3_destinations WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(ApiError::database)?,
        None => None,
    };

    let totals = sqlx::query_as::<_, ImageTotals>(
        r#"
        SELECT COUNT(*)::bigint AS images,
               COUNT(*) FILTER (WHERE r2_present)::bigint AS r2_images,
               COUNT(*) FILTER (WHERE NOT r2_present)::bigint AS pruned_images,
               COALESCE(SUM(size_bytes) FILTER (WHERE r2_present), 0)::bigint AS r2_bytes,
               COUNT(DISTINCT service_id)::bigint AS services,
               COUNT(*) FILTER (WHERE created_at > now() - interval '24 hours')::bigint AS pushes_24h,
               COUNT(*) FILTER (WHERE created_at > now() - interval '7 days')::bigint AS pushes_7d,
               COUNT(*) FILTER (WHERE created_at > now() - interval '30 days')::bigint AS pushes_30d,
               COALESCE(SUM(size_bytes) FILTER (WHERE created_at > now() - interval '30 days'), 0)::bigint
                   AS pushed_bytes_30d
        FROM rudder_images
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)?;

    let builds = sqlx::query_as::<_, BuildTotals>(
        r#"
        SELECT
          (SELECT COUNT(*) FROM build_queue WHERE created_at > now() - interval '30 days')::bigint
              AS builds_30d,
          (SELECT COUNT(*) FROM build_queue
            WHERE created_at > now() - interval '30 days' AND status = 'succeeded')::bigint
              AS succeeded_30d,
          (SELECT COUNT(*) FROM build_queue
            WHERE created_at > now() - interval '30 days' AND status IN ('failed', 'cancelled'))::bigint
              AS failed_30d,
          (SELECT COUNT(*) FROM build_queue WHERE status = 'running')::bigint AS running,
          (SELECT COUNT(*) FROM build_queue WHERE status = 'queued')::bigint AS queued,
          (SELECT COUNT(*) FROM deployments
            WHERE trigger IN ('r2_deploy', 'cli_deploy', 'auto_deploy') AND created_at > now() - interval '30 days')::bigint
              AS image_deploys_30d,
          (SELECT COUNT(*) FROM deployments
            WHERE trigger IN ('r2_deploy', 'cli_deploy', 'auto_deploy') AND status = 'failed'
              AND created_at > now() - interval '30 days')::bigint AS image_deploy_failures_30d
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)?;

    let daily = sqlx::query_as::<_, ImageDay>(
        r#"
        SELECT d.day::date AS day,
               COALESCE(p.pushes, 0)::bigint AS pushes,
               COALESCE(p.bytes, 0)::bigint AS bytes,
               COALESCE(b.builds, 0)::bigint AS builds,
               COALESCE(b.failed, 0)::bigint AS failed_builds,
               COALESCE(r.deploys, 0)::bigint AS image_deploys
        FROM generate_series(
                 (now()::date - ($1::int - 1)), now()::date, interval '1 day') AS d(day)
        LEFT JOIN (
            SELECT created_at::date AS day, COUNT(*) AS pushes, SUM(size_bytes) AS bytes
            FROM rudder_images GROUP BY 1
        ) p ON p.day = d.day::date
        LEFT JOIN (
            SELECT created_at::date AS day, COUNT(*) AS builds,
                   COUNT(*) FILTER (WHERE status IN ('failed', 'cancelled')) AS failed
            FROM build_queue GROUP BY 1
        ) b ON b.day = d.day::date
        LEFT JOIN (
            SELECT created_at::date AS day, COUNT(*) AS deploys
            FROM deployments WHERE trigger IN ('r2_deploy', 'cli_deploy', 'auto_deploy') GROUP BY 1
        ) r ON r.day = d.day::date
        ORDER BY d.day
        "#,
    )
    .bind(HISTORY_DAYS)
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;

    let services = sqlx::query_as::<_, ImageServiceStat>(
        r#"
        SELECT s.id AS service_id,
               p.name || '/' || e.name || '/' || s.name AS path,
               COUNT(i.id)::bigint AS images,
               COUNT(i.id) FILTER (WHERE i.r2_present)::bigint AS r2_images,
               COALESCE(SUM(i.size_bytes) FILTER (WHERE i.r2_present), 0)::bigint AS r2_bytes,
               s.image_auto_deploy AS auto_deploy,
               (ARRAY_AGG(i.tag ORDER BY i.created_at DESC))[1] AS last_tag,
               MAX(i.created_at) AS last_push_at
        FROM services s
        JOIN environments e ON e.id = s.environment_id
        JOIN projects p ON p.id = e.project_id
        LEFT JOIN rudder_images i ON i.service_id = s.id
        WHERE s.kind = 'application'
        GROUP BY s.id, p.name, e.name, s.name, s.image_auto_deploy
        HAVING COUNT(i.id) > 0 OR s.image_auto_deploy
        ORDER BY MAX(i.created_at) DESC NULLS LAST, path
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;

    let recent_pushes = sqlx::query_as::<_, RecentImagePush>(
        r#"
        SELECT i.id, i.service_id,
               p.name || '/' || e.name || '/' || s.name AS path,
               i.tag, i.git_sha, i.size_bytes, i.compression, i.r2_present,
               u.email AS pushed_by, i.created_at
        FROM rudder_images i
        JOIN services s ON s.id = i.service_id
        JOIN environments e ON e.id = s.environment_id
        JOIN projects p ON p.id = e.project_id
        LEFT JOIN users u ON u.id = i.pushed_by
        ORDER BY i.created_at DESC
        LIMIT 12
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;

    let clients = sqlx::query_as::<_, ImageClient>(
        r#"
        SELECT t.id, t.name, t.auth_method, u.email AS user_email, t.scopes,
               t.last_used_at, t.expires_at, t.created_at
        FROM api_tokens t
        JOIN users u ON u.id = t.user_id
        WHERE t.auth_method IN ('browser', 'device_code')
          AND (t.expires_at IS NULL OR t.expires_at > now())
        ORDER BY t.last_used_at DESC NULLS LAST, t.created_at DESC
        LIMIT 20
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;

    let auto_deploy_services = services.iter().filter(|s| s.auto_deploy).count();
    let setup = vec![
        match &destination {
            Some(d) => ImageSetupStep {
                key: "bucket".into(),
                state: "ok".into(),
                detail: format!("{} ({})", d.name, d.bucket),
            },
            None => ImageSetupStep {
                key: "bucket".into(),
                state: "todo".into(),
                detail: "Choose the Cloudflare R2 / S3 bucket that stores the images.".into(),
            },
        },
        if settings.r2_keep_count.is_some() || settings.r2_keep_days.is_some() {
            ImageSetupStep {
                key: "retention".into(),
                state: "ok".into(),
                detail: "Old versions are pruned from the bucket.".into(),
            }
        } else {
            ImageSetupStep {
                key: "retention".into(),
                state: "optional".into(),
                detail: "Every version stays in the bucket until you delete it.".into(),
            }
        },
        if clients.is_empty() {
            ImageSetupStep {
                key: "client".into(),
                state: "todo".into(),
                detail: "No command-line client has signed in yet (rudder login).".into(),
            }
        } else {
            ImageSetupStep {
                key: "client".into(),
                state: "ok".into(),
                detail: format!("{} signed-in client(s).", clients.len()),
            }
        },
        if totals.images > 0 {
            ImageSetupStep {
                key: "first_push".into(),
                state: "ok".into(),
                detail: format!("{} image version(s) on record.", totals.images),
            }
        } else {
            ImageSetupStep {
                key: "first_push".into(),
                state: "todo".into(),
                detail: "Nothing pushed yet (rudder build, then rudder push).".into(),
            }
        },
        if auto_deploy_services > 0 {
            ImageSetupStep {
                key: "auto_deploy".into(),
                state: "ok".into(),
                detail: format!("{auto_deploy_services} service(s) deploy on push."),
            }
        } else {
            ImageSetupStep {
                key: "auto_deploy".into(),
                state: "optional".into(),
                detail: "Pushes only notify; deploy by hand or turn auto-deploy on per service."
                    .into(),
            }
        },
    ];

    Ok(Json(ImageStoreOverview {
        settings,
        destination,
        setup,
        totals,
        builds,
        daily,
        services,
        recent_pushes,
        clients,
    }))
}

/// Write, read, and delete a probe object in the image bucket.
async fn test_store(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<TestResult>, ApiError> {
    auth.require_write()?;
    let settings = super::images::load_store_settings(&state.pool).await?;
    let Some(destination_id) = settings.destination_id else {
        return Err(conflict(
            "image_store_unconfigured",
            "no Cloudflare R2 / S3 destination is configured for Rudder Images",
        ));
    };
    let destination = s3::load_destination(&state, destination_id).await?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        s3::test_connection(destination.store.as_ref()),
    )
    .await
    .unwrap_or_else(|_| Err("timed out after 30 seconds".into()));
    Ok(Json(match result {
        Ok(()) => TestResult {
            ok: true,
            message: "write, read, and delete succeeded".into(),
        },
        Err(message) => TestResult { ok: false, message },
    }))
}
