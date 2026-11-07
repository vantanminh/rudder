//! Image rollout on the local Docker engine.
//!
//! Registry passwords and environment values stay in memory for the pull and
//! the container create call. They are not written to deployment logs.

use chrono::{DateTime, Utc};
use rudder_core::{
    container_name, image_registry, rollout, select_registry, validate_image_ref, HealthKind,
    HealthSpec, NodeDriver, PortMapping, RegistryAuth, RegistryRecord, RolloutPlan, VolumeMount,
    PROXY_NETWORK,
};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::audit;
use crate::error::ApiError;
use crate::publish::publish_server;
use crate::routes::catalog::{bad, conflict, core_error, not_found};
use crate::state::AppState;

pub struct DeployParams {
    pub service_id: Uuid,
    pub image: Option<String>,
    pub registry_id: Option<Uuid>,
    pub health: HealthSpec,
    pub trigger: String,
    pub actor: Option<Uuid>,
    pub watch_tag: Option<bool>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct DeploymentView {
    pub id: Uuid,
    pub service_id: Uuid,
    pub status: String,
    pub trigger: String,
    pub image_ref: Option<String>,
    pub error: Option<String>,
    /// The end of the log (at most 200 000 characters); see `logs_truncated`.
    pub logs: String,
    /// The log is longer than `logs`; the whole text is at
    /// `/api/v1/services/{id}/deployments/{deployment}/logs`.
    pub logs_truncated: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

struct ServiceDeployRow {
    id: Uuid,
    name: String,
    kind: String,
    server_id: Uuid,
    spec: Value,
    status: String,
    current_container_id: Option<String>,
    current_image: Option<String>,
}

struct DeploymentRow {
    id: Uuid,
    service_id: Uuid,
    status: String,
    trigger: String,
    image_ref: Option<String>,
    error: Option<String>,
    logs: String,
    logs_truncated: bool,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

struct RegistrySecret {
    id: Uuid,
    name: String,
    url: String,
    username: Option<String>,
    password_enc: Option<Vec<u8>>,
}

#[derive(Clone)]
struct SavedContainer {
    container_id: String,
    container_name: String,
    image: String,
    generation: i32,
    deployment_id: Uuid,
    watch_tag: Option<bool>,
}

pub async fn deploy_service(
    state: &AppState,
    params: DeployParams,
) -> Result<DeploymentView, ApiError> {
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    let service = sqlx::query_as!(
        ServiceDeployRow,
        r#"
        SELECT id, name, kind, server_id, spec, status, current_container_id, current_image
        FROM services
        WHERE id = $1
        FOR UPDATE
        "#,
        params.service_id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    let Some(service) = service else {
        return Err(not_found("service not found"));
    };
    if service.status == "deploying" || service.status == "building" {
        return Err(conflict(
            "deploy_in_progress",
            "a deployment is already running for this service",
        ));
    }
    if service.kind != "application" {
        return Err(bad(
            "invalid_kind",
            "image deploy is available for application services",
        ));
    }
    let server_kind = sqlx::query_scalar!(
        r#"SELECT kind FROM servers WHERE id = $1"#,
        service.server_id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("server not found"))?;
    if server_kind != "local" && state.agents.session(service.server_id).is_none() {
        return Err(conflict(
            "remote_unavailable",
            "the agent for this server is not connected",
        ));
    }
    sqlx::query!(
        r#"UPDATE services SET status = 'deploying', updated_at = now() WHERE id = $1"#,
        service.id
    )
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    tx.commit().await.map_err(ApiError::database)?;

    let image = params
        .image
        .clone()
        .or_else(|| {
            service
                .spec
                .get("image")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if let Err(error) = validate_image_ref(&image) {
        mark_failed(state, service.id, None, &error.to_string()).await;
        return Err(core_error(error));
    }
    let generation = next_generation(state, service.id).await?;
    let name = match container_name(&service.name, generation) {
        Ok(name) => name,
        Err(error) => {
            mark_failed(state, service.id, None, &error.to_string()).await;
            return Err(core_error(error));
        }
    };
    let deployment_id = match insert_deployment(state, service.id, &params.trigger, &image).await {
        Ok(id) => id,
        Err(error) => {
            mark_failed(state, service.id, None, "could not record the deployment").await;
            return Err(error);
        }
    };
    let auth = match registry_auth(state, &image, params.registry_id).await {
        Ok(auth) => auth,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    let env = match runtime_env(state, service.id).await {
        Ok(env) => access_env(state, service.id, env).await,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    let resources = match resources_from_spec(&service.spec) {
        Ok(resources) => resources,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    let plan = RolloutPlan {
        container_name: name.clone(),
        image: image.clone(),
        env,
        command: resources.command,
        ports: rudder_core::isolated_ports(
            resources.ports,
            rudder_core::network_isolation_enabled(&service.spec),
        ),
        volumes: resources.volumes,
        memory_bytes: resources.memory_bytes,
        nano_cpus: resources.nano_cpus,
        restart: resources.restart,
        network: Some(PROXY_NETWORK.into()),
        labels: vec![
            ("rudder.managed".into(), "true".into()),
            ("rudder.service".into(), service.id.to_string()),
        ],
        auth,
        health: params.health.clone(),
        skip_pull: false,
    };
    let previous_id = service.current_container_id.clone();
    let previous_image = service.current_image.clone();
    let previous_status = service.status.clone();
    let switch_saved = SavedContainer {
        container_id: String::new(),
        container_name: name.clone(),
        image: image.clone(),
        generation,
        deployment_id,
        watch_tag: params.watch_tag,
    };
    let driver = match crate::nodes::driver_for(state, service.server_id).await {
        Ok(driver) => driver,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    let switch_state = state.clone();
    let outcome = rollout(driver.as_ref(), &plan, previous_id.as_deref(), |started| {
        let container_id = started.id.clone();
        let state = switch_state.clone();
        let mut saved = switch_saved.clone();
        let previous_id = previous_id.clone();
        let previous_image = previous_image.clone();
        let previous_status = previous_status.clone();
        let server_id = service.server_id;
        let service_id = service.id;
        async move {
            saved.container_id = container_id.clone();
            record_running(&state, service_id, &saved)
                .await
                .map_err(|error| rudder_core::Error::Node(error.public_message()))?;
            if let Err(error) = publish_server(&state, server_id).await {
                restore_previous(
                    &state,
                    service_id,
                    previous_id.as_deref(),
                    previous_image.as_deref(),
                    &previous_status,
                    &container_id,
                )
                .await;
                return Err(rudder_core::Error::Node(error.public_message()));
            }
            Ok(())
        }
    })
    .await;

    match outcome {
        Ok(output) => {
            let mut logs = format!(
                "pulled {image}\nstarted {}\nrouting updated\n",
                output.container_name
            );
            if let Some(error) = &output.retire_error {
                let _ = std::fmt::Write::write_fmt(
                    &mut logs,
                    format_args!("previous container was left running: {error}\n"),
                );
            } else if previous_id.is_some() {
                logs.push_str("previous container stopped\n");
            }
            let _ =
                note_internal_ip(driver.as_ref(), state, service.id, &output.container_id).await;
            if let Ok(digest) = driver.image_digest(&image).await {
                let _ = sqlx::query!(
                    r#"UPDATE services SET current_digest = $2, updated_at = now() WHERE id = $1"#,
                    service.id,
                    digest
                )
                .execute(&state.pool)
                .await;
            }
            finish_deployment(state, deployment_id, "succeeded", None, &logs).await?;
            let _ = sqlx::query!(
                r#"
                UPDATE services
                SET status = 'running', updated_at = now()
                WHERE id = $1
                "#,
                service.id
            )
            .execute(&state.pool)
            .await;
            audit::record(
                &state.pool,
                params.actor,
                "service.deploy",
                "service",
                Some(&service.id.to_string()),
                json!({ "image": image, "trigger": params.trigger, "deployment_id": deployment_id }),
                None,
            )
            .await;
            load_deployment(state, deployment_id).await
        }
        Err(error) => {
            let message = error.to_string();
            mark_failed(state, service.id, Some(deployment_id), &message).await;
            Err(core_error(error))
        }
    }
}

pub(crate) struct BuiltRollout {
    pub service_id: Uuid,
    pub deployment_id: Uuid,
    pub image: String,
    pub actor: Option<Uuid>,
    pub log_prefix: String,
}

/// Load a Docker image tar stream from Rudder Image store (R2/S3) onto the target server and roll it out.
pub(crate) async fn deploy_r2_stream(
    state: &AppState,
    service_id: Uuid,
    image: String,
    tar_stream: rudder_core::ByteStream,
    trigger: String,
    actor: Option<Uuid>,
) -> Result<DeploymentView, ApiError> {
    let service = sqlx::query_as!(
        ServiceDeployRow,
        r#"
        SELECT id, name, kind, server_id, spec, status, current_container_id, current_image
        FROM services
        WHERE id = $1
        "#,
        service_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("service not found"))?;
    let compose = service.kind == "compose";
    if service.kind != "application" && !compose {
        return Err(bad(
            "invalid_kind",
            "image deploy is available for application and Compose services",
        ));
    }
    let server_kind = sqlx::query_scalar!(
        r#"SELECT kind FROM servers WHERE id = $1"#,
        service.server_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("server not found"))?;
    if server_kind != "local" && state.agents.session(service.server_id).is_none() {
        return Err(conflict(
            "remote_unavailable",
            "the agent for this server is not connected",
        ));
    }
    if compose {
        // A stack has no single container to swap: load the image onto its server, then run
        // `docker compose up -d`, which recreates the services whose image changed. The
        // compose file refers to the pushed image by its reference.
        let driver = crate::nodes::driver_for(state, service.server_id).await?;
        driver.load_image(tar_stream).await.map_err(core_error)?;
        return crate::stacks::start_action(
            state,
            service_id,
            crate::stacks::StackAction::Up,
            &trigger,
        )
        .await;
    }
    let deployment_id = insert_deployment(state, service_id, &trigger, &image).await?;
    let driver = match crate::nodes::driver_for(state, service.server_id).await {
        Ok(driver) => driver,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    if let Err(error) = driver.load_image(tar_stream).await {
        let message = error.to_string();
        let logs = format!("failed to load {image} from Rudder Image store: {message}\n");
        let _ = finish_deployment(state, deployment_id, "failed", Some(&message), &logs).await;
        mark_failed(state, service.id, None, &message).await;
        return Err(core_error(error));
    }
    roll_loaded_image(
        state,
        BuiltRollout {
            service_id,
            deployment_id,
            image: image.clone(),
            actor,
            log_prefix: format!("loaded {image} from Rudder Image store\n"),
        },
        true,
    )
    .await?;
    load_deployment(state, deployment_id).await
}

/// Roll a locally built image onto an existing deployment row.
pub(crate) async fn roll_built_image(state: &AppState, roll: BuiltRollout) -> Result<(), ApiError> {
    roll_loaded_image(state, roll, false).await
}

async fn roll_loaded_image(
    state: &AppState,
    roll: BuiltRollout,
    already_on_target: bool,
) -> Result<(), ApiError> {
    let service = sqlx::query_as!(
        ServiceDeployRow,
        r#"
        SELECT id, name, kind, server_id, spec, status, current_container_id, current_image
        FROM services
        WHERE id = $1
        "#,
        roll.service_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("service not found"))?;
    if service.kind != "application" {
        return Err(bad(
            "invalid_kind",
            "git deploy is available for application services",
        ));
    }
    sqlx::query!(
        r#"UPDATE services SET status = 'deploying', updated_at = now() WHERE id = $1"#,
        service.id
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if let Err(error) = validate_image_ref(&roll.image) {
        mark_failed(
            state,
            service.id,
            Some(roll.deployment_id),
            &error.to_string(),
        )
        .await;
        return Err(core_error(error));
    }
    let generation = match next_generation(state, service.id).await {
        Ok(generation) => generation,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(roll.deployment_id),
                "could not allocate a container name",
            )
            .await;
            return Err(error);
        }
    };
    let name = match container_name(&service.name, generation) {
        Ok(name) => name,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(roll.deployment_id),
                &error.to_string(),
            )
            .await;
            return Err(core_error(error));
        }
    };
    let env = match runtime_env(state, service.id).await {
        Ok(env) => access_env(state, service.id, env).await,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(roll.deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    let resources = match resources_from_spec(&service.spec) {
        Ok(resources) => resources,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(roll.deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    let plan = RolloutPlan {
        container_name: name.clone(),
        image: roll.image.clone(),
        env,
        command: resources.command,
        ports: rudder_core::isolated_ports(
            resources.ports,
            rudder_core::network_isolation_enabled(&service.spec),
        ),
        volumes: resources.volumes,
        memory_bytes: resources.memory_bytes,
        nano_cpus: resources.nano_cpus,
        restart: resources.restart,
        network: Some(PROXY_NETWORK.into()),
        labels: vec![
            ("rudder.managed".into(), "true".into()),
            ("rudder.service".into(), service.id.to_string()),
        ],
        auth: None,
        health: service_health(state, service.id)
            .await
            .unwrap_or_else(|_| HealthSpec::none(30)),
        skip_pull: true,
    };
    let previous_id = service.current_container_id.clone();
    let previous_image = service.current_image.clone();
    let previous_status = service.status.clone();
    let switch_saved = SavedContainer {
        container_id: String::new(),
        container_name: name.clone(),
        image: roll.image.clone(),
        generation,
        deployment_id: roll.deployment_id,
        watch_tag: None,
    };
    let driver = match crate::nodes::driver_for(state, service.server_id).await {
        Ok(driver) => driver,
        Err(error) => {
            mark_failed(
                state,
                service.id,
                Some(roll.deployment_id),
                &error.public_message(),
            )
            .await;
            return Err(error);
        }
    };
    if !already_on_target && state.agents.session(service.server_id).is_some() {
        if let Err(error) =
            copy_image(state.local_driver.as_ref(), driver.as_ref(), &roll.image).await
        {
            let message = format!("could not copy the image to the server: {error}");
            mark_failed(state, service.id, Some(roll.deployment_id), &message).await;
            return Err(core_error(error));
        }
    }
    let switch_state = state.clone();
    let outcome = rollout(driver.as_ref(), &plan, previous_id.as_deref(), |started| {
        let container_id = started.id.clone();
        let state = switch_state.clone();
        let mut saved = switch_saved.clone();
        let previous_id = previous_id.clone();
        let previous_image = previous_image.clone();
        let previous_status = previous_status.clone();
        let server_id = service.server_id;
        let service_id = service.id;
        async move {
            saved.container_id = container_id.clone();
            record_running(&state, service_id, &saved)
                .await
                .map_err(|error| rudder_core::Error::Node(error.public_message()))?;
            if let Err(error) = publish_server(&state, server_id).await {
                restore_previous(
                    &state,
                    service_id,
                    previous_id.as_deref(),
                    previous_image.as_deref(),
                    &previous_status,
                    &container_id,
                )
                .await;
                return Err(rudder_core::Error::Node(error.public_message()));
            }
            Ok(())
        }
    })
    .await;
    match outcome {
        Ok(output) => {
            let mut logs = roll.log_prefix;
            let _ = std::fmt::Write::write_fmt(
                &mut logs,
                format_args!("started {}\nrouting updated\n", output.container_name),
            );
            if let Some(error) = &output.retire_error {
                let _ = std::fmt::Write::write_fmt(
                    &mut logs,
                    format_args!("previous container was left running: {error}\n"),
                );
            }
            let _ =
                note_internal_ip(driver.as_ref(), state, service.id, &output.container_id).await;
            finish_deployment(state, roll.deployment_id, "succeeded", None, &logs).await?;
            let _ = sqlx::query!(
                r#"UPDATE services SET status = 'running', updated_at = now() WHERE id = $1"#,
                service.id
            )
            .execute(&state.pool)
            .await;
            audit::record(
                &state.pool,
                roll.actor,
                "service.build",
                "service",
                Some(&service.id.to_string()),
                json!({ "image": roll.image, "deployment_id": roll.deployment_id }),
                None,
            )
            .await;
            Ok(())
        }
        Err(error) => {
            let message = error.to_string();
            let logs = format!("{}{message}\n", roll.log_prefix);
            let _ =
                finish_deployment(state, roll.deployment_id, "failed", Some(&message), &logs).await;
            mark_failed(state, service.id, None, &message).await;
            Err(core_error(error))
        }
    }
}

pub async fn list_deployments(
    state: &AppState,
    service_id: Uuid,
) -> Result<Vec<DeploymentView>, ApiError> {
    let rows = sqlx::query_as!(
        DeploymentRow,
        r#"
        SELECT id, service_id, status, trigger, image_ref, error,
               right(logs, 200000) AS "logs!", length(logs) > 200000 AS "logs_truncated!",
               started_at, finished_at, created_at
        FROM deployments
        WHERE service_id = $1
        ORDER BY created_at DESC
        LIMIT 50
        "#,
        service_id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(rows.into_iter().map(deployment_view).collect())
}

/// The whole log of one deployment of a service.
pub async fn deployment_log(
    state: &AppState,
    service_id: Uuid,
    deployment_id: Uuid,
) -> Result<String, ApiError> {
    sqlx::query_scalar!(
        r#"SELECT logs FROM deployments WHERE id = $1 AND service_id = $2"#,
        deployment_id,
        service_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("deployment not found"))
}

pub async fn rollback_image(
    state: &AppState,
    service_id: Uuid,
    deployment_id: Uuid,
    actor: Option<Uuid>,
) -> Result<DeploymentView, ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT status, image_ref
        FROM deployments
        WHERE id = $1 AND service_id = $2
        "#,
        deployment_id,
        service_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(not_found("deployment not found"));
    };
    if row.status != "succeeded" {
        return Err(bad(
            "not_successful",
            "only a successful deployment can be restored",
        ));
    }
    let Some(image) = row.image_ref else {
        return Err(bad("missing_image", "that deployment has no image"));
    };
    let health = service_health(state, service_id).await?;
    deploy_service(
        state,
        DeployParams {
            service_id,
            image: Some(image),
            registry_id: None,
            health,
            trigger: "rollback".into(),
            actor,
            watch_tag: None,
        },
    )
    .await
}

/// Built images exist on the panel host; stream them to a remote server's engine.
async fn copy_image(
    from: &dyn NodeDriver,
    to: &dyn NodeDriver,
    image: &str,
) -> Result<(), rudder_core::Error> {
    let tar = from.save_image(image).await?;
    to.load_image(tar).await
}

pub fn health_from_request(
    kind: Option<&str>,
    path: Option<&str>,
    port: Option<u16>,
    timeout_secs: Option<u64>,
) -> Result<HealthSpec, ApiError> {
    let kind = match kind.unwrap_or("none") {
        "none" => HealthKind::None,
        "docker" => HealthKind::Docker,
        "http" => HealthKind::Http,
        "tcp" => HealthKind::Tcp,
        _ => {
            return Err(bad(
                "invalid",
                "health kind must be none, docker, http, or tcp",
            ))
        }
    };
    let timeout_secs = timeout_secs.unwrap_or(30).min(600);
    let path = path.unwrap_or("/").to_owned();
    if !path.starts_with('/') || path.contains([' ', '\r', '\n']) {
        return Err(bad("invalid", "health path must start with /"));
    }
    Ok(HealthSpec {
        kind,
        timeout_secs,
        path,
        port: port.unwrap_or(80),
    })
}

/// Poll watched tags. The first digest is recorded and does not redeploy.
pub async fn watch_once(state: &AppState) -> Result<(), sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT id, server_id, current_image AS "current_image!", current_digest
        FROM services
        WHERE kind = 'application'
          AND COALESCE(spec->>'watch_tag', 'false') = 'true'
          AND current_image IS NOT NULL
          AND status <> 'deploying'
        "#
    )
    .fetch_all(&state.pool)
    .await?;
    for row in rows {
        let Ok(driver) = crate::nodes::driver_for(state, row.server_id).await else {
            continue;
        };
        let digest = match driver.image_digest(&row.current_image).await {
            Ok(digest) => digest,
            Err(error) => {
                tracing::debug!(%error, service_id = %row.id, "image digest check skipped");
                continue;
            }
        };
        if row.current_digest.as_deref() == Some(digest.as_str()) {
            continue;
        }
        if row.current_digest.is_none() {
            sqlx::query!(
                r#"UPDATE services SET current_digest = $2, updated_at = now() WHERE id = $1"#,
                row.id,
                digest
            )
            .execute(&state.pool)
            .await?;
            continue;
        }
        let health = service_health(state, row.id)
            .await
            .unwrap_or_else(|_| HealthSpec::none(30));
        if let Err(error) = deploy_service(
            state,
            DeployParams {
                service_id: row.id,
                image: Some(row.current_image),
                registry_id: None,
                health,
                trigger: "watch".into(),
                actor: None,
                watch_tag: None,
            },
        )
        .await
        {
            tracing::warn!(
                service_id = %row.id,
                message = %error.public_message(),
                "tag watch redeploy failed"
            );
        }
    }
    Ok(())
}

async fn service_health(state: &AppState, service_id: Uuid) -> Result<HealthSpec, ApiError> {
    let spec = sqlx::query_scalar!(r#"SELECT spec FROM services WHERE id = $1"#, service_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?
        .ok_or_else(|| not_found("service not found"))?;
    let health = spec.get("health");
    health_from_request(
        health
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str),
        health
            .and_then(|value| value.get("path"))
            .and_then(Value::as_str),
        health
            .and_then(|value| value.get("port"))
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok()),
        health
            .and_then(|value| value.get("timeout_secs"))
            .and_then(Value::as_u64),
    )
}

async fn next_generation(state: &AppState, service_id: Uuid) -> Result<i32, ApiError> {
    let max = sqlx::query_scalar!(
        r#"
        SELECT COALESCE(MAX(generation), 0) AS "generation!"
        FROM service_containers
        WHERE service_id = $1
        "#,
        service_id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(max + 1)
}

async fn insert_deployment(
    state: &AppState,
    service_id: Uuid,
    trigger: &str,
    image: &str,
) -> Result<Uuid, ApiError> {
    sqlx::query_scalar!(
        r#"
        INSERT INTO deployments (service_id, status, trigger, image_ref, logs, started_at)
        VALUES ($1, 'running', $2, $3, '', now())
        RETURNING id
        "#,
        service_id,
        trigger,
        image
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)
}

pub(crate) async fn registry_auth(
    state: &AppState,
    image: &str,
    registry_id: Option<Uuid>,
) -> Result<Option<RegistryAuth>, ApiError> {
    let rows = sqlx::query_as!(
        RegistrySecret,
        r#"SELECT id, name, url, username, password_enc FROM registries ORDER BY name"#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let records: Vec<RegistryRecord> = rows
        .iter()
        .map(|row| RegistryRecord {
            name: row.name.clone(),
            url: row.url.clone(),
        })
        .collect();
    let explicit = registry_id.and_then(|id| rows.iter().find(|row| row.id == id));
    if registry_id.is_some() && explicit.is_none() {
        return Err(not_found("registry not found"));
    }
    let explicit_record = explicit.map(|row| RegistryRecord {
        name: row.name.clone(),
        url: row.url.clone(),
    });
    let Some(chosen) = select_registry(image, explicit_record.as_ref(), &records) else {
        return Ok(None);
    };
    let row = rows.iter().find(|row| row.name == chosen.name);
    let Some(row) = row else {
        return Ok(None);
    };
    if row.username.is_none() && row.password_enc.is_none() {
        return Ok(None);
    }
    let password = match &row.password_enc {
        Some(blob) => {
            let bytes = state.cipher.decrypt(blob).map_err(|_| {
                ApiError::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "crypto_error",
                    "stored registry password could not be decrypted",
                )
            })?;
            String::from_utf8(bytes)
                .map_err(|_| bad("invalid", "stored registry password is not UTF-8"))?
        }
        None => String::new(),
    };
    tracing::debug!(
        registry = %row.name,
        server = %image_registry(image),
        "using registry credentials for image pull"
    );
    Ok(Some(RegistryAuth {
        server: row.url.clone(),
        username: row.username.clone().unwrap_or_default(),
        password,
    }))
}

async fn note_internal_ip(
    driver: &dyn rudder_core::NodeDriver,
    state: &AppState,
    service_id: Uuid,
    container_id: &str,
) {
    let Ok(Some(address)) = driver.container_ipv4(container_id).await else {
        return;
    };
    if address.parse::<std::net::Ipv4Addr>().is_err() {
        return;
    }
    let _ = sqlx::query!(
        r#"UPDATE services SET spec = spec || $2, updated_at = now() WHERE id = $1"#,
        service_id,
        json!({ "internal_ip": address }),
    )
    .execute(&state.pool)
    .await;
}

async fn access_env(
    state: &AppState,
    service_id: Uuid,
    mut env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let enabled = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM access_policies WHERE service_id = $1 AND enabled
        ) AS "enabled!"
        "#,
        service_id
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);
    if !enabled {
        return env;
    }
    let Ok(Some(token)) = crate::routes::access::service_token(state, service_id).await else {
        return env;
    };
    let mut extra = vec![
        ("RUDDER_ACCESS_TOKEN".to_owned(), token),
        ("RUDDER_ACCESS_SERVICE".to_owned(), service_id.to_string()),
    ];
    if let Some(url) = state.config.public_url.clone() {
        extra.push(("RUDDER_ACCESS_URL".to_owned(), url));
    }
    for (key, value) in extra {
        if !env.iter().any(|(existing, _)| existing == &key) {
            env.push((key, value));
        }
    }
    env
}

async fn runtime_env(
    state: &AppState,
    service_id: Uuid,
) -> Result<Vec<(String, String)>, ApiError> {
    let rows = sqlx::query!(
        r#"
        SELECT key, value_enc
        FROM env_vars
        WHERE service_id = $1 AND is_build_secret = FALSE
        ORDER BY key
        "#,
        service_id
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let mut env = Vec::with_capacity(rows.len());
    for row in rows {
        let bytes = state.cipher.decrypt(&row.value_enc).map_err(|_| {
            ApiError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "crypto_error",
                "stored environment value could not be decrypted",
            )
        })?;
        let value = String::from_utf8(bytes)
            .map_err(|_| bad("invalid", "stored environment value is not UTF-8"))?;
        env.push((row.key, value));
    }
    Ok(env)
}

struct ServiceResources {
    command: Vec<String>,
    ports: Vec<PortMapping>,
    volumes: Vec<VolumeMount>,
    memory_bytes: Option<i64>,
    nano_cpus: Option<i64>,
    restart: String,
}

fn resources_from_spec(spec: &Value) -> Result<ServiceResources, ApiError> {
    let command = spec
        .get("command")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if command.iter().any(|part| part.contains('\0')) {
        return Err(bad("invalid", "command arguments cannot contain NUL"));
    }
    let mut ports = Vec::new();
    if let Some(items) = spec.get("ports").and_then(Value::as_array) {
        for item in items {
            let container = item
                .get("container")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
                .ok_or_else(|| bad("invalid", "port.container must be 1 to 65535"))?;
            if container == 0 {
                return Err(bad("invalid", "port.container must be 1 to 65535"));
            }
            let host = item
                .get("host")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok());
            let protocol = item
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("tcp")
                .to_owned();
            if protocol != "tcp" && protocol != "udp" {
                return Err(bad("invalid", "port protocol must be tcp or udp"));
            }
            ports.push(PortMapping {
                container,
                host,
                protocol,
            });
        }
    }
    let mut volumes = Vec::new();
    if let Some(items) = spec.get("volumes").and_then(Value::as_array) {
        for item in items {
            let source = item
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let target = item
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if source.is_empty()
                || target.is_empty()
                || source.contains(':')
                || target.contains(':')
                || source.contains('\0')
                || target.contains('\0')
            {
                return Err(bad(
                    "invalid",
                    "volume source and target must be non-empty and cannot contain ':'",
                ));
            }
            volumes.push(VolumeMount {
                source,
                target,
                read_only: item
                    .get("read_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    let memory = spec.get("memory_bytes").and_then(Value::as_i64);
    let cpus = spec.get("nano_cpus").and_then(Value::as_i64);
    let restart = spec
        .get("restart")
        .and_then(Value::as_str)
        .unwrap_or("unless-stopped")
        .to_owned();
    if !matches!(
        restart.as_str(),
        "no" | "always" | "unless-stopped" | "on-failure"
    ) {
        return Err(bad(
            "invalid",
            "restart policy must be no, always, unless-stopped, or on-failure",
        ));
    }
    Ok(ServiceResources {
        command,
        ports,
        volumes,
        memory_bytes: memory,
        nano_cpus: cpus,
        restart,
    })
}

async fn record_running(
    state: &AppState,
    service_id: Uuid,
    saved: &SavedContainer,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    sqlx::query!(
        r#"
        UPDATE service_containers
        SET state = 'stopped'
        WHERE service_id = $1 AND state = 'running'
        "#,
        service_id
    )
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    sqlx::query!(
        r#"
        INSERT INTO service_containers (
            service_id, deployment_id, container_id, container_name, image_ref, generation, state
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'running')
        "#,
        service_id,
        saved.deployment_id,
        saved.container_id,
        saved.container_name,
        saved.image,
        saved.generation
    )
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    if let Some(watch) = saved.watch_tag {
        sqlx::query!(
            r#"
            UPDATE services
            SET current_container_id = $2,
                current_image = $3,
                spec = spec || jsonb_build_object('image', $3::text, 'watch_tag', $4::bool),
                updated_at = now()
            WHERE id = $1
            "#,
            service_id,
            saved.container_id,
            saved.image,
            watch
        )
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
    } else {
        sqlx::query!(
            r#"
            UPDATE services
            SET current_container_id = $2,
                current_image = $3,
                spec = spec || jsonb_build_object('image', $3::text),
                updated_at = now()
            WHERE id = $1
            "#,
            service_id,
            saved.container_id,
            saved.image
        )
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
    }
    tx.commit().await.map_err(ApiError::database)?;
    Ok(())
}

async fn restore_previous(
    state: &AppState,
    service_id: Uuid,
    previous_id: Option<&str>,
    previous_image: Option<&str>,
    previous_status: &str,
    new_id: &str,
) {
    let result = sqlx::query!(
        r#"
        UPDATE services
        SET current_container_id = $2,
            current_image = $3,
            status = $4,
            updated_at = now()
        WHERE id = $1
        "#,
        service_id,
        previous_id,
        previous_image,
        previous_status
    )
    .execute(&state.pool)
    .await;
    if let Err(error) = result {
        tracing::error!(%error, "could not restore the previous container pointer");
    }
    let removed = sqlx::query!(
        r#"DELETE FROM service_containers WHERE service_id = $1 AND container_id = $2"#,
        service_id,
        new_id
    )
    .execute(&state.pool)
    .await;
    if let Err(error) = removed {
        tracing::error!(%error, "could not forget the failed container row");
    }
    if let Some(previous_id) = previous_id {
        let _ = sqlx::query!(
            r#"
            UPDATE service_containers
            SET state = 'running'
            WHERE service_id = $1 AND container_id = $2
            "#,
            service_id,
            previous_id
        )
        .execute(&state.pool)
        .await;
    }
}

async fn finish_deployment(
    state: &AppState,
    deployment_id: Uuid,
    status: &str,
    error: Option<&str>,
    logs: &str,
) -> Result<(), ApiError> {
    let service_id = sqlx::query_scalar!(
        r#"
        UPDATE deployments
        SET status = $2, error = $3, logs = $4, finished_at = now()
        WHERE id = $1
        RETURNING service_id
        "#,
        deployment_id,
        status,
        error,
        logs
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if let Some(service_id) = service_id {
        let event = match status {
            "succeeded" => Some(crate::events::Event::info(
                "deploy.succeeded",
                "Deployment succeeded",
            )),
            "failed" => Some(
                crate::events::Event::warning("deploy.failed", "Deployment failed")
                    .message(error.unwrap_or_default()),
            ),
            _ => None,
        };
        if let Some(event) = event {
            let event = event
                .service(service_id)
                .payload(serde_json::json!({ "deployment_id": deployment_id }));
            crate::events::record(&state.pool, event).await;
        }
    }
    Ok(())
}

async fn mark_failed(
    state: &AppState,
    service_id: Uuid,
    deployment_id: Option<Uuid>,
    message: &str,
) {
    if let Some(deployment_id) = deployment_id {
        let _ = finish_deployment(state, deployment_id, "failed", Some(message), message).await;
    }
    let result = sqlx::query!(
        r#"
        UPDATE services
        SET status = 'failed', updated_at = now()
        WHERE id = $1 AND status IN ('deploying', 'building')
        "#,
        service_id
    )
    .execute(&state.pool)
    .await;
    if let Err(error) = result {
        tracing::error!(%error, "could not mark the service failed");
    }
}

pub(crate) async fn load_deployment(
    state: &AppState,
    id: Uuid,
) -> Result<DeploymentView, ApiError> {
    let row = sqlx::query_as!(
        DeploymentRow,
        r#"
        SELECT id, service_id, status, trigger, image_ref, error,
               right(logs, 200000) AS "logs!", length(logs) > 200000 AS "logs_truncated!",
               started_at, finished_at, created_at
        FROM deployments
        WHERE id = $1
        "#,
        id
    )
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(deployment_view(row))
}

fn deployment_view(row: DeploymentRow) -> DeploymentView {
    DeploymentView {
        id: row.id,
        service_id: row.service_id,
        status: row.status,
        trigger: row.trigger,
        image_ref: row.image_ref,
        error: row.error,
        logs: row.logs,
        logs_truncated: row.logs_truncated,
        started_at: row.started_at,
        finished_at: row.finished_at,
        created_at: row.created_at,
    }
}
