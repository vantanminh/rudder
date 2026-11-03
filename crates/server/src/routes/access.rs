//! Rudder Access: a separate sign-in for internal apps.
//!
//! Panel administrators and Access administrators manage people, departments,
//! and ranks. Visitors sign in with Google or an email code, after Turnstile.
//! Traefik asks the gate before it forwards a request to a protected app.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar};
use chrono::{DateTime, Duration, Utc};
use hmac::KeyInit;
use hmac::{Hmac, Mac};
use rudder_core::{
    accept_turnstile, access_allowed, department_closure, parse_turnstile, safe_return_path,
    validate_host, AccessRule, Membership, ACCESS_LOGIN_ACTION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::audit;
use crate::auth::{self, AuthUser};
use crate::error::ApiError;
use crate::mailer::{send_login_code, MailConfig};
use crate::routes::catalog::{bad, conflict, is_unique, not_found};
use crate::state::AppState;

const ACCESS_COOKIE: &str = "rudder_access";
const SESSION_HOURS: i64 = 12;
const CODE_MINUTES: i64 = 10;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/v1/access/public", get(public_config))
        .route(
            "/api/v1/access/settings",
            get(get_settings).put(put_settings),
        )
        .route("/api/v1/access/settings/test-email", post(test_email))
        .route(
            "/api/v1/access/departments",
            get(list_departments).post(create_department),
        )
        .route(
            "/api/v1/access/departments/{id}",
            axum::routing::patch(update_department).delete(delete_department),
        )
        .route("/api/v1/access/ranks", get(list_ranks).post(create_rank))
        .route(
            "/api/v1/access/ranks/{id}",
            axum::routing::patch(update_rank).delete(delete_rank),
        )
        .route(
            "/api/v1/access/identities",
            get(list_identities).post(create_identity),
        )
        .route(
            "/api/v1/access/identities/{id}",
            axum::routing::patch(update_identity).delete(delete_identity),
        )
        .route(
            "/api/v1/access/identities/{id}/memberships",
            get(list_memberships).put(put_memberships),
        )
        .route(
            "/api/v1/access/services/{id}",
            get(get_policy).put(put_policy),
        )
        .route("/api/v1/access/services/{id}/token", post(rotate_token))
        .route("/api/v1/access/login/email", post(email_start))
        .route("/api/v1/access/login/email/verify", post(email_verify))
        .route("/api/v1/access/login/google", post(google_start))
        .route("/api/v1/access/login/google/callback", get(google_callback))
        .route("/api/v1/access/logout", post(logout))
        .route("/api/v1/access/me", get(me))
        .route("/api/v1/access/apps", get(apps))
        .route("/api/v1/access/callback", get(handoff_callback))
        .route("/api/v1/access/introspect", post(introspect))
        .route("/api/v1/edge/gate", get(edge_gate))
}

#[derive(Serialize, ToSchema)]
pub struct AccessPublic {
    pub site_key: String,
    pub ready: bool,
    pub google: bool,
    pub email: bool,
    pub portal_host: Option<String>,
    pub google_redirect_uri: String,
}

#[derive(Serialize, ToSchema)]
pub struct AccessSettingsView {
    pub portal_host: Option<String>,
    pub turnstile_site_key: String,
    pub has_turnstile_secret: bool,
    pub turnstile_hostnames: Vec<String>,
    pub google_client_id: String,
    pub has_google_secret: bool,
    pub email_provider: String,
    pub email_from: String,
    pub email_ready: bool,
    pub google_redirect_uri: String,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateAccessSettings {
    pub portal_host: Option<String>,
    pub clear_portal_host: Option<bool>,
    pub turnstile_site_key: Option<String>,
    pub turnstile_secret: Option<String>,
    pub turnstile_hostnames: Option<Vec<String>>,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub email_provider: Option<String>,
    pub email_from: Option<String>,
    pub resend_api_key: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<String>,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub cloudflare_account_id: Option<String>,
    pub cloudflare_api_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessDepartment {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub parent_id: Option<Uuid>,
    pub description: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SaveDepartment {
    pub name: String,
    pub slug: String,
    pub parent_id: Option<Uuid>,
    pub description: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessRank {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub level: i32,
    pub description: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SaveRank {
    pub name: String,
    pub slug: String,
    pub level: i32,
    pub description: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessIdentity {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub status: String,
    pub admin: bool,
    pub owner: bool,
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, ToSchema)]
pub struct SaveIdentity {
    pub email: String,
    pub display_name: Option<String>,
    pub status: Option<String>,
    pub admin: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessMembership {
    pub department_id: Uuid,
    pub department_slug: String,
    pub rank_id: Option<Uuid>,
    pub rank_slug: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct MembershipInput {
    pub department_id: Uuid,
    pub rank_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct SaveMemberships {
    pub memberships: Vec<MembershipInput>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessRuleView {
    pub kind: String,
    pub email: Option<String>,
    pub department_id: Option<Uuid>,
    pub rank_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessPolicyView {
    pub enabled: bool,
    pub rules: Vec<AccessRuleView>,
    pub token_configured: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct RuleInput {
    pub kind: String,
    pub email: Option<String>,
    pub department_id: Option<Uuid>,
    pub rank_id: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct SaveAccessPolicy {
    pub enabled: bool,
    pub rules: Vec<RuleInput>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessTokenCreated {
    pub token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AccessEmailStart {
    pub email: String,
    pub turnstile_token: String,
    pub return_host: Option<String>,
    pub return_path: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct AccessEmailVerify {
    pub email: String,
    pub code: String,
    pub turnstile_token: String,
    pub return_host: Option<String>,
    pub return_path: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessLoginResult {
    pub redirect: Option<String>,
    pub csrf_token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AccessGoogleStart {
    pub turnstile_token: String,
    pub return_host: Option<String>,
    pub return_path: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessGoogleUrl {
    pub url: String,
}

#[derive(Serialize, ToSchema)]
pub struct AccessMe {
    pub email: String,
    pub display_name: String,
    pub admin: bool,
    pub owner: bool,
    pub csrf_token: String,
}

#[derive(Serialize, ToSchema)]
pub struct AccessApp {
    pub service_id: Uuid,
    pub name: String,
    pub host: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct AccessIntrospectRequest {
    pub session: String,
}

#[derive(Serialize, ToSchema)]
pub struct AccessDepartmentGrant {
    pub slug: String,
    pub name: String,
    pub rank: Option<String>,
    pub level: Option<i32>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessIntrospect {
    pub email: String,
    pub display_name: String,
    pub departments: Vec<AccessDepartmentGrant>,
    pub expires_at: DateTime<Utc>,
}

struct Manager {
    email: String,
    panel_owner: bool,
}

struct SettingsRow {
    portal_host: Option<String>,
    turnstile_site_key: String,
    turnstile_secret_enc: Option<Vec<u8>>,
    turnstile_hostnames: Vec<String>,
    google_client_id: String,
    google_client_secret_enc: Option<Vec<u8>>,
    email_provider: String,
    email_from: String,
    email_config_enc: Option<Vec<u8>>,
}

impl FromRequestParts<AppState> for Manager {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let has_panel = parts.headers.contains_key("authorization")
            || cookie_value(&parts.headers, auth::SESSION_COOKIE).is_some();
        let panel_error = if has_panel {
            match AuthUser::from_request_parts(parts, state).await {
                Ok(user) if user.can_write() => {
                    return Ok(Manager {
                        email: user.email,
                        panel_owner: user.is_owner,
                    });
                }
                Ok(_) => None,
                Err(error) => Some(error),
            }
        } else {
            None
        };
        match access_admin(state, parts).await {
            Ok(manager) => Ok(manager),
            Err(error) => Err(panel_error.unwrap_or(error)),
        }
    }
}

async fn public_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AccessPublic>, ApiError> {
    let settings = load_settings(&state).await?;
    let origin = portal_origin(&state, &settings, &headers).await?;
    Ok(Json(AccessPublic {
        site_key: settings.turnstile_site_key.clone(),
        ready: turnstile_ready(&settings),
        google: google_ready(&settings),
        email: email_ready(&settings),
        portal_host: settings.portal_host.clone(),
        google_redirect_uri: redirect_uri(&origin),
    }))
}

async fn get_settings(
    State(state): State<AppState>,
    _manager: Manager,
    headers: HeaderMap,
) -> Result<Json<AccessSettingsView>, ApiError> {
    let settings = load_settings(&state).await?;
    let origin = portal_origin(&state, &settings, &headers).await?;
    Ok(Json(settings_view(&settings, &origin)))
}

async fn put_settings(
    State(state): State<AppState>,
    manager: Manager,
    headers: HeaderMap,
    Json(body): Json<UpdateAccessSettings>,
) -> Result<Json<AccessSettingsView>, ApiError> {
    let current = load_settings(&state).await?;
    let portal_host = if body.clear_portal_host.unwrap_or(false) {
        None
    } else if let Some(host) = body.portal_host.as_deref() {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            None
        } else {
            validate_host(&host).map_err(|_| bad("invalid", "portal host is not a hostname"))?;
            Some(host)
        }
    } else {
        current.portal_host.clone()
    };
    if let Some(host) = &portal_host {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM domains WHERE host = $1) AS "taken!""#,
            host
        )
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::database)?;
        if taken {
            return Err(conflict(
                "host_taken",
                "that hostname is already an app domain",
            ));
        }
    }
    let site_key = body
        .turnstile_site_key
        .clone()
        .unwrap_or(current.turnstile_site_key.clone());
    if site_key.len() > 200
        || site_key
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(bad("invalid", "Turnstile site key is not valid"));
    }
    let secret = match body.turnstile_secret.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => Some(encrypt(&state, value.as_bytes())?),
        _ => current.turnstile_secret_enc.clone(),
    };
    let hostnames = if let Some(hostnames) = body.turnstile_hostnames.clone() {
        let mut clean = Vec::new();
        for host in hostnames {
            let host = host.trim().to_ascii_lowercase();
            validate_host(&host)
                .map_err(|_| bad("invalid", "a Turnstile hostname is not valid"))?;
            if !clean.contains(&host) {
                clean.push(host);
            }
        }
        if clean.len() > 20 {
            return Err(bad("invalid", "at most 20 Turnstile hostnames"));
        }
        clean
    } else {
        current.turnstile_hostnames.clone()
    };
    let google_id = body
        .google_client_id
        .clone()
        .unwrap_or(current.google_client_id.clone());
    if google_id.len() > 200
        || google_id
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(bad("invalid", "Google client id is not valid"));
    }
    let google_secret = match body.google_client_secret.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => Some(encrypt(&state, value.as_bytes())?),
        _ => current.google_client_secret_enc.clone(),
    };
    let provider = body
        .email_provider
        .clone()
        .unwrap_or(current.email_provider.clone());
    if !matches!(provider.as_str(), "none" | "resend" | "smtp" | "cloudflare") {
        return Err(bad(
            "invalid",
            "email provider must be none, resend, smtp, or cloudflare",
        ));
    }
    let email_from = body
        .email_from
        .clone()
        .unwrap_or(current.email_from.clone());
    if email_from.len() > 200 {
        return Err(bad("invalid", "From address is too long"));
    }
    let email_config = merged_mail(&state, &current, &body, &provider)?;
    sqlx::query!(
        r#"
        UPDATE access_settings
        SET portal_host = $1, turnstile_site_key = $2, turnstile_secret_enc = $3,
            turnstile_hostnames = $4, google_client_id = $5, google_client_secret_enc = $6,
            email_provider = $7, email_from = $8, email_config_enc = $9, updated_at = now()
        WHERE id = TRUE
        "#,
        portal_host,
        site_key,
        secret,
        &hostnames,
        google_id,
        google_secret,
        provider,
        email_from,
        email_config,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    audit::record(
        &state.pool,
        None,
        "access.settings",
        "access",
        None,
        json!({ "actor": manager.email, "provider": provider }),
        None,
    )
    .await;
    let _ = crate::publish::publish_local(&state).await;
    let settings = load_settings(&state).await?;
    let origin = portal_origin(&state, &settings, &headers).await?;
    Ok(Json(settings_view(&settings, &origin)))
}

async fn test_email(
    State(state): State<AppState>,
    manager: Manager,
) -> Result<StatusCode, ApiError> {
    let settings = load_settings(&state).await?;
    let config = mail_config(&state, &settings)?;
    crate::mailer::send_text(
        http(),
        &config,
        &settings.email_from,
        &manager.email,
        "Rudder Access email",
        "Rudder Access can send mail from this panel.",
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_departments(
    State(state): State<AppState>,
    _manager: Manager,
) -> Result<Json<Vec<AccessDepartment>>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT id, name, slug, parent_id, description
        FROM access_departments
        ORDER BY name
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| AccessDepartment {
                id: row.id,
                name: row.name,
                slug: row.slug,
                parent_id: row.parent_id,
                description: row.description,
            })
            .collect(),
    ))
}

async fn create_department(
    State(state): State<AppState>,
    manager: Manager,
    Json(body): Json<SaveDepartment>,
) -> Result<(StatusCode, Json<AccessDepartment>), ApiError> {
    let (name, slug, description) = clean_org(&body.name, &body.slug, body.description.as_deref())?;
    if let Some(parent) = body.parent_id {
        department_exists(&state, parent).await?;
    }
    let inserted = sqlx::query!(
        r#"
        INSERT INTO access_departments (name, slug, parent_id, description)
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
        name,
        slug,
        body.parent_id,
        description,
    )
    .fetch_one(&state.pool)
    .await;
    let id = match inserted {
        Ok(row) => row.id,
        Err(error) if is_unique(&error) => {
            return Err(conflict("slug_taken", "that slug is in use"))
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    audit::record(
        &state.pool,
        None,
        "access.department",
        "access_department",
        Some(&id.to_string()),
        json!({ "actor": manager.email, "slug": slug }),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(AccessDepartment {
            id,
            name,
            slug,
            parent_id: body.parent_id,
            description,
        }),
    ))
}

async fn update_department(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveDepartment>,
) -> Result<Json<AccessDepartment>, ApiError> {
    let (name, slug, description) = clean_org(&body.name, &body.slug, body.description.as_deref())?;
    if body.parent_id == Some(id) || parent_cycles(&state, id, body.parent_id).await? {
        return Err(bad("invalid", "a department cannot contain itself"));
    }
    let updated = sqlx::query!(
        r#"
        UPDATE access_departments
        SET name = $2, slug = $3, parent_id = $4, description = $5
        WHERE id = $1
        "#,
        id,
        name,
        slug,
        body.parent_id,
        description,
    )
    .execute(&state.pool)
    .await;
    match updated {
        Ok(done) if done.rows_affected() == 0 => return Err(not_found("department not found")),
        Ok(_) => {}
        Err(error) if is_unique(&error) => {
            return Err(conflict("slug_taken", "that slug is in use"))
        }
        Err(error) => return Err(ApiError::database(error)),
    }
    Ok(Json(AccessDepartment {
        id,
        name,
        slug,
        parent_id: body.parent_id,
        description,
    }))
}

async fn delete_department(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query!(r#"DELETE FROM access_departments WHERE id = $1"#, id)
        .execute(&state.pool)
        .await;
    match deleted {
        Ok(done) if done.rows_affected() == 0 => Err(not_found("department not found")),
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(error) if is_fk(&error) => Err(conflict(
            "in_use",
            "remove child departments and memberships first",
        )),
        Err(error) => Err(ApiError::database(error)),
    }
}

async fn list_ranks(
    State(state): State<AppState>,
    _manager: Manager,
) -> Result<Json<Vec<AccessRank>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT id, name, slug, level, description FROM access_ranks ORDER BY level, name"#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| AccessRank {
                id: row.id,
                name: row.name,
                slug: row.slug,
                level: row.level,
                description: row.description,
            })
            .collect(),
    ))
}

async fn create_rank(
    State(state): State<AppState>,
    _manager: Manager,
    Json(body): Json<SaveRank>,
) -> Result<(StatusCode, Json<AccessRank>), ApiError> {
    let rank = clean_rank(&body)?;
    let inserted = sqlx::query!(
        r#"
        INSERT INTO access_ranks (name, slug, level, description)
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
        rank.0,
        rank.1,
        rank.2,
        rank.3,
    )
    .fetch_one(&state.pool)
    .await;
    let id = match inserted {
        Ok(row) => row.id,
        Err(error) if is_unique(&error) => {
            return Err(conflict("slug_taken", "that slug is in use"))
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    Ok((
        StatusCode::CREATED,
        Json(AccessRank {
            id,
            name: rank.0,
            slug: rank.1,
            level: rank.2,
            description: rank.3,
        }),
    ))
}

async fn update_rank(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveRank>,
) -> Result<Json<AccessRank>, ApiError> {
    let rank = clean_rank(&body)?;
    let updated = sqlx::query!(
        r#"UPDATE access_ranks SET name = $2, slug = $3, level = $4, description = $5 WHERE id = $1"#,
        id,
        rank.0,
        rank.1,
        rank.2,
        rank.3,
    )
    .execute(&state.pool)
    .await;
    match updated {
        Ok(done) if done.rows_affected() == 0 => return Err(not_found("rank not found")),
        Ok(_) => {}
        Err(error) if is_unique(&error) => {
            return Err(conflict("slug_taken", "that slug is in use"))
        }
        Err(error) => return Err(ApiError::database(error)),
    }
    Ok(Json(AccessRank {
        id,
        name: rank.0,
        slug: rank.1,
        level: rank.2,
        description: rank.3,
    }))
}

async fn delete_rank(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let deleted = sqlx::query!(r#"DELETE FROM access_ranks WHERE id = $1"#, id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if deleted.rows_affected() == 0 {
        return Err(not_found("rank not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_identities(
    State(state): State<AppState>,
    _manager: Manager,
) -> Result<Json<Vec<AccessIdentity>>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT i.id, i.email, i.display_name, i.status, i.last_login_at, a.is_owner AS "is_owner?"
        FROM access_identities i
        LEFT JOIN access_admins a ON a.identity_id = i.id
        ORDER BY i.email
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| AccessIdentity {
                id: row.id,
                email: row.email,
                display_name: row.display_name,
                status: row.status,
                admin: row.is_owner.is_some(),
                owner: row.is_owner.unwrap_or(false),
                last_login_at: row.last_login_at,
            })
            .collect(),
    ))
}

async fn create_identity(
    State(state): State<AppState>,
    manager: Manager,
    Json(body): Json<SaveIdentity>,
) -> Result<(StatusCode, Json<AccessIdentity>), ApiError> {
    let email = auth::normalize_email(&body.email)?;
    let name = display(&body.display_name, &email);
    let status = clean_status(body.status.as_deref())?;
    let inserted = sqlx::query!(
        r#"
        INSERT INTO access_identities (email, display_name, status)
        VALUES ($1, $2, $3)
        RETURNING id
        "#,
        email,
        name,
        status,
    )
    .fetch_one(&state.pool)
    .await;
    let id = match inserted {
        Ok(row) => row.id,
        Err(error) if is_unique(&error) => {
            return Err(conflict("email_taken", "that email is already allowed"))
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    let owner = if body.admin.unwrap_or(false) {
        set_admin(&state, id, true, manager.panel_owner).await?
    } else {
        false
    };
    audit::record(
        &state.pool,
        None,
        "access.identity",
        "access_identity",
        Some(&id.to_string()),
        json!({ "actor": manager.email }),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(AccessIdentity {
            id,
            email,
            display_name: name,
            status,
            admin: body.admin.unwrap_or(false),
            owner,
            last_login_at: None,
        }),
    ))
}

async fn update_identity(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveIdentity>,
) -> Result<Json<AccessIdentity>, ApiError> {
    let email = auth::normalize_email(&body.email)?;
    let name = display(&body.display_name, &email);
    let status = clean_status(body.status.as_deref())?;
    let current_owner = is_access_owner(&state, id).await?;
    if current_owner && status == "blocked" && !manager.panel_owner {
        return Err(conflict(
            "owner_protected",
            "the original Access administrator cannot be blocked by another administrator",
        ));
    }
    let updated = sqlx::query!(
        r#"
        UPDATE access_identities
        SET email = $2, display_name = $3, status = $4
        WHERE id = $1
        RETURNING last_login_at
        "#,
        id,
        email,
        name,
        status,
    )
    .fetch_optional(&state.pool)
    .await;
    let last_login_at = match updated {
        Ok(Some(row)) => row.last_login_at,
        Ok(None) => return Err(not_found("person not found")),
        Err(error) if is_unique(&error) => {
            return Err(conflict("email_taken", "that email is already allowed"))
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    let owner = if let Some(admin) = body.admin {
        set_admin(&state, id, admin, manager.panel_owner).await?
    } else {
        current_owner
    };
    let admin = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM access_admins WHERE identity_id = $1) AS "admin!""#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(AccessIdentity {
        id,
        email,
        display_name: name,
        status,
        admin,
        owner,
        last_login_at,
    }))
}

async fn delete_identity(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if is_access_owner(&state, id).await? && !manager.panel_owner {
        return Err(conflict(
            "owner_protected",
            "the original Access administrator cannot be removed by another administrator",
        ));
    }
    let deleted = sqlx::query!(r#"DELETE FROM access_identities WHERE id = $1"#, id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if deleted.rows_affected() == 0 {
        return Err(not_found("person not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_memberships(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<AccessMembership>>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT m.department_id, d.slug AS department_slug, m.rank_id, r.slug AS "rank_slug?"
        FROM access_memberships m
        JOIN access_departments d ON d.id = m.department_id
        LEFT JOIN access_ranks r ON r.id = m.rank_id
        WHERE m.identity_id = $1
        ORDER BY d.slug
        "#,
        id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| AccessMembership {
                department_id: row.department_id,
                department_slug: row.department_slug,
                rank_id: row.rank_id,
                rank_slug: row.rank_slug,
            })
            .collect(),
    ))
}

async fn put_memberships(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveMemberships>,
) -> Result<Json<Vec<AccessMembership>>, ApiError> {
    if body.memberships.len() > 50 {
        return Err(bad("invalid", "at most 50 departments"));
    }
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM access_identities WHERE id = $1) AS "exists!""#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if !exists {
        return Err(not_found("person not found"));
    }
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    sqlx::query!(
        r#"DELETE FROM access_memberships WHERE identity_id = $1"#,
        id
    )
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    for item in &body.memberships {
        sqlx::query!(
            r#"
            INSERT INTO access_memberships (identity_id, department_id, rank_id)
            VALUES ($1, $2, $3)
            "#,
            id,
            item.department_id,
            item.rank_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            if is_fk(&error) {
                bad("invalid", "department or rank was not found")
            } else if is_unique(&error) {
                conflict("duplicate", "that department is listed twice")
            } else {
                ApiError::database(error)
            }
        })?;
    }
    tx.commit().await.map_err(ApiError::database)?;
    list_memberships(
        State(state),
        Manager {
            email: String::new(),
            panel_owner: true,
        },
        Path(id),
    )
    .await
}

async fn get_policy(
    State(state): State<AppState>,
    _manager: Manager,
    Path(id): Path<Uuid>,
) -> Result<Json<AccessPolicyView>, ApiError> {
    Ok(Json(policy_view(&state, id).await?))
}

async fn put_policy(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<Uuid>,
    Json(body): Json<SaveAccessPolicy>,
) -> Result<Json<AccessPolicyView>, ApiError> {
    if body.rules.len() > 100 {
        return Err(bad("invalid", "at most 100 rules"));
    }
    let service = sqlx::query_scalar!(r#"SELECT id FROM services WHERE id = $1"#, id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if service.is_none() {
        return Err(not_found("service not found"));
    }
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    sqlx::query!(
        r#"
        INSERT INTO access_policies (service_id, enabled)
        VALUES ($1, $2)
        ON CONFLICT (service_id) DO UPDATE SET enabled = $2, updated_at = now()
        "#,
        id,
        body.enabled,
    )
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    sqlx::query!(r#"DELETE FROM access_rules WHERE service_id = $1"#, id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
    for rule in &body.rules {
        let rule = clean_rule(rule)?;
        sqlx::query!(
            r#"
            INSERT INTO access_rules (service_id, kind, email, department_id, rank_id)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            id,
            rule.kind,
            rule.email,
            rule.department_id,
            rule.rank_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            if is_fk(&error) {
                bad("invalid", "department or rank was not found")
            } else {
                ApiError::database(error)
            }
        })?;
    }
    tx.commit().await.map_err(ApiError::database)?;
    audit::record(
        &state.pool,
        None,
        "access.policy",
        "service",
        Some(&id.to_string()),
        json!({ "actor": manager.email, "enabled": body.enabled }),
        None,
    )
    .await;
    let _ = crate::publish::publish_for_service(&state, id).await;
    Ok(Json(policy_view(&state, id).await?))
}

async fn rotate_token(
    State(state): State<AppState>,
    manager: Manager,
    Path(id): Path<Uuid>,
) -> Result<Json<AccessTokenCreated>, ApiError> {
    let service = sqlx::query_scalar!(r#"SELECT id FROM services WHERE id = $1"#, id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if service.is_none() {
        return Err(not_found("service not found"));
    }
    let token = auth::random_token("rdr_acc");
    let hash = auth::sha256_hex(&token);
    let encrypted = encrypt(&state, token.as_bytes())?;
    sqlx::query!(
        r#"
        INSERT INTO access_service_tokens (service_id, token_hash, token_enc)
        VALUES ($1, $2, $3)
        ON CONFLICT (service_id) DO UPDATE
        SET token_hash = $2, token_enc = $3, created_at = now()
        "#,
        id,
        hash,
        encrypted,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    audit::record(
        &state.pool,
        None,
        "access.token",
        "service",
        Some(&id.to_string()),
        json!({ "actor": manager.email }),
        None,
    )
    .await;
    Ok(Json(AccessTokenCreated { token }))
}

async fn email_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    auth::OptionalPeer(peer): auth::OptionalPeer,
    Json(body): Json<AccessEmailStart>,
) -> Result<StatusCode, ApiError> {
    let ip = auth::client_ip(&headers, peer, state.config.trust_proxy);
    limit_ip(&state, &ip)?;
    let _pending_return = (body.return_host.as_deref(), body.return_path.as_deref());
    let settings = load_settings(&state).await?;
    verify_turnstile(&state, &settings, &body.turnstile_token, &ip).await?;
    let email = auth::normalize_email(&body.email)?;
    let identity = active_identity(&state, &email).await?;
    let Some(_identity) = identity else {
        return Err(blocked());
    };
    let config = mail_config(&state, &settings)?;
    let recent = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
        FROM access_codes
        WHERE email = $1 AND purpose = 'login' AND created_at > now() - interval '1 hour'
        "#,
        email
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if recent >= 5 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many codes were requested",
        ));
    }
    let code = six_digits();
    sqlx::query!(
        r#"
        UPDATE access_codes SET used_at = now()
        WHERE email = $1 AND purpose = 'login' AND used_at IS NULL
        "#,
        email
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    sqlx::query!(
        r#"
        INSERT INTO access_codes (email, code_hash, purpose, expires_at)
        VALUES ($1, $2, 'login', now() + interval '10 minutes')
        "#,
        email,
        auth::sha256_hex(&code),
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    send_login_code(http(), &config, &settings.email_from, &email, &code).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn email_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    auth::OptionalPeer(peer): auth::OptionalPeer,
    jar: CookieJar,
    Json(body): Json<AccessEmailVerify>,
) -> Result<(CookieJar, Json<AccessLoginResult>), ApiError> {
    let ip = auth::client_ip(&headers, peer, state.config.trust_proxy);
    limit_ip(&state, &ip)?;
    let settings = load_settings(&state).await?;
    verify_turnstile(&state, &settings, &body.turnstile_token, &ip).await?;
    let email = auth::normalize_email(&body.email)?;
    let Some(identity) = active_identity(&state, &email).await? else {
        return Err(blocked());
    };
    let code = body.code.trim();
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_code());
    }
    consume_login_code(&state, &email, code).await?;
    let host = request_host(&headers);
    let (jar, csrf) = issue_session(&state, jar, identity, &host, &ip, &headers).await?;
    let redirect = handoff_url(
        &state,
        &email,
        body.return_host.as_deref(),
        body.return_path.as_deref(),
    )
    .await?;
    Ok((
        jar,
        Json(AccessLoginResult {
            redirect,
            csrf_token: csrf,
        }),
    ))
}

async fn google_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    auth::OptionalPeer(peer): auth::OptionalPeer,
    Json(body): Json<AccessGoogleStart>,
) -> Result<Json<AccessGoogleUrl>, ApiError> {
    let ip = auth::client_ip(&headers, peer, state.config.trust_proxy);
    limit_ip(&state, &ip)?;
    let settings = load_settings(&state).await?;
    verify_turnstile(&state, &settings, &body.turnstile_token, &ip).await?;
    if !google_ready(&settings) {
        return Err(not_ready("Google sign-in is not configured"));
    }
    let client_id = settings.google_client_id.clone();
    let origin = portal_origin(&state, &settings, &headers).await?;
    let (host, path) = clean_return(body.return_host.as_deref(), body.return_path.as_deref())?;
    let payload = json!({
        "exp": (Utc::now() + Duration::minutes(CODE_MINUTES)).timestamp(),
        "host": host,
        "path": path,
        "nonce": hex::encode(rudder_core::random_bytes::<16>()),
    });
    let state_value = sign_state(state.config.master_key(), &payload.to_string())?;
    let url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri={}&response_type=code&scope=openid%20email%20profile&state={}&prompt=select_account",
        url_encode(&client_id),
        url_encode(&redirect_uri(&origin)),
        url_encode(&state_value),
    );
    Ok(Json(AccessGoogleUrl { url }))
}

#[derive(Deserialize)]
struct GoogleQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn google_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    auth::OptionalPeer(peer): auth::OptionalPeer,
    jar: CookieJar,
    Query(query): Query<GoogleQuery>,
) -> Result<Response, ApiError> {
    if query.error.is_some() {
        return Ok(Redirect::temporary("/access?error=blocked").into_response());
    }
    let code = query.code.unwrap_or_default();
    let state_value = query.state.unwrap_or_default();
    let payload = open_state(state.config.master_key(), &state_value)?;
    let settings = load_settings(&state).await?;
    let origin = portal_origin(&state, &settings, &headers).await?;
    let secret = decrypt_opt(&state, settings.google_client_secret_enc.as_deref())?;
    let token = http()
        .post(&state.config.google_token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("code", code.as_str()),
            ("client_id", settings.google_client_id.as_str()),
            ("client_secret", secret.as_str()),
            ("redirect_uri", &redirect_uri(&origin)),
            ("grant_type", "authorization_code"),
        ]))
        .send()
        .await
        .map_err(|_| blocked())?;
    if !token.status().is_success() {
        return Err(blocked());
    }
    let token: Value = token.json().await.map_err(|_| blocked())?;
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    if access.is_empty() {
        return Err(blocked());
    }
    let profile = http()
        .get(&state.config.google_userinfo_url)
        .bearer_auth(access)
        .send()
        .await
        .map_err(|_| blocked())?;
    if !profile.status().is_success() {
        return Err(blocked());
    }
    let profile: Value = profile.json().await.map_err(|_| blocked())?;
    let verified = profile
        .get("email_verified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let email = profile.get("email").and_then(Value::as_str).unwrap_or("");
    let sub = profile.get("sub").and_then(Value::as_str).unwrap_or("");
    if !verified || sub.is_empty() {
        return Err(blocked());
    }
    let email = auth::normalize_email(email)?;
    let Some(identity) = active_identity(&state, &email).await? else {
        return Ok(Redirect::temporary("/access?error=blocked").into_response());
    };
    link_google(&state, identity, sub).await?;
    let ip = auth::client_ip(&headers, peer, state.config.trust_proxy);
    let host = request_host(&headers);
    let (jar, _csrf) = issue_session(&state, jar, identity, &host, &ip, &headers).await?;
    let return_host = payload.get("host").and_then(Value::as_str);
    let return_path = payload.get("path").and_then(Value::as_str);
    let next = match handoff_url(&state, &email, return_host, return_path).await? {
        Some(url) => url,
        None => "/access".to_owned(),
    };
    let mut response = Redirect::temporary(&next).into_response();
    if let Some(cookie) = jar.get(ACCESS_COOKIE) {
        if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
            response.headers_mut().insert(SET_COOKIE, value);
        }
    }
    Ok(response)
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Result<CookieJar, ApiError> {
    if let Some(token) = cookie_value(&headers, ACCESS_COOKIE) {
        let hash = auth::sha256_hex(&token);
        let _ = sqlx::query!(r#"DELETE FROM access_sessions WHERE token_hash = $1"#, hash)
            .execute(&state.pool)
            .await;
    }
    Ok(jar.remove(Cookie::from(ACCESS_COOKIE)))
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<AccessMe>, ApiError> {
    let session = portal_session(&state, &headers).await?;
    Ok(Json(AccessMe {
        email: session.email,
        display_name: session.display_name,
        admin: session.admin,
        owner: session.owner,
        csrf_token: session.csrf,
    }))
}

async fn apps(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<AccessApp>>, ApiError> {
    let session = portal_session(&state, &headers).await?;
    if session.status != "active" {
        return Err(blocked());
    }
    let rows = sqlx::query!(
        r#"
        SELECT s.id, s.name,
               (
                   SELECT d.host FROM domains d
                   WHERE d.service_id = s.id AND NOT d.is_preview
                   ORDER BY d.created_at
                   LIMIT 1
               ) AS "host?"
        FROM services s
        JOIN access_policies p ON p.service_id = s.id AND p.enabled
        ORDER BY s.name
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let mut apps = Vec::new();
    for row in rows {
        if person_allowed(&state, session.identity_id, &session.email, row.id).await? {
            apps.push(AccessApp {
                service_id: row.id,
                name: row.name,
                host: row.host,
            });
        }
    }
    Ok(Json(apps))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
}

async fn handoff_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    auth::OptionalPeer(peer): auth::OptionalPeer,
    jar: CookieJar,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    let hash = auth::sha256_hex(query.code.trim());
    let row = sqlx::query!(
        r#"
        SELECT id, email, return_host, return_path, expires_at, used_at
        FROM access_codes
        WHERE code_hash = $1 AND purpose = 'handoff'
        "#,
        hash
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(blocked());
    };
    if row.used_at.is_some() || row.expires_at <= Utc::now() {
        return Err(blocked());
    }
    let host = row.return_host.unwrap_or_default();
    let path =
        safe_return_path(row.return_path.as_deref().unwrap_or("/")).unwrap_or_else(|| "/".into());
    sqlx::query!(
        r#"UPDATE access_codes SET used_at = now() WHERE id = $1"#,
        row.id
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(identity) = active_identity(&state, &row.email).await? else {
        return Err(blocked());
    };
    let ip = auth::client_ip(&headers, peer, state.config.trust_proxy);
    let (jar, _csrf) = issue_session(&state, jar, identity, &host, &ip, &headers).await?;
    let mut response = Redirect::temporary(&path).into_response();
    if let Some(cookie) = jar.get(ACCESS_COOKIE) {
        if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
            response.headers_mut().insert(SET_COOKIE, value);
        }
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn introspect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AccessIntrospectRequest>,
) -> Result<Json<AccessIntrospect>, ApiError> {
    let token = bearer(&headers).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "service token required",
        )
    })?;
    if !token.starts_with("rdr_acc_") {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "service token required",
        ));
    }
    let service_id = sqlx::query_scalar!(
        r#"SELECT service_id FROM access_service_tokens WHERE token_hash = $1"#,
        auth::sha256_hex(&token)
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(service_id) = service_id else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "service token required",
        ));
    };
    let session = load_session(&state, body.session.trim()).await?;
    let owns = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM domains WHERE service_id = $1 AND host = $2
        ) AS "owns!"
        "#,
        service_id,
        session.host
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if !owns || !person_allowed(&state, session.identity_id, &session.email, service_id).await? {
        return Err(blocked());
    }
    let departments = grants(&state, session.identity_id).await?;
    Ok(Json(AccessIntrospect {
        email: session.email,
        display_name: session.display_name,
        departments,
        expires_at: session.expires_at,
    }))
}

async fn edge_gate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let secret = match gate_secret(&state).await {
        Ok(secret) => secret,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let provided = query.get("k").map(String::as_str).unwrap_or("");
    if !auth::tokens_eq(&auth::sha256_hex(provided), &auth::sha256_hex(&secret)) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let host = forwarded(&headers, "x-forwarded-host").unwrap_or_default();
    let host = host
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let uri = forwarded(&headers, "x-forwarded-uri").unwrap_or_else(|| "/".into());
    let path = uri.split('?').next().unwrap_or("/");
    let method = forwarded(&headers, "x-forwarded-method").unwrap_or_else(|| "GET".into());
    let ip = forwarded(&headers, "x-forwarded-for")
        .and_then(|value| value.split(',').next().map(|item| item.trim().to_owned()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let ip = auth::tunnel_client_ip(&headers, &ip).unwrap_or(ip);
    if forwarded(&headers, "x-forwarded-proto").as_deref() == Some("http") {
        state
            .edge
            .http_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    if rudder_core::host_is_ip(host.split(':').next().unwrap_or(&host)) {
        state
            .edge
            .ip_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let mut header_map = HashMap::new();
    for (name, value) in headers.iter() {
        if let Ok(text) = value.to_str() {
            header_map.insert(name.as_str().to_ascii_lowercase(), text.to_owned());
        }
    }
    let decision = match state
        .edge
        .decide(&state, &host, path, &method, &ip, &header_map)
        .await
    {
        Ok(decision) => decision,
        Err(_) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if !decision.allowed {
        if let Some(id) = decision
            .blocked_by
            .as_deref()
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            let _ = sqlx::query!(
                r#"
                INSERT INTO rate_limit_blocks (rule_id, host, path, ip)
                VALUES ($1, $2, $3, $4)
                "#,
                id,
                host,
                path,
                ip,
            )
            .execute(&state.pool)
            .await;
        }
        let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            HeaderValue::from_str(&decision.retry_after_secs.to_string())
                .unwrap_or(HeaderValue::from_static("60")),
        );
        return response;
    }
    let protected = match protected_service(&state, &host).await {
        Ok(value) => value,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(service_id) = protected else {
        return StatusCode::OK.into_response();
    };
    let Some(token) = cookie_value(&headers, ACCESS_COOKIE) else {
        return redirect_to_login(&state, &host, path).await;
    };
    let session = match load_session(&state, &token).await {
        Ok(session)
            if session.host.eq_ignore_ascii_case(&strip_port(&host))
                && session.status == "active" =>
        {
            session
        }
        _ => return redirect_to_login(&state, &host, path).await,
    };
    match person_allowed(&state, session.identity_id, &session.email, service_id).await {
        Ok(true) => {}
        Ok(false) => return redirect_denied(&state).await,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    let grants = grants(&state, session.identity_id)
        .await
        .unwrap_or_default();
    let departments = grants
        .iter()
        .map(|item| item.slug.clone())
        .collect::<Vec<_>>()
        .join(",");
    let ranks = grants
        .iter()
        .filter_map(|item| item.rank.clone())
        .collect::<Vec<_>>()
        .join(",");
    let mut response = StatusCode::OK.into_response();
    insert_header(&mut response, "x-rudder-access-email", &session.email);
    insert_header(&mut response, "x-rudder-access-user", &session.display_name);
    insert_header(&mut response, "x-rudder-access-departments", &departments);
    insert_header(&mut response, "x-rudder-access-ranks", &ranks);
    response
}

pub(crate) async fn gate_secret(state: &AppState) -> Result<String, ApiError> {
    let current = sqlx::query!(r#"SELECT gate_secret_enc FROM access_settings WHERE id = TRUE"#)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if let Some(bytes) = current.gate_secret_enc {
        return decrypt_string(state, &bytes);
    }
    let secret = hex::encode(rudder_core::random_bytes::<32>());
    let encrypted = encrypt(state, secret.as_bytes())?;
    sqlx::query!(
        r#"UPDATE access_settings SET gate_secret_enc = $1 WHERE id = TRUE AND gate_secret_enc IS NULL"#,
        encrypted
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let stored = sqlx::query!(r#"SELECT gate_secret_enc FROM access_settings WHERE id = TRUE"#)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::database)?;
    decrypt_string(state, stored.gate_secret_enc.as_deref().unwrap_or_default())
}

pub(crate) async fn service_token(
    state: &AppState,
    service_id: Uuid,
) -> Result<Option<String>, ApiError> {
    let row = sqlx::query!(
        r#"SELECT token_enc FROM access_service_tokens WHERE service_id = $1"#,
        service_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    match row {
        Some(row) => Ok(Some(decrypt_string(state, &row.token_enc)?)),
        None => Ok(None),
    }
}

struct LoadedSession {
    identity_id: Uuid,
    email: String,
    display_name: String,
    status: String,
    host: String,
    csrf: String,
    admin: bool,
    owner: bool,
    expires_at: DateTime<Utc>,
}

async fn load_session(state: &AppState, token: &str) -> Result<LoadedSession, ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT s.identity_id, s.csrf_token, s.host, s.expires_at,
               i.email, i.display_name, i.status, a.is_owner AS "is_owner?"
        FROM access_sessions s
        JOIN access_identities i ON i.id = s.identity_id
        LEFT JOIN access_admins a ON a.identity_id = i.id
        WHERE s.token_hash = $1
        "#,
        auth::sha256_hex(token)
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in required",
        ));
    };
    if row.expires_at <= Utc::now() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "session expired",
        ));
    }
    Ok(LoadedSession {
        identity_id: row.identity_id,
        email: row.email,
        display_name: row.display_name,
        status: row.status,
        host: row.host,
        csrf: row.csrf_token,
        admin: row.is_owner.is_some(),
        owner: row.is_owner.unwrap_or(false),
        expires_at: row.expires_at,
    })
}

async fn portal_session(state: &AppState, headers: &HeaderMap) -> Result<LoadedSession, ApiError> {
    let token = cookie_value(headers, ACCESS_COOKIE).ok_or_else(|| {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", "sign in required")
    })?;
    let session = load_session(state, &token).await?;
    if session.host != request_host(headers) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in required",
        ));
    }
    Ok(session)
}

async fn access_admin(state: &AppState, parts: &Parts) -> Result<Manager, ApiError> {
    let session = portal_session(state, &parts.headers).await?;
    if !session.admin {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Access administrator required",
        ));
    }
    if matches!(
        parts.method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::PATCH
            | axum::http::Method::DELETE
    ) {
        let provided = parts
            .headers
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !auth::tokens_eq(provided, &session.csrf) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "csrf",
                "missing or invalid CSRF token",
            ));
        }
    }
    Ok(Manager {
        email: session.email,
        panel_owner: false,
    })
}

async fn issue_session(
    state: &AppState,
    jar: CookieJar,
    identity: Uuid,
    host: &str,
    ip: &str,
    headers: &HeaderMap,
) -> Result<(CookieJar, String), ApiError> {
    let token = hex::encode(rudder_core::random_bytes::<32>());
    let csrf = hex::encode(rudder_core::random_bytes::<16>());
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(200).collect::<String>());
    sqlx::query!(
        r#"
        INSERT INTO access_sessions (identity_id, token_hash, csrf_token, host, expires_at, user_agent, ip)
        VALUES ($1, $2, $3, $4, now() + interval '12 hours', $5, $6)
        "#,
        identity,
        auth::sha256_hex(&token),
        csrf,
        host,
        user_agent,
        ip,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let _ = sqlx::query!(
        r#"UPDATE access_identities SET last_login_at = now() WHERE id = $1"#,
        identity
    )
    .execute(&state.pool)
    .await;
    let cookie = Cookie::build((ACCESS_COOKIE, token))
        .path("/")
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(cookie::SameSite::Lax)
        .max_age(cookie::time::Duration::hours(SESSION_HOURS))
        .build();
    Ok((jar.add(cookie), csrf))
}

async fn handoff_url(
    state: &AppState,
    email: &str,
    host: Option<&str>,
    path: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let (Some(host), path) = (host.filter(|host| !host.is_empty()), path) else {
        return Ok(None);
    };
    let (host, path) = clean_return(Some(host), path)?;
    let Some(host) = host else {
        return Ok(None);
    };
    let protected = protected_service(state, &host).await?;
    if protected.is_none() {
        return Err(bad("invalid", "that app is not protected by Rudder Access"));
    }
    let code = hex::encode(rudder_core::random_bytes::<32>());
    sqlx::query!(
        r#"
        INSERT INTO access_codes (email, code_hash, purpose, return_host, return_path, expires_at)
        VALUES ($1, $2, 'handoff', $3, $4, now() + interval '2 minutes')
        "#,
        email,
        auth::sha256_hex(&code),
        host,
        path,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Some(format!("https://{host}/_rudder/callback?code={code}")))
}

async fn protected_service(state: &AppState, host: &str) -> Result<Option<Uuid>, ApiError> {
    let host = strip_port(host);
    sqlx::query_scalar!(
        r#"
        SELECT s.id
        FROM domains d
        JOIN services s ON s.id = d.service_id
        JOIN access_policies p ON p.service_id = s.id AND p.enabled
        WHERE d.host = $1
        LIMIT 1
        "#,
        host
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)
}

async fn person_allowed(
    state: &AppState,
    identity: Uuid,
    email: &str,
    service_id: Uuid,
) -> Result<bool, ApiError> {
    let memberships = sqlx::query!(
        r#"
        SELECT d.slug AS department, r.slug AS "rank?"
        FROM access_memberships m
        JOIN access_departments d ON d.id = m.department_id
        LEFT JOIN access_ranks r ON r.id = m.rank_id
        WHERE m.identity_id = $1
        "#,
        identity
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let memberships = memberships
        .into_iter()
        .map(|row| Membership {
            department: row.department,
            rank: row.rank,
        })
        .collect::<Vec<_>>();
    let rules = sqlx::query!(
        r#"
        SELECT kind, email, d.slug AS "department?", r.slug AS "rank?"
        FROM access_rules rule
        LEFT JOIN access_departments d ON d.id = rule.department_id
        LEFT JOIN access_ranks r ON r.id = rule.rank_id
        WHERE rule.service_id = $1
        "#,
        service_id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let rules = rules
        .into_iter()
        .filter_map(|row| match row.kind.as_str() {
            "email" => row.email.map(AccessRule::Email),
            "department" => row.department.map(AccessRule::Department),
            "rank" => row.rank.map(AccessRule::Rank),
            _ => None,
        })
        .collect::<Vec<_>>();
    let edges = sqlx::query!(
        r#"
        SELECT child.slug AS child, parent.slug AS "parent?"
        FROM access_departments child
        LEFT JOIN access_departments parent ON parent.id = child.parent_id
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let edges = edges
        .into_iter()
        .map(|row| (row.child, row.parent))
        .collect::<Vec<_>>();
    let closure = department_closure(&edges);
    Ok(access_allowed(email, &memberships, &rules, &closure))
}

async fn grants(state: &AppState, identity: Uuid) -> Result<Vec<AccessDepartmentGrant>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT d.slug, d.name, r.slug AS "rank?", r.level AS "level?"
        FROM access_memberships m
        JOIN access_departments d ON d.id = m.department_id
        LEFT JOIN access_ranks r ON r.id = m.rank_id
        WHERE m.identity_id = $1
        ORDER BY d.slug
        "#,
        identity
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(rows
        .into_iter()
        .map(|row| AccessDepartmentGrant {
            slug: row.slug,
            name: row.name,
            rank: row.rank,
            level: row.level,
        })
        .collect())
}

async fn redirect_to_login(state: &AppState, host: &str, path: &str) -> Response {
    let settings = load_settings(state).await.ok();
    let origin = match settings {
        Some(settings) => portal_origin(state, &settings, &HeaderMap::new())
            .await
            .unwrap_or_else(|_| String::new()),
        None => String::new(),
    };
    if origin.is_empty() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let host = strip_port(host);
    let path = safe_return_path(path).unwrap_or_else(|| "/".into());
    let location = format!(
        "{origin}/access?host={}&path={}",
        url_encode(&host),
        url_encode(&path)
    );
    Redirect::temporary(&location).into_response()
}

async fn redirect_denied(state: &AppState) -> Response {
    let settings = load_settings(state).await.ok();
    let origin = match settings {
        Some(settings) => portal_origin(state, &settings, &HeaderMap::new())
            .await
            .unwrap_or_else(|_| String::new()),
        None => String::new(),
    };
    if origin.is_empty() {
        return StatusCode::FORBIDDEN.into_response();
    }
    Redirect::temporary(&format!("{origin}/access?error=blocked")).into_response()
}

async fn load_settings(state: &AppState) -> Result<SettingsRow, ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT portal_host, turnstile_site_key, turnstile_secret_enc, turnstile_hostnames,
               google_client_id, google_client_secret_enc, email_provider, email_from, email_config_enc
        FROM access_settings
        WHERE id = TRUE
        "#
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(SettingsRow {
        portal_host: row.portal_host,
        turnstile_site_key: row.turnstile_site_key,
        turnstile_secret_enc: row.turnstile_secret_enc,
        turnstile_hostnames: row.turnstile_hostnames,
        google_client_id: row.google_client_id,
        google_client_secret_enc: row.google_client_secret_enc,
        email_provider: row.email_provider,
        email_from: row.email_from,
        email_config_enc: row.email_config_enc,
    })
}

fn settings_view(settings: &SettingsRow, origin: &str) -> AccessSettingsView {
    AccessSettingsView {
        portal_host: settings.portal_host.clone(),
        turnstile_site_key: settings.turnstile_site_key.clone(),
        has_turnstile_secret: settings.turnstile_secret_enc.is_some(),
        turnstile_hostnames: settings.turnstile_hostnames.clone(),
        google_client_id: settings.google_client_id.clone(),
        has_google_secret: settings.google_client_secret_enc.is_some(),
        email_provider: settings.email_provider.clone(),
        email_from: settings.email_from.clone(),
        email_ready: email_ready(settings),
        google_redirect_uri: redirect_uri(origin),
    }
}

fn turnstile_ready(settings: &SettingsRow) -> bool {
    !settings.turnstile_site_key.is_empty()
        && settings.turnstile_secret_enc.is_some()
        && !settings.turnstile_hostnames.is_empty()
}

fn google_ready(settings: &SettingsRow) -> bool {
    !settings.google_client_id.is_empty() && settings.google_client_secret_enc.is_some()
}

fn email_ready(settings: &SettingsRow) -> bool {
    settings.email_provider != "none"
        && settings.email_config_enc.is_some()
        && !settings.email_from.is_empty()
}

async fn verify_turnstile(
    state: &AppState,
    settings: &SettingsRow,
    token: &str,
    ip: &str,
) -> Result<(), ApiError> {
    if !turnstile_ready(settings) {
        return Err(not_ready("Turnstile is not configured"));
    }
    if token.is_empty() || token.len() > 2048 {
        return Err(forbidden());
    }
    let secret = decrypt_string(
        state,
        settings.turnstile_secret_enc.as_deref().unwrap_or_default(),
    )?;
    let response = http()
        .post(&state.config.turnstile_siteverify_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("secret", secret.as_str()),
            ("response", token),
            ("remoteip", ip),
        ]))
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(error = %error.without_url(), "turnstile siteverify failed");
            forbidden()
        })?;
    if !response.status().is_success() {
        return Err(forbidden());
    }
    let body: Value = response.json().await.map_err(|_| forbidden())?;
    let parsed = parse_turnstile(&body);
    accept_turnstile(&parsed, ACCESS_LOGIN_ACTION, &settings.turnstile_hostnames).map_err(
        |reason| {
            tracing::info!(reason, "turnstile rejected an Access sign-in");
            forbidden()
        },
    )
}

fn mail_config(state: &AppState, settings: &SettingsRow) -> Result<MailConfig, ApiError> {
    let Some(bytes) = settings.email_config_enc.as_deref() else {
        return Err(not_ready("email is not configured"));
    };
    let text = decrypt_string(state, bytes)?;
    serde_json::from_str(&text).map_err(|_| not_ready("email is not configured"))
}

fn merged_mail(
    state: &AppState,
    current: &SettingsRow,
    body: &UpdateAccessSettings,
    provider: &str,
) -> Result<Option<Vec<u8>>, ApiError> {
    let existing = current
        .email_config_enc
        .as_deref()
        .and_then(|bytes| decrypt_string(state, bytes).ok())
        .and_then(|text| serde_json::from_str::<MailConfig>(&text).ok());
    let config = match provider {
        "none" => return Ok(None),
        "resend" => {
            let key = if let Some(value) = body
                .resend_api_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                value.to_owned()
            } else if let Some(MailConfig::Resend { api_key }) = existing {
                api_key
            } else {
                return Err(bad("invalid", "Resend needs an API key"));
            };
            MailConfig::Resend { api_key: key }
        }
        "smtp" => {
            let (old_host, old_port, old_security, old_user, old_pass) = match existing {
                Some(MailConfig::Smtp {
                    host,
                    port,
                    security,
                    username,
                    password,
                }) => (Some(host), port, Some(security), username, password),
                _ => (None, None, None, None, None),
            };
            let host = body
                .smtp_host
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or(old_host)
                .ok_or_else(|| bad("invalid", "SMTP needs a host"))?;
            let security = match body.smtp_security.as_deref() {
                Some("tls") => crate::monitor::notify::SmtpSecurity::Tls,
                Some("none") => crate::monitor::notify::SmtpSecurity::None,
                Some("starttls") => crate::monitor::notify::SmtpSecurity::Starttls,
                None => old_security.unwrap_or(crate::monitor::notify::SmtpSecurity::Starttls),
                _ => {
                    return Err(bad(
                        "invalid",
                        "SMTP security must be starttls, tls, or none",
                    ))
                }
            };
            MailConfig::Smtp {
                host,
                port: body.smtp_port.or(old_port),
                security,
                username: body.smtp_username.clone().or(old_user),
                password: body
                    .smtp_password
                    .clone()
                    .filter(|value| !value.is_empty())
                    .or(old_pass),
            }
        }
        "cloudflare" => {
            let previous = match existing {
                Some(MailConfig::Cloudflare {
                    account_id,
                    api_token,
                }) => Some((account_id, api_token)),
                _ => None,
            };
            let account_id = body
                .cloudflare_account_id
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| previous.as_ref().map(|item| item.0.clone()))
                .ok_or_else(|| bad("invalid", "Cloudflare email needs an account id"))?;
            let api_token = body
                .cloudflare_api_token
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| previous.map(|item| item.1))
                .ok_or_else(|| bad("invalid", "Cloudflare email needs an API token"))?;
            MailConfig::Cloudflare {
                account_id,
                api_token,
            }
        }
        _ => return Err(bad("invalid", "unknown email provider")),
    };
    Ok(Some(encrypt(
        state,
        serde_json::to_string(&config)
            .unwrap_or_default()
            .as_bytes(),
    )?))
}

async fn consume_login_code(state: &AppState, email: &str, code: &str) -> Result<(), ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT id, code_hash, attempts, expires_at
        FROM access_codes
        WHERE email = $1 AND purpose = 'login' AND used_at IS NULL
        ORDER BY created_at DESC
        LIMIT 1
        "#,
        email
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(invalid_code());
    };
    let matches = auth::tokens_eq(&auth::sha256_hex(code), &row.code_hash);
    let attempts = row.attempts + 1;
    sqlx::query!(
        r#"
        UPDATE access_codes
        SET attempts = $2, used_at = CASE WHEN $3 OR $2 >= 5 THEN now() ELSE used_at END
        WHERE id = $1
        "#,
        row.id,
        attempts,
        matches,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if !matches || attempts > 5 || row.expires_at <= Utc::now() {
        return Err(invalid_code());
    }
    Ok(())
}

async fn active_identity(state: &AppState, email: &str) -> Result<Option<Uuid>, ApiError> {
    let row = sqlx::query!(
        r#"SELECT id, status FROM access_identities WHERE email = $1"#,
        email
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(row.and_then(|row| (row.status == "active").then_some(row.id)))
}

async fn link_google(state: &AppState, id: Uuid, sub: &str) -> Result<(), ApiError> {
    if sub.len() > 255 || sub.chars().any(|ch| ch.is_control()) {
        return Err(blocked());
    }
    let current = sqlx::query!(
        r#"SELECT google_sub FROM access_identities WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(current) = current else {
        return Err(blocked());
    };
    if let Some(saved) = current.google_sub {
        if saved != sub {
            return Err(blocked());
        }
        return Ok(());
    }
    sqlx::query!(
        r#"UPDATE access_identities SET google_sub = $2 WHERE id = $1 AND google_sub IS NULL"#,
        id,
        sub
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(())
}

async fn set_admin(
    state: &AppState,
    id: Uuid,
    admin: bool,
    panel_owner: bool,
) -> Result<bool, ApiError> {
    let owner = is_access_owner(state, id).await?;
    if owner && !admin && !panel_owner {
        return Err(conflict(
            "owner_protected",
            "the original Access administrator cannot lose administrator access",
        ));
    }
    if !admin {
        sqlx::query!(r#"DELETE FROM access_admins WHERE identity_id = $1"#, id)
            .execute(&state.pool)
            .await
            .map_err(ApiError::database)?;
        return Ok(false);
    }
    let owners =
        sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM access_admins WHERE is_owner"#)
            .fetch_one(&state.pool)
            .await
            .map_err(ApiError::database)?;
    let make_owner = owners == 0;
    sqlx::query!(
        r#"
        INSERT INTO access_admins (identity_id, is_owner)
        VALUES ($1, $2)
        ON CONFLICT (identity_id) DO NOTHING
        "#,
        id,
        make_owner,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    is_access_owner(state, id).await
}

async fn is_access_owner(state: &AppState, id: Uuid) -> Result<bool, ApiError> {
    sqlx::query_scalar!(
        r#"
        SELECT COALESCE((SELECT is_owner FROM access_admins WHERE identity_id = $1), FALSE) AS "owner!"
        "#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)
}

async fn policy_view(state: &AppState, id: Uuid) -> Result<AccessPolicyView, ApiError> {
    let enabled = sqlx::query_scalar!(
        r#"SELECT enabled FROM access_policies WHERE service_id = $1"#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .unwrap_or(false);
    let rules = sqlx::query!(
        r#"
        SELECT kind, email, department_id, rank_id
        FROM access_rules
        WHERE service_id = $1
        ORDER BY kind, email
        "#,
        id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let token = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM access_service_tokens WHERE service_id = $1) AS "exists!""#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(AccessPolicyView {
        enabled,
        rules: rules
            .into_iter()
            .map(|row| AccessRuleView {
                kind: row.kind,
                email: row.email,
                department_id: row.department_id,
                rank_id: row.rank_id,
            })
            .collect(),
        token_configured: token,
    })
}

async fn portal_origin(
    state: &AppState,
    settings: &SettingsRow,
    headers: &HeaderMap,
) -> Result<String, ApiError> {
    if let Some(host) = settings
        .portal_host
        .as_deref()
        .filter(|host| !host.is_empty())
    {
        return Ok(format!("https://{host}"));
    }
    let panel =
        sqlx::query!(r#"SELECT panel_domain, panel_ssl_mode FROM panel_settings WHERE id = TRUE"#)
            .fetch_one(&state.pool)
            .await
            .map_err(ApiError::database)?;
    if let Some(host) = panel.panel_domain.filter(|host| !host.is_empty()) {
        let scheme = if panel.panel_ssl_mode == "none" {
            "http"
        } else {
            "https"
        };
        return Ok(format!("{scheme}://{host}"));
    }
    if let Some(url) = state.config.public_url.as_deref() {
        return Ok(url.trim_end_matches('/').to_owned());
    }
    let host = request_host(headers);
    if host.is_empty() {
        return Err(not_ready("set a portal domain or the panel's public URL"));
    }
    let scheme = forwarded(headers, "x-forwarded-proto").unwrap_or_else(|| "http".into());
    Ok(format!("{scheme}://{host}"))
}

fn redirect_uri(origin: &str) -> String {
    format!("{origin}/api/v1/access/login/google/callback")
}

fn clean_return(
    host: Option<&str>,
    path: Option<&str>,
) -> Result<(Option<String>, Option<String>), ApiError> {
    let host = match host.map(str::trim).filter(|host| !host.is_empty()) {
        Some(host) => {
            let host = strip_port(host);
            validate_host(&host).map_err(|_| bad("invalid", "return host is not a hostname"))?;
            Some(host)
        }
        None => None,
    };
    let path = match path {
        Some(path) => Some(
            safe_return_path(path).ok_or_else(|| bad("invalid", "return path is not allowed"))?,
        ),
        None => None,
    };
    Ok((host, path))
}

fn clean_org(
    name: &str,
    slug: &str,
    description: Option<&str>,
) -> Result<(String, String, String), ApiError> {
    let name = clean_label(name, "name")?;
    let slug = clean_slug(slug)?;
    let description = description.unwrap_or("").trim().chars().take(400).collect();
    Ok((name, slug, description))
}

fn clean_rank(body: &SaveRank) -> Result<(String, String, i32, String), ApiError> {
    if !(0..=1000).contains(&body.level) {
        return Err(bad("invalid", "rank level must be between 0 and 1000"));
    }
    let (name, slug, description) = clean_org(&body.name, &body.slug, body.description.as_deref())?;
    Ok((name, slug, body.level, description))
}

struct StoredRule {
    kind: String,
    email: Option<String>,
    department_id: Option<Uuid>,
    rank_id: Option<Uuid>,
}

fn clean_rule(rule: &RuleInput) -> Result<StoredRule, ApiError> {
    match rule.kind.as_str() {
        "email" => {
            let email = auth::normalize_email(rule.email.as_deref().unwrap_or(""))?;
            Ok(StoredRule {
                kind: "email".into(),
                email: Some(email),
                department_id: None,
                rank_id: None,
            })
        }
        "department" => Ok(StoredRule {
            kind: "department".into(),
            email: None,
            department_id: Some(
                rule.department_id
                    .ok_or_else(|| bad("invalid", "department is required"))?,
            ),
            rank_id: None,
        }),
        "rank" => Ok(StoredRule {
            kind: "rank".into(),
            email: None,
            department_id: None,
            rank_id: Some(
                rule.rank_id
                    .ok_or_else(|| bad("invalid", "rank is required"))?,
            ),
        }),
        _ => Err(bad(
            "invalid",
            "rule kind must be email, department, or rank",
        )),
    }
}

fn clean_label(value: &str, field: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 80 || value.chars().any(|ch| ch.is_control()) {
        return Err(bad(
            "invalid",
            format!("{field} must be 1 to 80 characters"),
        ));
    }
    Ok(value.to_owned())
}

fn clean_slug(value: &str) -> Result<String, ApiError> {
    let value = value.trim().to_ascii_lowercase();
    let ok = (1..=48).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-');
    if ok {
        Ok(value)
    } else {
        Err(bad(
            "invalid",
            "slug must be lowercase letters, digits, and hyphens",
        ))
    }
}

fn clean_status(value: Option<&str>) -> Result<String, ApiError> {
    match value.unwrap_or("active") {
        "active" => Ok("active".into()),
        "blocked" => Ok("blocked".into()),
        _ => Err(bad("invalid", "status must be active or blocked")),
    }
}

fn display(requested: &Option<String>, email: &str) -> String {
    let trimmed = requested.as_deref().unwrap_or("").trim();
    if trimmed.is_empty() {
        email.split('@').next().unwrap_or("person").to_owned()
    } else {
        trimmed.chars().take(80).collect()
    }
}

async fn department_exists(state: &AppState, id: Uuid) -> Result<(), ApiError> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM access_departments WHERE id = $1) AS "exists!""#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if exists {
        Ok(())
    } else {
        Err(not_found("parent department not found"))
    }
}

async fn parent_cycles(state: &AppState, id: Uuid, parent: Option<Uuid>) -> Result<bool, ApiError> {
    let mut current = parent;
    for _ in 0..64 {
        let Some(next) = current else {
            return Ok(false);
        };
        if next == id {
            return Ok(true);
        }
        current = sqlx::query_scalar!(
            r#"SELECT parent_id FROM access_departments WHERE id = $1"#,
            next
        )
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?
        .flatten();
    }
    Ok(true)
}

fn six_digits() -> String {
    let bytes = rudder_core::random_bytes::<4>();
    let value = u32::from_be_bytes(bytes) % 1_000_000;
    format!("{value:06}")
}

fn limit_ip(state: &AppState, ip: &str) -> Result<(), ApiError> {
    if state
        .login_limiter
        .check_key(&format!("access:{ip}"))
        .is_err()
    {
        Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many sign-in attempts",
        ))
    } else {
        Ok(())
    }
}

fn sign_state(key: &[u8; 32], payload: &str) -> Result<String, ApiError> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "could not sign state",
        )
    })?;
    let body = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload.as_bytes(),
    );
    mac.update(body.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    Ok(format!("{body}.{sig}"))
}

fn open_state(key: &[u8; 32], value: &str) -> Result<Value, ApiError> {
    let Some((body, sig)) = value.split_once('.') else {
        return Err(blocked());
    };
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).map_err(|_| blocked())?;
    mac.update(body.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    if !auth::tokens_eq(&expected, sig) {
        return Err(blocked());
    }
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, body)
        .map_err(|_| blocked())?;
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| blocked())?;
    let exp = payload.get("exp").and_then(Value::as_i64).unwrap_or(0);
    if exp < Utc::now().timestamp() {
        return Err(blocked());
    }
    Ok(payload)
}

fn encrypt(state: &AppState, bytes: &[u8]) -> Result<Vec<u8>, ApiError> {
    state.cipher.encrypt(bytes).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "could not store a secret",
        )
    })
}

fn decrypt_string(state: &AppState, bytes: &[u8]) -> Result<String, ApiError> {
    let plain = state.cipher.decrypt(bytes).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "stored secret could not be read",
        )
    })?;
    String::from_utf8(plain).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "stored secret could not be read",
        )
    })
}

fn decrypt_opt(state: &AppState, bytes: Option<&[u8]>) -> Result<String, ApiError> {
    match bytes {
        Some(bytes) => decrypt_string(state, bytes),
        None => Err(not_ready("Google sign-in is not configured")),
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for item in Cookie::split_parse(header) {
        let Ok(cookie) = item else { continue };
        if cookie.name() == name {
            return Some(cookie.value().to_owned());
        }
    }
    None
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)?
                .to_str()
                .ok()?
                .strip_prefix("bearer ")
        })
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

fn request_host(headers: &HeaderMap) -> String {
    let raw = forwarded(headers, "x-forwarded-host")
        .or_else(|| {
            headers
                .get(axum::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    strip_port(raw.split(',').next().unwrap_or("").trim())
}

fn strip_port(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(host) = host.strip_prefix('[') {
        return host.split(']').next().unwrap_or(host).to_owned();
    }
    host.split(':').next().unwrap_or(&host).to_owned()
}

fn forwarded(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn insert_header(response: &mut Response, name: &'static str, value: &str) {
    if value.is_empty() || value.chars().any(|ch| ch.is_control()) {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(name, value);
    }
}

fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn url_encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn blocked() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "access_blocked", "access is blocked")
}

fn forbidden() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        "sign-in could not be verified",
    )
}

fn invalid_code() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_code",
        "the code is not valid",
    )
}

fn not_ready(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "not_ready", message)
}

fn is_fk(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_foreign_key_violation())
}
