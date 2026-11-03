//! Write Traefik's dynamic file and the certificate PEMs it reads.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use rudder_core::{
    render_dynamic_config_for, EdgeGate, EdgeMode, NodeDriver, ProxyRoute, TlsMaterial, TlsMode,
    TRAEFIK_CERT_DIR,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::catalog::{bad, core_error};
use crate::state::AppState;

struct RouteRow {
    id: Uuid,
    host: String,
    path_prefix: String,
    target_port: i32,
    https_redirect: bool,
    ssl_mode: String,
    basic_auth_user: Option<String>,
    basic_auth_hash: Option<String>,
    service_name: String,
    container_name: Option<String>,
    compose_service: Option<String>,
    stack_container: Option<String>,
    project: Option<String>,
    cache_enabled: bool,
    cache_policy_id: Option<Uuid>,
    access_enabled: bool,
}

struct PanelRoute {
    panel_domain: Option<String>,
    panel_ssl_mode: String,
    panel_certificate_id: Option<Uuid>,
}

struct CertBlob {
    id: Uuid,
    cert_pem_enc: Vec<u8>,
    key_pem_enc: Vec<u8>,
    chain_pem_enc: Option<Vec<u8>>,
}

/// Write a server's Traefik routes, certificates, and cache rules. Changes for a
/// remote server whose agent is offline are applied when it reconnects.
pub async fn publish_server(state: &AppState, server_id: Uuid) -> Result<bool, ApiError> {
    let kind = sqlx::query_scalar!(r#"SELECT kind FROM servers WHERE id = $1"#, server_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    let Some(kind) = kind else {
        return Err(crate::routes::catalog::not_found("server not found"));
    };
    let driver: std::sync::Arc<dyn NodeDriver> = if kind == "local" {
        state.local_driver.clone()
    } else if let Some(driver) = state.agents.driver(server_id) {
        driver
    } else {
        tracing::info!(%server_id, "routing change stored; the agent applies it when connected");
        return Ok(false);
    };
    let secret = crate::routes::access::gate_secret(state).await?;
    let (routes, note, sites, gate) = render_for_server(
        &state.pool,
        server_id,
        state.config.bind.port(),
        &state.config.self_container,
        &secret,
    )
    .await?;
    let yaml = (routes, note);
    let budget_cap = crate::cache::memory_cap(&state.pool, &state.config.host_proc).await?;
    let cache_document = crate::cache::render_document(&state.pool, sites, budget_cap).await?;
    crate::cache::ensure_token(driver.as_ref()).await?;
    driver
        .write_file(
            rudder_core::CACHE_RULES_FILE,
            &serde_json::to_vec_pretty(&cache_document).unwrap_or_default(),
        )
        .await
        .map_err(core_error)?;
    let materials = write_certificates(state, driver.as_ref()).await?;
    let mode = if kind == "local" && crate::cloudflare::tunnel_published(&state.pool).await {
        EdgeMode::Tunnel
    } else {
        EdgeMode::Direct
    };
    let document =
        render_dynamic_config_for(&yaml.0, &materials, gate.as_ref(), mode).map_err(core_error)?;
    let mut body = document;
    if !yaml.1.is_empty() {
        let _ = write!(body, "\n# panel certificate {}\n", yaml.1);
    }
    driver
        .write_file("traefik/dynamic/routes.yml", body.as_bytes())
        .await
        .map_err(core_error)?;
    Ok(true)
}

pub async fn publish_local(state: &AppState) -> Result<(), ApiError> {
    let id = sqlx::query_scalar!(r#"SELECT id FROM servers WHERE kind = 'local' LIMIT 1"#)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::database)?;
    if let Some(id) = id {
        publish_server(state, id).await?;
    }
    Ok(())
}

pub async fn publish_for_service(state: &AppState, service_id: Uuid) -> Result<(), ApiError> {
    let server_id = sqlx::query_scalar!(
        r#"SELECT server_id FROM services WHERE id = $1"#,
        service_id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if let Some(server_id) = server_id {
        publish_server(state, server_id).await?;
    }
    Ok(())
}

async fn render_for_server(
    pool: &PgPool,
    server_id: Uuid,
    panel_port: u16,
    panel_container: &str,
    gate_secret: &str,
) -> Result<
    (
        Vec<ProxyRoute>,
        String,
        Vec<crate::cache::SiteSource>,
        Option<EdgeGate>,
    ),
    ApiError,
> {
    let rows = sqlx::query_as!(
        RouteRow,
        r#"
        SELECT d.id,
               d.host,
               d.path_prefix,
               d.target_port,
               d.https_redirect,
               d.ssl_mode,
               d.basic_auth_user,
               d.basic_auth_hash,
               s.name AS service_name,
               d.compose_service,
               s.spec -> 'containers' ->> d.compose_service AS "stack_container?",
               s.spec ->> 'project' AS "project?",
               d.cache_enabled,
               d.cache_policy_id,
               EXISTS (
                   SELECT 1 FROM access_policies ap
                   WHERE ap.service_id = s.id AND ap.enabled
               ) AS "access_enabled!",
               (
                   SELECT sc.container_name
                   FROM service_containers sc
                   WHERE sc.service_id = s.id AND sc.state = 'running'
                   ORDER BY sc.generation DESC
                   LIMIT 1
               ) AS "container_name?"
        FROM domains d
        JOIN services s ON s.id = d.service_id
        WHERE s.server_id = $1
        ORDER BY d.host, d.path_prefix
        "#,
        server_id
    )
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;
    let global_limit = sqlx::query_scalar!(
        r#"
        SELECT EXISTS(
            SELECT 1 FROM rate_limit_rules
            WHERE enabled AND (host IS NULL OR host = '')
        ) AS "global!"
        "#
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)?;
    let limited_hosts = sqlx::query_scalar!(
        r#"
        SELECT host AS "host!"
        FROM rate_limit_rules
        WHERE enabled AND host IS NOT NULL AND host <> ''
        "#
    )
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;
    let portal_host =
        sqlx::query_scalar!(r#"SELECT portal_host FROM access_settings WHERE id = TRUE"#)
            .fetch_one(pool)
            .await
            .map_err(ApiError::database)?;
    let mut routes = Vec::new();
    let mut sites = Vec::new();
    for row in rows {
        let access = row.access_enabled;
        let cache = row.cache_enabled.then_some(row.cache_policy_id);
        let mut route = route_from_row(row)?;
        let portal = portal_host.as_deref() == Some(route.host.as_str());
        let limited = global_limit
            || limited_hosts
                .iter()
                .any(|item| item.eq_ignore_ascii_case(&route.host));
        route.access_handoff = access && !portal;
        route.gate = (access || limited) && !portal;
        if let Some(policy_id) = cache {
            sites.push(crate::cache::SiteSource {
                host: route.host.clone(),
                path_prefix: route.path_prefix.clone(),
                upstream: format!("{}:{}", route.container, route.port),
                policy_id,
            });
            route.container = rudder_core::CACHE_CONTAINER.into();
            route.port = rudder_core::CACHE_PORT;
        }
        routes.push(route);
    }
    let panel = sqlx::query_as!(
        PanelRoute,
        r#"
        SELECT panel_domain, panel_ssl_mode, panel_certificate_id
        FROM panel_settings
        WHERE id = TRUE
        "#
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)?;
    let note = panel
        .panel_certificate_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    if let Some(host) = panel.panel_domain.filter(|host| !host.is_empty()) {
        let local = sqlx::query_scalar!(r#"SELECT kind FROM servers WHERE id = $1"#, server_id)
            .fetch_one(pool)
            .await
            .map_err(ApiError::database)?;
        if local == "local" {
            routes.push(ProxyRoute {
                name: "panel".into(),
                host: host.clone(),
                path_prefix: "/".into(),
                container: "rudder".into(),
                port: panel_port,
                https_redirect: panel.panel_ssl_mode != "none",
                tls: tls_mode(&panel.panel_ssl_mode)?,
                basic_auth_user: None,
                basic_auth_hash: None,
                gate: false,
                access_handoff: false,
            });
            for route in &mut routes {
                if route.host == host {
                    route.gate = false;
                    route.access_handoff = false;
                }
            }
        }
    }
    if let Some(host) = portal_host.filter(|host| !host.is_empty()) {
        if !routes.iter().any(|route| route.host == host) {
            routes.push(ProxyRoute {
                name: "access".into(),
                host,
                path_prefix: "/".into(),
                container: panel_container.into(),
                port: panel_port,
                https_redirect: true,
                tls: TlsMode::LetsEncrypt,
                basic_auth_user: None,
                basic_auth_hash: None,
                gate: false,
                access_handoff: false,
            });
        }
    }
    let gate = routes
        .iter()
        .any(|route| route.gate || route.access_handoff)
        .then(|| EdgeGate {
            secret: gate_secret.to_owned(),
            panel_container: panel_container.to_owned(),
            panel_port,
        });
    Ok((routes, note, sites, gate))
}

fn route_from_row(row: RouteRow) -> Result<ProxyRoute, ApiError> {
    let port = u16::try_from(row.target_port)
        .map_err(|_| bad("invalid", "target port is out of range"))?;
    let container = match (row.compose_service, row.stack_container, row.project) {
        (Some(_), Some(container), _) => container,
        (Some(service), None, Some(project)) => format!("{project}-{service}-1"),
        _ => row.container_name.unwrap_or(row.service_name),
    };
    Ok(ProxyRoute {
        name: router_name(row.id),
        host: row.host,
        path_prefix: row.path_prefix,
        container,
        port,
        https_redirect: row.https_redirect,
        tls: tls_mode(&row.ssl_mode)?,
        basic_auth_user: row.basic_auth_user,
        basic_auth_hash: row.basic_auth_hash,
        gate: false,
        access_handoff: false,
    })
}

pub(crate) fn router_name(id: Uuid) -> String {
    let compact = id.to_string().replace('-', "");
    format!("r{}", &compact[..12])
}

pub(crate) fn tls_mode(value: &str) -> Result<TlsMode, ApiError> {
    match value {
        "letsencrypt" => Ok(TlsMode::LetsEncrypt),
        "custom" => Ok(TlsMode::Custom),
        "none" => Ok(TlsMode::None),
        _ => Err(bad(
            "invalid",
            "ssl mode must be letsencrypt, custom, or none",
        )),
    }
}

async fn write_certificates(
    state: &AppState,
    driver: &dyn NodeDriver,
) -> Result<Vec<TlsMaterial>, ApiError> {
    let rows = sqlx::query_as!(
        CertBlob,
        r#"
        SELECT id, cert_pem_enc, key_pem_enc, chain_pem_enc
        FROM certificates
        ORDER BY created_at
        "#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let mut materials = Vec::new();
    for row in rows {
        let cert = decrypt_pem(&state.cipher, &row.cert_pem_enc)?;
        let key = decrypt_pem(&state.cipher, &row.key_pem_enc)?;
        let mut full = cert;
        if let Some(chain) = row.chain_pem_enc {
            let chain = decrypt_pem(&state.cipher, &chain)?;
            if !full.ends_with('\n') {
                full.push('\n');
            }
            full.push_str(&chain);
        }
        let cert_rel = format!("traefik/certs/{}.crt", row.id);
        let key_rel = format!("traefik/certs/{}.key", row.id);
        driver
            .write_file(&cert_rel, full.as_bytes())
            .await
            .map_err(core_error)?;
        driver
            .write_file(&key_rel, key.as_bytes())
            .await
            .map_err(core_error)?;
        materials.push(TlsMaterial {
            cert_file: format!("{TRAEFIK_CERT_DIR}/{}.crt", row.id),
            key_file: format!("{TRAEFIK_CERT_DIR}/{}.key", row.id),
        });
    }
    Ok(materials)
}

fn decrypt_pem(cipher: &rudder_core::Cipher, blob: &[u8]) -> Result<String, ApiError> {
    let bytes = cipher.decrypt(blob).map_err(|_| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "stored certificate could not be decrypted",
        )
    })?;
    String::from_utf8(bytes).map_err(|_| bad("invalid", "stored certificate is not UTF-8"))
}

pub fn expiry_unix_to_time(unix: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(unix, 0).unwrap_or(DateTime::UNIX_EPOCH)
}
