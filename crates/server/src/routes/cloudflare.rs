//! Optional Cloudflare Zero Trust mode: tunnel replicas, hostnames, Access in
//! front of the panel, and SSH through Access for Infrastructure.
//!
//! Every call needs an administrator. Secrets are written encrypted and are
//! never returned.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::Json;
use rudder_core::{
    can_close_ports, normalize_team_domain, preflight, sshd_trust_script, tunnel_cname,
    validate_access_email, validate_cloudflare_id, validate_replicas, validate_ssh_ca_public_key,
    validate_unix_user, PreflightInput, MAX_TUNNEL_REPLICAS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::cloudflare::{self as cf, Cf, Settings, SkippedHost, SyncReport};
use crate::error::ApiError;
use crate::routes::catalog::{bad, conflict, core_error};
use crate::state::AppState;

const MAX_ACCESS_EMAILS: usize = 20;
const MAX_SSH_USERS: usize = 10;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/v1/cloudflare", get(status))
        .route("/api/v1/cloudflare/credentials", put(save_credentials))
        .route(
            "/api/v1/cloudflare/tunnel",
            post(create_tunnel).delete(remove_tunnel),
        )
        .route("/api/v1/cloudflare/tunnel/replicas", put(set_replicas))
        .route("/api/v1/cloudflare/tunnel/publish", post(publish))
        .route("/api/v1/cloudflare/access", put(setup_access))
        .route(
            "/api/v1/cloudflare/access/enforcement",
            put(set_enforcement),
        )
        .route("/api/v1/cloudflare/ssh", put(setup_ssh).delete(remove_ssh))
        .route("/api/v1/cloudflare/ssh/script", get(ssh_script))
        .route("/api/v1/cloudflare/ssh/confirmation", put(confirm_ssh))
        .route("/api/v1/cloudflare/preflight", get(readiness))
}

#[derive(Serialize, ToSchema)]
pub struct CloudflareStatus {
    pub account_id: String,
    pub zone_id: String,
    pub zone_name: String,
    /// True once an API token is saved. The token itself is never returned.
    pub token_saved: bool,
    pub tunnel_id: Option<String>,
    pub tunnel_name: String,
    pub replicas: i32,
    pub replicas_running: usize,
    pub max_replicas: u8,
    /// Connections Cloudflare sees for the tunnel; `None` when it could not be read.
    pub connections: Option<usize>,
    pub tunnel_published: bool,
    pub published_hosts: Vec<String>,
    pub panel_domain: Option<String>,
    pub team_domain: String,
    pub access_emails: Vec<String>,
    pub access_configured: bool,
    pub access_enforced: bool,
    pub agent_bypass: bool,
    pub ssh_address: String,
    pub ssh_users: Vec<String>,
    pub ssh_configured: bool,
    pub ssh_ready: bool,
    pub ssh_ca_public_key: String,
    /// Why the replicas could not be started, when the last create or scale failed.
    pub replica_error: Option<String>,
}

async fn panel_domain(state: &AppState) -> Result<Option<String>, ApiError> {
    let domain: Option<String> =
        sqlx::query_scalar("SELECT panel_domain FROM panel_settings WHERE id = TRUE")
            .fetch_one(&state.pool)
            .await
            .map_err(ApiError::database)?;
    Ok(domain
        .map(|d| d.to_ascii_lowercase())
        .filter(|d| !d.is_empty()))
}

async fn build_status(state: &AppState) -> Result<CloudflareStatus, ApiError> {
    let settings = cf::load(&state.pool).await?;
    let connections = match (&settings.tunnel_id, &settings.api_token_enc) {
        (Some(tunnel), Some(_)) => match cf::client(state, &settings) {
            Ok(client) => client
                .tunnel_connections(&settings.account_id, tunnel)
                .await
                .ok(),
            Err(_) => None,
        },
        _ => None,
    };
    Ok(CloudflareStatus {
        token_saved: settings.api_token_enc.is_some(),
        replicas_running: if settings.tunnel_id.is_some() {
            cf::replicas_running(state).await
        } else {
            0
        },
        max_replicas: MAX_TUNNEL_REPLICAS,
        connections,
        panel_domain: panel_domain(state).await?,
        access_configured: settings.access_app_id.is_some(),
        ssh_configured: settings.ssh_app_id.is_some(),
        account_id: settings.account_id,
        zone_id: settings.zone_id,
        zone_name: settings.zone_name,
        tunnel_id: settings.tunnel_id,
        tunnel_name: settings.tunnel_name,
        replicas: settings.replicas,
        tunnel_published: settings.tunnel_published,
        published_hosts: settings.published_hosts,
        team_domain: settings.team_domain,
        access_emails: settings.access_emails,
        access_enforced: settings.access_enforced,
        agent_bypass: settings.agent_bypass,
        ssh_address: settings.ssh_address,
        ssh_users: settings.ssh_users,
        ssh_ready: settings.ssh_ready,
        ssh_ca_public_key: settings.ssh_ca_public_key,
        replica_error: None,
    })
}

async fn status(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<CloudflareStatus>, ApiError> {
    auth.require_admin()?;
    Ok(Json(build_status(&state).await?))
}

async fn audit(state: &AppState, auth: &AuthUser, action: &str, metadata: Value) {
    crate::audit::record(
        &state.pool,
        Some(auth.id),
        action,
        "cloudflare",
        None,
        metadata,
        None,
    )
    .await;
}

// ------------------------------------------------------------- credentials

#[derive(Deserialize, ToSchema)]
pub struct SaveCredentials {
    /// 32-character Cloudflare account id.
    pub account_id: String,
    /// 32-character id of the zone the hostnames live in.
    pub zone_id: String,
    /// API token. Leave out to keep the saved one.
    pub api_token: Option<String>,
}

fn validate_api_token(token: &str) -> Result<(), ApiError> {
    let ok = !token.is_empty()
        && token.len() <= 200
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte));
    if ok {
        Ok(())
    } else {
        Err(bad("invalid_token", "that is not a Cloudflare API token"))
    }
}

async fn save_credentials(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<SaveCredentials>,
) -> Result<Json<CloudflareStatus>, ApiError> {
    auth.require_write()?;
    let account_id = request.account_id.trim().to_ascii_lowercase();
    let zone_id = request.zone_id.trim().to_ascii_lowercase();
    validate_cloudflare_id("account id", &account_id).map_err(core_error)?;
    validate_cloudflare_id("zone id", &zone_id).map_err(core_error)?;
    let settings = cf::load(&state.pool).await?;
    if settings.tunnel_id.is_some()
        && (settings.account_id != account_id || settings.zone_id != zone_id)
    {
        return Err(conflict(
            "tunnel_exists",
            "remove the tunnel before changing the account or zone",
        ));
    }
    let (client, token_blob) = match request.api_token.as_deref().map(str::trim) {
        Some(token) if !token.is_empty() => {
            validate_api_token(token)?;
            let blob = state.cipher.encrypt(token.as_bytes()).map_err(core_error)?;
            (Cf::new(&state.config.cloudflare_api_url, token)?, blob)
        }
        _ => {
            let blob = settings
                .api_token_enc
                .clone()
                .ok_or_else(|| bad("invalid_token", "enter the Cloudflare API token"))?;
            (cf::client(&state, &settings)?, blob)
        }
    };
    // Each check names the call that failed, so a missing permission is obvious.
    let zone_name = client.zone(&account_id, &zone_id).await?;
    client.check_tunnel_access(&account_id).await?;
    client.check_access_apps(&account_id).await?;
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET account_id = $1, zone_id = $2, zone_name = $3, api_token_enc = $4, updated_at = now()
           WHERE id = TRUE"#,
    )
    .bind(&account_id)
    .bind(&zone_id)
    .bind(&zone_name)
    .bind(&token_blob)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    audit(
        &state,
        &auth,
        "cloudflare.credentials",
        json!({ "account_id": account_id, "zone": zone_name }),
    )
    .await;
    Ok(Json(build_status(&state).await?))
}

// ------------------------------------------------------------------ tunnel

#[derive(Deserialize, ToSchema)]
pub struct CreateTunnel {
    /// Number of `cloudflared` replicas, 1 to 5. Defaults to 2.
    pub replicas: Option<i64>,
}

async fn create_tunnel(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<CreateTunnel>,
) -> Result<(StatusCode, Json<CloudflareStatus>), ApiError> {
    auth.require_write()?;
    let replicas = validate_replicas(request.replicas.unwrap_or(2)).map_err(core_error)?;
    let settings = cf::load(&state.pool).await?;
    if settings.tunnel_id.is_some() {
        return Err(conflict("tunnel_exists", "the tunnel already exists"));
    }
    if settings.api_token_enc.is_none() || settings.zone_id.is_empty() {
        return Err(bad(
            "not_configured",
            "save the Cloudflare account, zone, and API token first",
        ));
    }
    let client = cf::client(&state, &settings)?;
    let name = format!("rudder-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let tunnel_id = client.create_tunnel(&settings.account_id, &name).await?;
    let token = client
        .tunnel_token(&settings.account_id, &tunnel_id)
        .await?;
    let token_blob = state.cipher.encrypt(token.as_bytes()).map_err(core_error)?;
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET tunnel_id = $1, tunnel_name = $2, tunnel_token_enc = $3, replicas = $4,
               updated_at = now()
           WHERE id = TRUE"#,
    )
    .bind(&tunnel_id)
    .bind(&name)
    .bind(&token_blob)
    .bind(i32::from(replicas))
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    // The tunnel exists at Cloudflare and is saved, so a failure to start the
    // containers is reported in the status and fixed by saving the count again.
    let started = cf::scale_replicas(&state, &token, replicas, false).await;
    audit(
        &state,
        &auth,
        "cloudflare.tunnel.create",
        json!({ "tunnel": name, "replicas": replicas }),
    )
    .await;
    let mut status = build_status(&state).await?;
    status.replica_error = started.err().map(|error| error.public_message());
    Ok((StatusCode::CREATED, Json(status)))
}

#[derive(Deserialize, ToSchema)]
pub struct SetReplicas {
    pub replicas: i64,
}

async fn set_replicas(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<SetReplicas>,
) -> Result<Json<CloudflareStatus>, ApiError> {
    auth.require_write()?;
    let replicas = validate_replicas(request.replicas).map_err(core_error)?;
    let settings = cf::load(&state.pool).await?;
    cf::require_tunnel(&settings)?;
    let token = cf::tunnel_token(&state, &settings)?;
    sqlx::query("UPDATE cloudflare_settings SET replicas = $1, updated_at = now() WHERE id = TRUE")
        .bind(i32::from(replicas))
        .execute(&state.pool)
        .await
        .map_err(ApiError::database)?;
    let started = cf::scale_replicas(&state, &token, replicas, false).await;
    audit(
        &state,
        &auth,
        "cloudflare.tunnel.scale",
        json!({ "replicas": replicas }),
    )
    .await;
    let mut status = build_status(&state).await?;
    status.replica_error = started.err().map(|error| error.public_message());
    Ok(Json(status))
}

#[derive(Deserialize, ToSchema, Default)]
pub struct PublishRequest {
    /// Replace an existing A, AAAA, or CNAME record with the tunnel's. Off by default.
    #[serde(default)]
    pub overwrite_dns: bool,
}

async fn publish(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<PublishRequest>,
) -> Result<Json<SyncReport>, ApiError> {
    auth.require_write()?;
    let report = cf::sync(&state, request.overwrite_dns).await?;
    cf::refresh_mode(&state).await;
    audit(
        &state,
        &auth,
        "cloudflare.tunnel.publish",
        json!({
            "hostnames": report.hostnames,
            "pending": report.pending,
            "overwrite_dns": request.overwrite_dns,
        }),
    )
    .await;
    Ok(Json(report))
}

#[derive(Serialize, ToSchema)]
pub struct TeardownReport {
    /// Hostnames whose DNS record Rudder removed. Point them back at this server.
    pub removed_dns: Vec<String>,
    /// Steps that did not finish; the rest was removed.
    pub warnings: Vec<String>,
}

async fn remove_tunnel(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<TeardownReport>, ApiError> {
    auth.require_write()?;
    let settings = cf::load(&state.pool).await?;
    let tunnel_id = cf::require_tunnel(&settings)?.to_owned();
    let mut report = TeardownReport {
        removed_dns: Vec::new(),
        warnings: Vec::new(),
    };
    let client = cf::client(&state, &settings)?;
    // Stop serving through the tunnel first, so routing is back to normal.
    sqlx::query(
        "UPDATE cloudflare_settings SET tunnel_published = FALSE, access_enforced = FALSE WHERE id = TRUE",
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    cf::refresh_mode(&state).await;
    if let Err(error) = crate::publish::publish_local(&state).await {
        report.warnings.push(error.public_message());
    }
    cf::remove_replicas(&state).await;
    for host in &settings.published_hosts {
        match client.dns_records(&settings.zone_id, host).await {
            Ok(records) => {
                for record in records.iter().filter(|record| {
                    record.kind == "CNAME"
                        && record
                            .content
                            .eq_ignore_ascii_case(&tunnel_cname(&tunnel_id))
                }) {
                    match client.delete_dns(&settings.zone_id, &record.id).await {
                        Ok(()) => report.removed_dns.push(host.clone()),
                        Err(error) => report.warnings.push(error.public_message()),
                    }
                }
            }
            Err(error) => report.warnings.push(error.public_message()),
        }
    }
    let mut apps: Vec<&String> = settings.agent_bypass_app_ids.iter().collect();
    apps.extend(settings.access_app_id.iter());
    apps.extend(settings.ssh_app_id.iter());
    for app in apps {
        if let Err(error) = client.delete_access_app(&settings.account_id, app).await {
            report.warnings.push(error.public_message());
        }
    }
    if let Some(target) = &settings.ssh_target_id {
        if let Err(error) = client.delete_target(&settings.account_id, target).await {
            report.warnings.push(error.public_message());
        }
    }
    if !settings.ssh_address.is_empty() && settings.ssh_app_id.is_some() {
        match client.default_virtual_network(&settings.account_id).await {
            Ok(vnet) => {
                if let Err(error) = client
                    .delete_route(&settings.account_id, &settings.ssh_address, &vnet)
                    .await
                {
                    report.warnings.push(error.public_message());
                }
            }
            Err(error) => report.warnings.push(error.public_message()),
        }
    }
    if let Err(error) = client.delete_tunnel(&settings.account_id, &tunnel_id).await {
        report.warnings.push(error.public_message());
    }
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET tunnel_id = NULL, tunnel_name = '', tunnel_token_enc = NULL,
               tunnel_published = FALSE, published_hosts = '{}',
               access_app_id = NULL, access_aud = NULL, access_enforced = FALSE,
               agent_bypass = FALSE, agent_bypass_app_ids = '{}',
               ssh_address = '', ssh_users = '{}', ssh_target_id = NULL, ssh_app_id = NULL,
               ssh_ca_public_key = '', ssh_ready = FALSE, updated_at = now()
           WHERE id = TRUE"#,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    cf::refresh_mode(&state).await;
    audit(
        &state,
        &auth,
        "cloudflare.tunnel.remove",
        json!({ "removed_dns": report.removed_dns.len(), "warnings": report.warnings.len() }),
    )
    .await;
    Ok(Json(report))
}

// ------------------------------------------------------------------ Access

#[derive(Deserialize, ToSchema)]
pub struct SetupAccess {
    /// Your Zero Trust team domain, such as `myteam.cloudflareaccess.com` or just `myteam`.
    pub team_domain: String,
    /// Email addresses allowed through Cloudflare Access.
    pub emails: Vec<String>,
    /// Let AI agents and API clients reach `/mcp`, `/oauth`, `/.well-known`, and
    /// `/api/health` without an Access login. They still need their own tokens.
    #[serde(default)]
    pub agent_bypass: bool,
}

fn emails_policy(emails: &[String]) -> Vec<Value> {
    emails
        .iter()
        .map(|email| json!({ "email": { "email": email } }))
        .collect()
}

async fn setup_access(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<SetupAccess>,
) -> Result<Json<CloudflareStatus>, ApiError> {
    auth.require_write()?;
    let team_domain = normalize_team_domain(&request.team_domain).map_err(core_error)?;
    let mut emails = Vec::new();
    for email in &request.emails {
        let email = validate_access_email(email).map_err(core_error)?;
        if !emails.contains(&email) {
            emails.push(email);
        }
    }
    if emails.is_empty() || emails.len() > MAX_ACCESS_EMAILS {
        return Err(bad(
            "invalid_emails",
            format!("enter between 1 and {MAX_ACCESS_EMAILS} email addresses"),
        ));
    }
    let settings = cf::load(&state.pool).await?;
    if settings.api_token_enc.is_none() {
        return Err(bad(
            "not_configured",
            "save the Cloudflare credentials first",
        ));
    }
    let host = panel_domain(&state).await?.ok_or_else(|| {
        bad(
            "no_panel_domain",
            "set a panel domain under Certificates first",
        )
    })?;
    if !rudder_core::host_in_zone(&host, &settings.zone_name) {
        return Err(bad(
            "outside_zone",
            format!("the panel domain is not in the {} zone", settings.zone_name),
        ));
    }
    let client = cf::client(&state, &settings)?;
    let allow = json!({
        "name": "Rudder administrators",
        "decision": "allow",
        "include": emails_policy(&emails),
    });
    let (app_id, aud) = client
        .upsert_access_app(
            &settings.account_id,
            settings.access_app_id.as_deref(),
            &json!({
                "name": "Rudder panel",
                "domain": host,
                "type": "self_hosted",
                "session_duration": "24h",
                "policies": [allow],
            }),
        )
        .await?;
    if aud.is_empty() {
        return Err(ApiError::new(
            StatusCode::FAILED_DEPENDENCY,
            "cloudflare",
            "saving the Access application: Cloudflare returned no audience tag",
        ));
    }
    let mut bypass_ids: Vec<String> = Vec::new();
    if request.agent_bypass {
        for (index, path) in cf::BYPASS_PATHS.iter().enumerate() {
            let existing = settings.agent_bypass_app_ids.get(index).map(String::as_str);
            let (id, _) = client
                .upsert_access_app(
                    &settings.account_id,
                    existing,
                    &json!({
                        "name": format!("Rudder agents {path}"),
                        "domain": format!("{host}{path}"),
                        "type": "self_hosted",
                        "session_duration": "24h",
                        "policies": [{
                            "name": "Bypass",
                            "decision": "bypass",
                            "include": [{ "everyone": {} }],
                        }],
                    }),
                )
                .await?;
            bypass_ids.push(id);
        }
    } else {
        for id in &settings.agent_bypass_app_ids {
            client.delete_access_app(&settings.account_id, id).await?;
        }
    }
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET team_domain = $1, access_emails = $2, access_app_id = $3, access_aud = $4,
               agent_bypass = $5, agent_bypass_app_ids = $6, updated_at = now()
           WHERE id = TRUE"#,
    )
    .bind(&team_domain)
    .bind(&emails)
    .bind(&app_id)
    .bind(&aud)
    .bind(request.agent_bypass)
    .bind(&bypass_ids)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    cf::refresh_mode(&state).await;
    audit(
        &state,
        &auth,
        "cloudflare.access.save",
        json!({ "team_domain": team_domain, "emails": emails.len(), "agent_bypass": request.agent_bypass }),
    )
    .await;
    Ok(Json(build_status(&state).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct SetEnforcement {
    pub enforced: bool,
}

async fn set_enforcement(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Json(request): Json<SetEnforcement>,
) -> Result<Json<CloudflareStatus>, ApiError> {
    auth.require_write()?;
    let settings = cf::load(&state.pool).await?;
    if request.enforced {
        let (Some(_), Some(aud)) = (&settings.access_app_id, &settings.access_aud) else {
            return Err(bad("no_access", "set up Cloudflare Access first"));
        };
        // A request through the panel domain must already carry a valid Access
        // token. That proves Access works before the panel starts requiring it.
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(':').next().unwrap_or("").to_ascii_lowercase())
            .unwrap_or_default();
        if panel_domain(&state).await?.as_deref() == Some(host.as_str()) {
            let token = headers
                .get("cf-access-jwt-assertion")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if !state
                .cloudflare
                .verify(&settings.team_domain, aud, token)
                .await
            {
                return Err(conflict(
                    "access_not_verified",
                    "this request did not come through Cloudflare Access, so turning it on could lock you out; open the panel through its Cloudflare address and try again",
                ));
            }
        }
    }
    sqlx::query(
        "UPDATE cloudflare_settings SET access_enforced = $1, updated_at = now() WHERE id = TRUE",
    )
    .bind(request.enforced)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    cf::refresh_mode(&state).await;
    audit(
        &state,
        &auth,
        "cloudflare.access.enforce",
        json!({ "enforced": request.enforced }),
    )
    .await;
    Ok(Json(build_status(&state).await?))
}

// --------------------------------------------------------------------- SSH

#[derive(Deserialize, ToSchema)]
pub struct SetupSsh {
    /// IPv4 address `cloudflared` reaches the server's SSH daemon on, for
    /// example the Docker gateway of the `rudder-proxy` network.
    pub address: String,
    /// Unix accounts Cloudflare may log people in as.
    pub users: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SshSetup {
    pub status: CloudflareStatus,
    /// Run as root on the server so `sshd` trusts Cloudflare's certificates.
    pub script: String,
}

fn parse_ssh_address(input: &str) -> Result<std::net::Ipv4Addr, ApiError> {
    let ip: std::net::Ipv4Addr = input
        .trim()
        .parse()
        .map_err(|_| bad("invalid_address", "enter an IPv4 address"))?;
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() || ip.is_broadcast() {
        return Err(bad(
            "invalid_address",
            "that address cannot be reached from the tunnel; use the server's own address or the Docker gateway",
        ));
    }
    Ok(ip)
}

fn target_name(tunnel_name: &str) -> String {
    let clean: String = tunnel_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    format!("{clean}-ssh")
}

async fn setup_ssh(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<SetupSsh>,
) -> Result<Json<SshSetup>, ApiError> {
    auth.require_write()?;
    let address = parse_ssh_address(&request.address)?.to_string();
    let mut users = Vec::new();
    for user in &request.users {
        let user = user.trim();
        validate_unix_user(user).map_err(core_error)?;
        if !users.iter().any(|existing| existing == user) {
            users.push(user.to_owned());
        }
    }
    if users.is_empty() || users.len() > MAX_SSH_USERS {
        return Err(bad(
            "invalid_users",
            format!("enter between 1 and {MAX_SSH_USERS} Unix accounts"),
        ));
    }
    let settings = cf::load(&state.pool).await?;
    let tunnel_id = cf::require_tunnel(&settings)?.to_owned();
    if settings.access_emails.is_empty() {
        return Err(bad(
            "no_access",
            "set up Cloudflare Access first; SSH uses the same email list",
        ));
    }
    let client = cf::client(&state, &settings)?;
    let ca = validate_ssh_ca_public_key(&client.ssh_ca(&settings.account_id).await?)
        .map_err(core_error)?;
    let vnet = client.default_virtual_network(&settings.account_id).await?;
    if !settings.ssh_address.is_empty() && settings.ssh_address != address {
        // Moving to another address: drop the old route, then add the new one.
        let _ = client
            .delete_route(&settings.account_id, &settings.ssh_address, &vnet)
            .await;
    }
    client
        .ensure_route(&settings.account_id, &tunnel_id, &address, &vnet)
        .await?;
    let hostname = target_name(&settings.tunnel_name);
    let target_id = client
        .ensure_target(&settings.account_id, &hostname, &address, &vnet)
        .await?;
    let (app_id, _) = client
        .upsert_access_app(
            &settings.account_id,
            settings.ssh_app_id.as_deref(),
            &json!({
                "name": "Rudder SSH",
                "type": "infrastructure",
                "session_duration": "24h",
                "target_criteria": [{
                    "port": 22,
                    "protocol": "SSH",
                    "include": { "target_attributes": { "hostname": [hostname] } },
                }],
                "policies": [{
                    "name": "Rudder administrators",
                    "decision": "allow",
                    "include": emails_policy(&settings.access_emails),
                    "connection_rules": { "ssh": { "usernames": users } },
                }],
            }),
        )
        .await?;
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET ssh_address = $1, ssh_users = $2, ssh_target_id = $3, ssh_app_id = $4,
               ssh_ca_public_key = $5, ssh_ready = FALSE, updated_at = now()
           WHERE id = TRUE"#,
    )
    .bind(&address)
    .bind(&users)
    .bind(&target_id)
    .bind(&app_id)
    .bind(&ca)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    audit(
        &state,
        &auth,
        "cloudflare.ssh.save",
        json!({ "address": address, "users": users }),
    )
    .await;
    let script = sshd_trust_script(&ca).map_err(core_error)?;
    Ok(Json(SshSetup {
        status: build_status(&state).await?,
        script,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct SshScript {
    pub script: String,
}

async fn ssh_script(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<SshScript>, ApiError> {
    auth.require_admin()?;
    let settings = cf::load(&state.pool).await?;
    if settings.ssh_ca_public_key.is_empty() {
        return Err(bad("no_ssh", "set up SSH first"));
    }
    Ok(Json(SshScript {
        script: sshd_trust_script(&settings.ssh_ca_public_key).map_err(core_error)?,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct ConfirmSsh {
    /// True once the script ran on the server and a connection through Cloudflare worked.
    pub confirmed: bool,
}

async fn confirm_ssh(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(request): Json<ConfirmSsh>,
) -> Result<Json<CloudflareStatus>, ApiError> {
    auth.require_write()?;
    let settings = cf::load(&state.pool).await?;
    if settings.ssh_app_id.is_none() {
        return Err(bad("no_ssh", "set up SSH first"));
    }
    sqlx::query(
        "UPDATE cloudflare_settings SET ssh_ready = $1, updated_at = now() WHERE id = TRUE",
    )
    .bind(request.confirmed)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    audit(
        &state,
        &auth,
        "cloudflare.ssh.confirm",
        json!({ "confirmed": request.confirmed }),
    )
    .await;
    Ok(Json(build_status(&state).await?))
}

async fn remove_ssh(State(state): State<AppState>, auth: AuthUser) -> Result<StatusCode, ApiError> {
    auth.require_write()?;
    let settings = cf::load(&state.pool).await?;
    let client = cf::client(&state, &settings)?;
    if let Some(app) = &settings.ssh_app_id {
        client.delete_access_app(&settings.account_id, app).await?;
    }
    if let Some(target) = &settings.ssh_target_id {
        client.delete_target(&settings.account_id, target).await?;
    }
    if !settings.ssh_address.is_empty() {
        let vnet = client.default_virtual_network(&settings.account_id).await?;
        client
            .delete_route(&settings.account_id, &settings.ssh_address, &vnet)
            .await?;
    }
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET ssh_address = '', ssh_users = '{}', ssh_target_id = NULL, ssh_app_id = NULL,
               ssh_ca_public_key = '', ssh_ready = FALSE, updated_at = now()
           WHERE id = TRUE"#,
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    audit(&state, &auth, "cloudflare.ssh.remove", json!({})).await;
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------------- preflight

#[derive(Serialize, ToSchema)]
pub struct PreflightCheckView {
    pub id: String,
    pub ok: bool,
    /// A failing required check blocks closing the public ports.
    pub required: bool,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct Readiness {
    /// True when every required check passes, so the installer may close the ports.
    pub ready: bool,
    pub checks: Vec<PreflightCheckView>,
    pub skipped: Vec<SkippedHost>,
}

async fn readiness(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Readiness>, ApiError> {
    auth.require_admin()?;
    let settings: Settings = cf::load(&state.pool).await?;
    let (hosts, skipped) = cf::wanted_hosts(&state.pool, &settings.zone_name).await?;
    let connections = match (&settings.tunnel_id, &settings.api_token_enc) {
        (Some(tunnel), Some(_)) => match cf::client(&state, &settings) {
            Ok(client) => client
                .tunnel_connections(&settings.account_id, tunnel)
                .await
                .unwrap_or(0),
            Err(_) => 0,
        },
        _ => 0,
    };
    let checks = preflight(PreflightInput {
        replicas_wanted: if settings.tunnel_id.is_some() {
            usize::try_from(settings.replicas).unwrap_or(0)
        } else {
            0
        },
        replicas_running: cf::replicas_running(&state).await,
        tunnel_connections: connections,
        tunnel_serving: settings.tunnel_published,
        panel_domain_set: panel_domain(&state).await?.is_some(),
        hostnames_pending: hosts
            .iter()
            .filter(|host| !settings.published_hosts.contains(host))
            .count(),
        access_enforced: settings.access_enforced,
        ssh_ready: settings.ssh_ready,
    });
    Ok(Json(Readiness {
        ready: can_close_ports(&checks),
        checks: checks
            .into_iter()
            .map(|check| PreflightCheckView {
                id: check.id.to_owned(),
                ok: check.ok,
                required: check.required,
                message: check.message,
            })
            .collect(),
        skipped,
    }))
}
