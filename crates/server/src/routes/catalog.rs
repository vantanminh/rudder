//! Servers, projects, environments, and services.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use rudder_core::{validate_env_key, validate_name, Error as CoreError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::audit;
use crate::auth::{self, AuthUser};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Serialize, ToSchema)]
pub struct ServerView {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub public_ip: Option<String>,
    pub description: String,
    pub agent_version: Option<String>,
    pub docker_version: Option<String>,
    pub os_info: Option<String>,
    pub cpu_count: Option<i32>,
    pub memory_bytes: Option<i64>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Remove dangling images and build cache once a day.
    pub auto_prune: bool,
    pub last_pruned_at: Option<DateTime<Utc>>,
    /// Hostname the agent reported.
    pub hostname: Option<String>,
    /// When the agent last exchanged a join token for a certificate.
    pub enrolled_at: Option<DateTime<Utc>>,
    /// Telemetry items the agent discarded because its buffer was full.
    pub telemetry_dropped: i64,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedServer {
    #[serde(flatten)]
    pub server: ServerView,
    pub join_token: Option<String>,
    pub join_command: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateServer {
    pub name: String,
    pub kind: String,
    pub public_ip: Option<String>,
    pub description: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateServer {
    pub name: Option<String>,
    pub public_ip: Option<String>,
    pub description: Option<String>,
    /// Prune dangling images and build cache once a day.
    pub auto_prune: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct ProjectView {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateProject {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct EnvironmentView {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub description: String,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateEnvironment {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ServiceView {
    pub id: Uuid,
    pub environment_id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub spec: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct ServiceSummary {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub server_id: Uuid,
    pub environment_id: Uuid,
    pub environment_name: String,
    pub project_id: Uuid,
    pub project_name: String,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateService {
    pub name: String,
    pub kind: String,
    pub server_id: Uuid,
    pub spec: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateNetwork {
    pub isolation: bool,
}

#[derive(Serialize, ToSchema)]
pub struct NetworkView {
    pub isolation: bool,
    pub internal_ip: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateService {
    pub spec: Option<Value>,
    pub server_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct EnvVarView {
    pub key: String,
    pub is_build_secret: bool,
    /// When the value was last written.
    pub updated_at: DateTime<Utc>,
    /// Characters in the saved value, so a typo or a truncated paste shows without revealing it.
    pub length: i32,
}

#[derive(Deserialize, ToSchema)]
pub struct ImportEnv {
    /// The text of a `.env` file: `KEY=value` lines, `#` comments, quotes.
    pub text: String,
    #[serde(default)]
    pub is_build_secret: bool,
    /// Only report what would change; save nothing.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, ToSchema)]
pub struct ImportEnvEntry {
    pub key: String,
    /// `new`, `changed`, or `unchanged` compared with what is saved.
    pub status: String,
    pub length: i32,
}

#[derive(Serialize, ToSchema)]
pub struct ImportEnvError {
    pub line: i32,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct ImportEnvResult {
    pub entries: Vec<ImportEnvEntry>,
    pub errors: Vec<ImportEnvError>,
    /// The values were saved. False for a dry run or when the text has errors.
    pub applied: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct VerifyEnvVar {
    pub value: String,
    #[serde(default)]
    pub is_build_secret: bool,
}

#[derive(Serialize, ToSchema)]
pub struct VerifyEnvResult {
    /// The value typed matches the saved one exactly.
    pub matches: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct PutEnvVar {
    pub key: String,
    pub value: String,
    pub is_build_secret: bool,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/v1/servers", get(list_servers).post(create_server))
        .route(
            "/api/v1/servers/{id}",
            axum::routing::patch(update_server).delete(delete_server),
        )
        .route(
            "/api/v1/servers/{id}/probe",
            axum::routing::post(probe_server),
        )
        .route("/api/v1/projects", get(list_projects).post(create_project))
        .route(
            "/api/v1/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route(
            "/api/v1/projects/{id}/environments",
            get(list_environments).post(create_environment),
        )
        .route(
            "/api/v1/environments/{id}",
            axum::routing::delete(delete_environment),
        )
        .route(
            "/api/v1/environments/{id}/services",
            get(list_services).post(create_service),
        )
        .route("/api/v1/services", get(list_all_services))
        .route(
            "/api/v1/services/{id}",
            get(get_service)
                .patch(update_service)
                .delete(delete_service),
        )
        .route(
            "/api/v1/services/{id}/network",
            axum::routing::patch(update_network),
        )
        .route("/api/v1/services/{id}/env", get(list_env).put(put_env))
        .route("/api/v1/services/{id}/env/import", post(import_env))
        .route("/api/v1/services/{id}/env/{key}/verify", post(verify_env))
        .route(
            "/api/v1/services/{id}/env/{key}",
            axum::routing::delete(delete_env),
        )
}

async fn list_servers(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<Vec<ServerView>>, ApiError> {
    let rows = sqlx::query_as!(
        ServerRow,
        r#"
        SELECT id, name, kind, status, public_ip, description, agent_version, docker_version,
               os_info, cpu_count, memory_bytes, last_heartbeat_at, created_at,
                  auto_prune, last_pruned_at, hostname, enrolled_at, telemetry_dropped
        FROM servers
        ORDER BY created_at
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(rows.into_iter().map(server_from_row).collect()))
}

async fn create_server(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateServer>,
) -> Result<(StatusCode, Json<CreatedServer>), ApiError> {
    auth.require_write()?;
    validate_name(&body.name).map_err(core_error)?;
    if body.kind != "local" && body.kind != "remote" {
        return Err(bad("invalid_kind", "kind must be local or remote"));
    }
    if body.kind == "local" {
        let existing =
            sqlx::query_scalar!(r#"SELECT COUNT(*) AS "count!" FROM servers WHERE kind = 'local'"#)
                .fetch_one(&state.pool)
                .await
                .map_err(ApiError::database)?;
        if existing > 0 {
            return Err(conflict("local_exists", "the local server already exists"));
        }
    }
    let (join_hash, join_expires, join_token) = if body.kind == "remote" {
        let token = auth::random_token("join");
        (
            Some(auth::sha256_hex(&token)),
            Some(Utc::now() + Duration::hours(1)),
            Some(token),
        )
    } else {
        (None, None, None)
    };
    let description = body.description.unwrap_or_default();
    let inserted = sqlx::query_as!(
        ServerRow,
        r#"
        INSERT INTO servers (
            name, kind, status, public_ip, description, join_token_hash, join_token_expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id, name, kind, status, public_ip, description, agent_version, docker_version,
                  os_info, cpu_count, memory_bytes, last_heartbeat_at, created_at,
                  auto_prune, last_pruned_at, hostname, enrolled_at, telemetry_dropped
        "#,
        body.name,
        body.kind,
        if body.kind == "remote" {
            "pending"
        } else {
            "unknown"
        },
        body.public_ip,
        description,
        join_hash,
        join_expires,
    )
    .fetch_one(&state.pool)
    .await;
    let row = match inserted {
        Ok(row) => row,
        Err(error) if is_unique(&error) => {
            return Err(conflict("name_taken", "a server with that name exists"));
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    audit::record(
        &state.pool,
        Some(auth.id),
        "server.create",
        "server",
        Some(&row.id.to_string()),
        json!({ "kind": row.kind }),
        None,
    )
    .await;
    let command = join_token
        .as_ref()
        .map(|token| crate::routes::agents::join_command(&state, &headers, token));
    Ok((
        StatusCode::CREATED,
        Json(CreatedServer {
            server: server_from_row(row),
            join_token,
            join_command: command,
        }),
    ))
}

async fn update_server(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateServer>,
) -> Result<Json<ServerView>, ApiError> {
    auth.require_write()?;
    if let Some(name) = &body.name {
        validate_name(name).map_err(core_error)?;
    }
    let row = sqlx::query_as!(
        ServerRow,
        r#"
        UPDATE servers
        SET name = COALESCE($2, name),
            public_ip = COALESCE($3, public_ip),
            description = COALESCE($4, description),
            auto_prune = COALESCE($5, auto_prune),
            updated_at = now()
        WHERE id = $1
        RETURNING id, name, kind, status, public_ip, description, agent_version, docker_version,
                  os_info, cpu_count, memory_bytes, last_heartbeat_at, created_at,
                  auto_prune, last_pruned_at, hostname, enrolled_at, telemetry_dropped
        "#,
        id,
        body.name,
        body.public_ip,
        body.description,
        body.auto_prune,
    )
    .fetch_optional(&state.pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return Err(not_found("server not found")),
        Err(error) if is_unique(&error) => {
            return Err(conflict("name_taken", "a server with that name exists"));
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    audit::record(
        &state.pool,
        Some(auth.id),
        "server.update",
        "server",
        Some(&id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok(Json(server_from_row(row)))
}

async fn delete_server(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    let row = sqlx::query!(r#"SELECT kind FROM servers WHERE id = $1"#, id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(not_found("server not found"));
    };
    if row.kind == "local" {
        return Err(conflict(
            "local_server",
            "the local server cannot be removed",
        ));
    }
    let services = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM services WHERE server_id = $1"#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if services > 0 {
        return Err(conflict(
            "server_in_use",
            "move or delete services before removing this server",
        ));
    }
    sqlx::query!(r#"DELETE FROM servers WHERE id = $1"#, id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    state.agents.disconnect(id);
    audit::record(
        &state.pool,
        Some(auth.id),
        "server.delete",
        "server",
        Some(&id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn probe_server(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ServerView>, ApiError> {
    auth.require_write()?;
    Ok(Json(probe(&state, id).await?))
}

/// Ask the server's Docker for its version and capacity and mark it online.
pub(crate) async fn probe(state: &AppState, id: Uuid) -> Result<ServerView, ApiError> {
    let info = crate::nodes::driver_for(state, id)
        .await?
        .docker_info()
        .await
        .map_err(core_error)?;
    let row = sqlx::query_as!(
        ServerRow,
        r#"
        UPDATE servers
        SET status = 'online',
            docker_version = $2,
            os_info = $3,
            cpu_count = $4,
            memory_bytes = $5,
            last_heartbeat_at = now(),
            updated_at = now()
        WHERE id = $1
        RETURNING id, name, kind, status, public_ip, description, agent_version, docker_version,
                  os_info, cpu_count, memory_bytes, last_heartbeat_at, created_at,
                  auto_prune, last_pruned_at, hostname, enrolled_at, telemetry_dropped
        "#,
        id,
        info.version,
        info.os,
        i32::try_from(info.cpus).unwrap_or(i32::MAX),
        info.memory_bytes,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(server_from_row(row))
}

async fn list_projects(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    let rows =
        sqlx::query!(r#"SELECT id, name, description, created_at FROM projects ORDER BY name"#)
            .fetch_all(&state.pool)
            .await
            .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| ProjectView {
                id: row.id,
                name: row.name,
                description: row.description,
                created_at: row.created_at,
            })
            .collect(),
    ))
}

async fn create_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(body): Json<CreateProject>,
) -> Result<(StatusCode, Json<ProjectView>), ApiError> {
    auth.require_write()?;
    validate_name(&body.name).map_err(core_error)?;
    let description = body.description.unwrap_or_default();
    let inserted = sqlx::query!(
        r#"
        INSERT INTO projects (name, description)
        VALUES ($1, $2)
        RETURNING id, name, description, created_at
        "#,
        body.name,
        description,
    )
    .fetch_one(&state.pool)
    .await;
    let row = match inserted {
        Ok(row) => row,
        Err(error) if is_unique(&error) => {
            return Err(conflict("name_taken", "a project with that name exists"));
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    audit::record(
        &state.pool,
        Some(auth.id),
        "project.create",
        "project",
        Some(&row.id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(ProjectView {
            id: row.id,
            name: row.name,
            description: row.description,
            created_at: row.created_at,
        }),
    ))
}

async fn get_project(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ProjectView>, ApiError> {
    let row = sqlx::query!(
        r#"SELECT id, name, description, created_at FROM projects WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(not_found("project not found"));
    };
    Ok(Json(ProjectView {
        id: row.id,
        name: row.name,
        description: row.description,
        created_at: row.created_at,
    }))
}

async fn update_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateProject>,
) -> Result<Json<ProjectView>, ApiError> {
    auth.require_write()?;
    validate_name(&body.name).map_err(core_error)?;
    let description = body.description.unwrap_or_default();
    let updated = sqlx::query!(
        r#"
        UPDATE projects SET name = $2, description = $3, updated_at = now()
        WHERE id = $1
        RETURNING id, name, description, created_at
        "#,
        id,
        body.name,
        description,
    )
    .fetch_optional(&state.pool)
    .await;
    let row = match updated {
        Ok(Some(row)) => row,
        Ok(None) => return Err(not_found("project not found")),
        Err(error) if is_unique(&error) => {
            return Err(conflict("name_taken", "a project with that name exists"));
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    Ok(Json(ProjectView {
        id: row.id,
        name: row.name,
        description: row.description,
        created_at: row.created_at,
    }))
}

async fn delete_project(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    let exists = sqlx::query_scalar::<_, Uuid>("SELECT id FROM projects WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if exists.is_none() {
        return Err(not_found("project not found"));
    }
    let servers = teardown_services_of(&state, id, None).await?;
    sqlx::query!(r#"DELETE FROM projects WHERE id = $1"#, id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    republish(&state, servers).await;
    audit::record(
        &state.pool,
        Some(auth.id),
        "project.delete",
        "project",
        Some(&id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_environments(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<EnvironmentView>>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT id, project_id, name, description
        FROM environments
        WHERE project_id = $1
        ORDER BY name
        "#,
        id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| EnvironmentView {
                id: row.id,
                project_id: row.project_id,
                name: row.name,
                description: row.description,
            })
            .collect(),
    ))
}

async fn create_environment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateEnvironment>,
) -> Result<(StatusCode, Json<EnvironmentView>), ApiError> {
    auth.require_write()?;
    validate_name(&body.name).map_err(core_error)?;
    let description = body.description.unwrap_or_default();
    let inserted = sqlx::query!(
        r#"
        INSERT INTO environments (project_id, name, description)
        VALUES ($1, $2, $3)
        RETURNING id, project_id, name, description
        "#,
        project_id,
        body.name,
        description,
    )
    .fetch_one(&state.pool)
    .await;
    let row = match inserted {
        Ok(row) => row,
        Err(error) if is_unique(&error) => {
            return Err(conflict("name_taken", "that environment already exists"));
        }
        Err(error) if is_fk(&error) => return Err(not_found("project not found")),
        Err(error) => return Err(ApiError::database(error)),
    };
    audit::record(
        &state.pool,
        Some(auth.id),
        "environment.create",
        "environment",
        Some(&row.id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(EnvironmentView {
            id: row.id,
            project_id: row.project_id,
            name: row.name,
            description: row.description,
        }),
    ))
}

async fn delete_environment(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    let project_id =
        sqlx::query_scalar::<_, Uuid>("SELECT project_id FROM environments WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(ApiError::database)?
            .ok_or_else(|| not_found("environment not found"))?;
    let servers = teardown_services_of(&state, project_id, Some(id)).await?;
    sqlx::query!(r#"DELETE FROM environments WHERE id = $1"#, id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    republish(&state, servers).await;
    audit::record(
        &state.pool,
        Some(auth.id),
        "environment.delete",
        "environment",
        Some(&id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_services(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(environment_id): Path<Uuid>,
) -> Result<Json<Vec<ServiceView>>, ApiError> {
    let rows = sqlx::query_as!(
        ServiceRow,
        r#"
        SELECT id, environment_id, server_id, name, kind, status, spec, created_at
        FROM services
        WHERE environment_id = $1
        ORDER BY name
        "#,
        environment_id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(rows.into_iter().map(service_from_row).collect()))
}

async fn list_all_services(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Result<Json<Vec<ServiceSummary>>, ApiError> {
    let rows = sqlx::query_as!(
        ServiceSummary,
        r#"
        SELECT s.id, s.name, s.kind, s.status, s.server_id, s.environment_id,
               e.name AS environment_name, p.id AS project_id, p.name AS project_name
        FROM services s
        JOIN environments e ON e.id = s.environment_id
        JOIN projects p ON p.id = e.project_id
        ORDER BY p.name, e.name, s.name
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(rows))
}

async fn create_service(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(environment_id): Path<Uuid>,
    Json(body): Json<CreateService>,
) -> Result<(StatusCode, Json<ServiceView>), ApiError> {
    auth.require_write()?;
    validate_name(&body.name).map_err(core_error)?;
    check_kind(&body.kind)?;
    let spec = checked_spec(body.spec.unwrap_or_else(|| json!({})))?;
    let inserted = sqlx::query_as!(
        ServiceRow,
        r#"
        INSERT INTO services (environment_id, server_id, name, kind, spec)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, environment_id, server_id, name, kind, status, spec, created_at
        "#,
        environment_id,
        body.server_id,
        body.name,
        body.kind,
        spec,
    )
    .fetch_one(&state.pool)
    .await;
    let row = match inserted {
        Ok(row) => row,
        Err(error) if is_unique(&error) => {
            return Err(conflict("name_taken", "that service already exists"));
        }
        Err(error) if is_fk(&error) => {
            return Err(not_found("environment or server not found"));
        }
        Err(error) => return Err(ApiError::database(error)),
    };
    audit::record(
        &state.pool,
        Some(auth.id),
        "service.create",
        "service",
        Some(&row.id.to_string()),
        json!({ "kind": row.kind }),
        None,
    )
    .await;
    Ok((StatusCode::CREATED, Json(service_from_row(row))))
}

async fn get_service(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<ServiceView>, ApiError> {
    let row = sqlx::query_as!(
        ServiceRow,
        r#"
        SELECT id, environment_id, server_id, name, kind, status, spec, created_at
        FROM services WHERE id = $1
        "#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(not_found("service not found"));
    };
    Ok(Json(service_from_row(row)))
}

async fn update_network(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateNetwork>,
) -> Result<Json<NetworkView>, ApiError> {
    auth.require_write()?;
    let row = sqlx::query!(r#"SELECT kind, spec FROM services WHERE id = $1"#, id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?
        .ok_or_else(|| not_found("service not found"))?;
    if row.kind == "compose" {
        let mut spec: crate::stacks::StackSpec = serde_json::from_value(row.spec)
            .map_err(|_| bad("invalid_spec", "stored stack spec is not valid"))?;
        spec.network_isolation = Some(body.isolation);
        let routed = crate::stacks::routed_services(&state, id, &spec).await?;
        crate::stacks::replan(&mut spec, &routed)?;
        crate::stacks::save(&state, id, &spec).await?;
        return Ok(Json(NetworkView {
            isolation: body.isolation,
            internal_ip: None,
        }));
    }
    let spec = json!({ "network_isolation": body.isolation });
    sqlx::query!(
        r#"UPDATE services SET spec = spec || $2, updated_at = now() WHERE id = $1"#,
        id,
        spec,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let internal_ip = row
        .spec
        .get("internal_ip")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Json(NetworkView {
        isolation: body.isolation,
        internal_ip,
    }))
}

async fn update_service(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateService>,
) -> Result<Json<ServiceView>, ApiError> {
    auth.require_write()?;
    if let Some(spec) = &body.spec {
        checked_spec(spec.clone())?;
    }
    let row = sqlx::query_as!(
        ServiceRow,
        r#"
        UPDATE services
        SET spec = COALESCE($2, spec),
            server_id = COALESCE($3, server_id),
            updated_at = now()
        WHERE id = $1
        RETURNING id, environment_id, server_id, name, kind, status, spec, created_at
        "#,
        id,
        body.spec,
        body.server_id,
    )
    .fetch_optional(&state.pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return Err(not_found("service not found")),
        Err(error) if is_fk(&error) => return Err(not_found("server not found")),
        Err(error) => return Err(ApiError::database(error)),
    };
    Ok(Json(service_from_row(row)))
}

async fn delete_service(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    let server_id = teardown_service(&state, id)
        .await?
        .ok_or_else(|| not_found("service not found"))?;
    sqlx::query!(r#"DELETE FROM services WHERE id = $1"#, id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if let Err(error) = crate::publish::publish_server(&state, server_id).await {
        tracing::warn!(message = %error.public_message(), "routes were not republished");
    }
    audit::record(
        &state.pool,
        Some(auth.id),
        "service.delete",
        "service",
        Some(&id.to_string()),
        json!({}),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes everything a service runs (its stack or containers) and returns the server it
/// ran on, or `None` when the service does not exist. The database row stays.
async fn teardown_service(state: &AppState, id: Uuid) -> Result<Option<Uuid>, ApiError> {
    let Some(service) = sqlx::query!(
        r#"SELECT kind, server_id, current_container_id FROM services WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    else {
        return Ok(None);
    };
    if service.kind == "compose" {
        crate::stacks::destroy(state, id, false).await?;
    } else {
        remove_service_containers(state, id, service.server_id, service.current_container_id).await;
    }
    Ok(Some(service.server_id))
}

/// Tears down every service of an environment, or of a whole project when `environment_id`
/// is `None`, so that deleting the parent row (which cascades to the services) does not leave
/// containers behind. Returns the servers that hosted them, to republish routes afterwards.
async fn teardown_services_of(
    state: &AppState,
    project_id: Uuid,
    environment_id: Option<Uuid>,
) -> Result<Vec<Uuid>, ApiError> {
    let ids: Vec<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT s.id FROM services s
         JOIN environments e ON e.id = s.environment_id
         WHERE e.project_id = $1 AND ($2::uuid IS NULL OR e.id = $2)",
    )
    .bind(project_id)
    .bind(environment_id)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let mut servers = Vec::new();
    for id in ids {
        if let Some(server_id) = teardown_service(state, id).await? {
            if !servers.contains(&server_id) {
                servers.push(server_id);
            }
        }
    }
    Ok(servers)
}

async fn republish(state: &AppState, servers: Vec<Uuid>) {
    for server_id in servers {
        if let Err(error) = crate::publish::publish_server(state, server_id).await {
            tracing::warn!(message = %error.public_message(), "routes were not republished");
        }
    }
}

/// Best effort: a server that is offline or already gone must not block the delete.
async fn remove_service_containers(
    state: &AppState,
    id: Uuid,
    server_id: Uuid,
    current: Option<String>,
) {
    let driver = match crate::nodes::driver_for(state, server_id).await {
        Ok(driver) => driver,
        Err(error) => {
            tracing::warn!(message = %error.public_message(), service_id = %id, "containers of the deleted service were not removed");
            return;
        }
    };
    let label = id.to_string();
    let mut containers: Vec<String> = match driver.list_containers().await {
        Ok(list) => list
            .into_iter()
            .filter(|c| c.labels.get("rudder.service") == Some(&label))
            .map(|c| c.id)
            .collect(),
        Err(error) => {
            tracing::warn!(%error, service_id = %id, "could not list containers of the deleted service");
            Vec::new()
        }
    };
    if let Some(current) = current {
        if !containers
            .iter()
            .any(|c| c.starts_with(&current) || current.starts_with(c.as_str()))
        {
            containers.push(current);
        }
    }
    for container in containers {
        if let Err(error) = driver.remove_container(&container, true).await {
            tracing::warn!(%error, service_id = %id, container, "container of the deleted service was not removed");
        }
    }
}

async fn list_env(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<EnvVarView>>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT key, is_build_secret, value_enc, updated_at
        FROM env_vars
        WHERE service_id = $1
        ORDER BY key
        "#,
        id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| EnvVarView {
                key: row.key,
                is_build_secret: row.is_build_secret,
                updated_at: row.updated_at,
                length: saved_length(&state, &row.value_enc),
            })
            .collect(),
    ))
}

/// Characters in a stored value; zero when it cannot be read.
fn saved_length(state: &AppState, encrypted: &[u8]) -> i32 {
    state
        .cipher
        .decrypt(encrypted)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map_or(0, |value| {
            i32::try_from(value.chars().count()).unwrap_or(i32::MAX)
        })
}

/// Preview or save many variables from the text of a `.env` file.
async fn import_env(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ImportEnv>,
) -> Result<Json<ImportEnvResult>, ApiError> {
    auth.require_write()?;
    let exists = sqlx::query_scalar!(r#"SELECT id FROM services WHERE id = $1"#, id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if exists.is_none() {
        return Err(not_found("service not found"));
    }
    let parse = rudder_core::parse_env_text(&body.text);
    let saved = sqlx::query!(
        r#"
        SELECT key, value_enc
        FROM env_vars
        WHERE service_id = $1 AND is_build_secret = $2
        "#,
        id,
        body.is_build_secret
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let mut current: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for row in saved {
        if let Ok(bytes) = state.cipher.decrypt(&row.value_enc) {
            current.insert(row.key, bytes);
        }
    }
    let entries: Vec<ImportEnvEntry> = parse
        .entries
        .iter()
        .map(|entry| ImportEnvEntry {
            key: entry.key.clone(),
            status: match current.get(&entry.key) {
                None => "new",
                Some(old) if old.as_slice() == entry.value.as_bytes() => "unchanged",
                Some(_) => "changed",
            }
            .to_owned(),
            length: i32::try_from(entry.value.chars().count()).unwrap_or(i32::MAX),
        })
        .collect();
    let errors: Vec<ImportEnvError> = parse
        .errors
        .iter()
        .map(|error| ImportEnvError {
            line: i32::try_from(error.line).unwrap_or(i32::MAX),
            message: error.message.clone(),
        })
        .collect();
    if body.dry_run || !errors.is_empty() {
        if !body.dry_run {
            let first = &errors[0];
            return Err(bad(
                "invalid_env",
                format!("line {}: {}; nothing was saved", first.line, first.message),
            ));
        }
        return Ok(Json(ImportEnvResult {
            entries,
            errors,
            applied: false,
        }));
    }
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    for (entry, view) in parse.entries.iter().zip(&entries) {
        if view.status == "unchanged" {
            continue;
        }
        let encrypted = state.cipher.encrypt(entry.value.as_bytes()).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "crypto_error",
                "could not store environment value",
            )
        })?;
        sqlx::query!(
            r#"
            INSERT INTO env_vars (service_id, key, value_enc, is_build_secret)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (service_id, key, is_build_secret)
            DO UPDATE SET value_enc = EXCLUDED.value_enc, updated_at = now()
            "#,
            id,
            entry.key,
            encrypted.as_slice(),
            body.is_build_secret,
        )
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
    }
    tx.commit().await.map_err(ApiError::database)?;
    let changed: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.status != "unchanged")
        .map(|entry| entry.key.as_str())
        .collect();
    audit::record(
        &state.pool,
        Some(auth.id),
        "service.env_import",
        "service",
        Some(&id.to_string()),
        json!({ "keys": changed, "build_secret": body.is_build_secret }),
        None,
    )
    .await;
    Ok(Json(ImportEnvResult {
        entries,
        errors,
        applied: true,
    }))
}

/// Say whether a typed value equals the saved one, without ever returning the saved one.
async fn verify_env(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, key)): Path<(Uuid, String)>,
    Json(body): Json<VerifyEnvVar>,
) -> Result<Json<VerifyEnvResult>, ApiError> {
    auth.require_write()?;
    let saved = sqlx::query_scalar!(
        r#"
        SELECT value_enc
        FROM env_vars
        WHERE service_id = $1 AND key = $2 AND is_build_secret = $3
        "#,
        id,
        key,
        body.is_build_secret
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("that variable is not saved"))?;
    let matches = state
        .cipher
        .decrypt(&saved)
        .map(|bytes| bool::from(bytes.as_slice().ct_eq(body.value.as_bytes())))
        .unwrap_or(false);
    Ok(Json(VerifyEnvResult { matches }))
}

async fn put_env(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<PutEnvVar>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    validate_env_key(&body.key).map_err(core_error)?;
    if body.value.contains('\0') || body.value.len() > 16_384 {
        return Err(bad(
            "invalid_value",
            "environment value cannot contain NUL and must be at most 16 KiB",
        ));
    }
    let encrypted = state.cipher.encrypt(body.value.as_bytes()).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "could not store environment value",
        )
    })?;
    let inserted = sqlx::query!(
        r#"
        INSERT INTO env_vars (service_id, key, value_enc, is_build_secret)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (service_id, key, is_build_secret)
        DO UPDATE SET value_enc = EXCLUDED.value_enc, updated_at = now()
        "#,
        id,
        body.key,
        encrypted.as_slice(),
        body.is_build_secret,
    )
    .execute(&state.pool)
    .await;
    match inserted {
        Ok(_) => {}
        Err(error) if is_fk(&error) => return Err(not_found("service not found")),
        Err(error) => return Err(ApiError::database(error)),
    }
    audit::record(
        &state.pool,
        Some(auth.id),
        "service.env_set",
        "service",
        Some(&id.to_string()),
        json!({ "key": body.key, "build_secret": body.is_build_secret }),
        None,
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_env(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((id, key)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    sqlx::query!(
        r#"DELETE FROM env_vars WHERE service_id = $1 AND key = $2"#,
        id,
        key
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct ServerRow {
    id: Uuid,
    name: String,
    kind: String,
    status: String,
    public_ip: Option<String>,
    description: String,
    agent_version: Option<String>,
    docker_version: Option<String>,
    os_info: Option<String>,
    cpu_count: Option<i32>,
    memory_bytes: Option<i64>,
    last_heartbeat_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    auto_prune: bool,
    last_pruned_at: Option<DateTime<Utc>>,
    hostname: Option<String>,
    enrolled_at: Option<DateTime<Utc>>,
    telemetry_dropped: i64,
}

fn server_from_row(row: ServerRow) -> ServerView {
    ServerView {
        id: row.id,
        name: row.name,
        kind: row.kind,
        status: row.status,
        public_ip: row.public_ip,
        description: row.description,
        agent_version: row.agent_version,
        docker_version: row.docker_version,
        os_info: row.os_info,
        cpu_count: row.cpu_count,
        memory_bytes: row.memory_bytes,
        last_heartbeat_at: row.last_heartbeat_at,
        created_at: row.created_at,
        auto_prune: row.auto_prune,
        last_pruned_at: row.last_pruned_at,
        hostname: row.hostname,
        enrolled_at: row.enrolled_at,
        telemetry_dropped: row.telemetry_dropped,
    }
}

#[derive(sqlx::FromRow)]
struct ServiceRow {
    id: Uuid,
    environment_id: Uuid,
    server_id: Uuid,
    name: String,
    kind: String,
    status: String,
    spec: Value,
    created_at: DateTime<Utc>,
}

fn service_from_row(row: ServiceRow) -> ServiceView {
    ServiceView {
        id: row.id,
        environment_id: row.environment_id,
        server_id: row.server_id,
        name: row.name,
        kind: row.kind,
        status: row.status,
        spec: row.spec,
        created_at: row.created_at,
    }
}

pub(crate) async fn load_service_view(state: &AppState, id: Uuid) -> Result<ServiceView, ApiError> {
    let row = sqlx::query_as!(
        ServiceRow,
        r#"
        SELECT id, environment_id, server_id, name, kind, status, spec, created_at
        FROM services WHERE id = $1
        "#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("service not found"))?;
    Ok(service_from_row(row))
}

fn checked_spec(spec: Value) -> Result<Value, ApiError> {
    if !spec.is_object() {
        return Err(bad("invalid_spec", "service spec must be a JSON object"));
    }
    if spec.to_string().len() > 65_536 {
        return Err(bad("invalid_spec", "service spec is too large"));
    }
    Ok(spec)
}

fn check_kind(kind: &str) -> Result<(), ApiError> {
    if matches!(kind, "application" | "database" | "compose") {
        Ok(())
    } else {
        Err(bad(
            "invalid_kind",
            "kind must be application, database, or compose",
        ))
    }
}

pub(crate) fn core_error(error: CoreError) -> ApiError {
    match error {
        CoreError::InvalidName(name) => bad("invalid_name", format!("invalid name '{name}'")),
        CoreError::Validation(message) => bad("invalid", message),
        CoreError::Node(message) => ApiError::new(StatusCode::BAD_GATEWAY, "node", message),
        CoreError::Crypto => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "crypto failure",
        ),
    }
}

pub(crate) fn bad(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code, message)
}

pub(crate) fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

pub(crate) fn not_found(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", message)
}

pub(crate) fn is_unique(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_unique_violation())
}

pub(crate) fn is_fk(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.is_foreign_key_violation())
}
