//! Optional Cloudflare Zero Trust mode: the API client, the tunnel replicas, and
//! the panel-side check of Cloudflare Access tokens.
//!
//! Nothing here runs unless an administrator saves Cloudflare credentials. The
//! API token and the tunnel token are stored encrypted and never returned.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use rudder_core::{
    cloudflared_spec, host_in_zone, plan_dns, replica_container_name, tunnel_cname, tunnel_config,
    validate_host, DnsAction, DnsRecord, NodeDriver, MAX_TUNNEL_REPLICAS,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::sync::{Mutex, RwLock};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::catalog::{bad, core_error};
use crate::state::AppState;

const API_TIMEOUT: Duration = Duration::from_secs(20);
const GUARD_TTL: Duration = Duration::from_secs(30);
const JWKS_TTL: Duration = Duration::from_secs(600);

/// A failed Cloudflare call. The message comes from Cloudflare's error list and
/// never contains the token.
#[derive(Debug)]
pub struct CfError {
    pub status: u16,
    pub message: String,
}

impl CfError {
    pub fn into_api(self, step: &str) -> ApiError {
        if self.status == 401 || self.status == 403 {
            return ApiError::new(
                StatusCode::FAILED_DEPENDENCY,
                "cloudflare_permission",
                format!(
                    "{step}: Cloudflare refused the request ({}). Check the API token permissions.",
                    self.message
                ),
            );
        }
        ApiError::new(
            StatusCode::FAILED_DEPENDENCY,
            "cloudflare",
            format!("{step}: {}", self.message),
        )
    }

    fn already_exists(&self) -> bool {
        self.status == 409 || self.message.to_ascii_lowercase().contains("exist")
    }
}

/// Cloudflare API v4 client for one account.
pub struct Cf {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Cf {
    pub fn new(base: &str, token: &str) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "http_client",
                    "could not start the HTTP client",
                )
            })?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    /// Returns the `result` member of a successful response.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, CfError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| CfError {
            status: 0,
            message: if error.is_timeout() {
                "Cloudflare did not answer in time".into()
            } else {
                "could not reach the Cloudflare API".into()
            },
        })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let success = parsed.get("success").and_then(Value::as_bool);
        if status.is_success() && success != Some(false) {
            return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
        }
        let message = parsed
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
            .map(|error| {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed");
                match error.get("code").and_then(Value::as_i64) {
                    Some(code) => format!("{text} (code {code})"),
                    None => text.to_owned(),
                }
            })
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        Err(CfError {
            status: status.as_u16(),
            message: message.chars().take(300).collect(),
        })
    }

    /// Check the zone and return its name, refusing a zone in another account.
    pub async fn zone(&self, account_id: &str, zone_id: &str) -> Result<String, ApiError> {
        let zone = self
            .call(Method::GET, &format!("/zones/{zone_id}"), None)
            .await
            .map_err(|e| e.into_api("reading the zone"))?;
        let owner = zone
            .pointer("/account/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !owner.is_empty() && !owner.eq_ignore_ascii_case(account_id) {
            return Err(bad(
                "zone_account_mismatch",
                "the zone belongs to a different Cloudflare account",
            ));
        }
        Ok(zone
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase())
    }

    pub async fn check_tunnel_access(&self, account_id: &str) -> Result<(), ApiError> {
        self.call(
            Method::GET,
            &format!("/accounts/{account_id}/cfd_tunnel?per_page=1&is_deleted=false"),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("listing tunnels"))
    }

    pub async fn check_access_apps(&self, account_id: &str) -> Result<(), ApiError> {
        self.call(
            Method::GET,
            &format!("/accounts/{account_id}/access/apps?per_page=1"),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("listing Access applications"))
    }

    pub async fn create_tunnel(&self, account_id: &str, name: &str) -> Result<String, ApiError> {
        let result = self
            .call(
                Method::POST,
                &format!("/accounts/{account_id}/cfd_tunnel"),
                Some(&json!({ "name": name, "config_src": "cloudflare" })),
            )
            .await
            .map_err(|e| e.into_api("creating the tunnel"))?;
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Uuid::parse_str(&id).map_err(|_| {
            ApiError::new(
                StatusCode::FAILED_DEPENDENCY,
                "cloudflare",
                "creating the tunnel: Cloudflare returned no tunnel id",
            )
        })?;
        Ok(id)
    }

    pub async fn tunnel_token(
        &self,
        account_id: &str,
        tunnel_id: &str,
    ) -> Result<String, ApiError> {
        let result = self
            .call(
                Method::GET,
                &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/token"),
                None,
            )
            .await
            .map_err(|e| e.into_api("reading the tunnel token"))?;
        result
            .as_str()
            .map(str::to_owned)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::FAILED_DEPENDENCY,
                    "cloudflare",
                    "reading the tunnel token: Cloudflare returned no token",
                )
            })
    }

    pub async fn put_tunnel_config(
        &self,
        account_id: &str,
        tunnel_id: &str,
        config: &Value,
    ) -> Result<(), ApiError> {
        self.call(
            Method::PUT,
            &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/configurations"),
            Some(config),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("publishing the tunnel hostnames"))
    }

    /// Number of live connections the tunnel's replicas hold.
    pub async fn tunnel_connections(
        &self,
        account_id: &str,
        tunnel_id: &str,
    ) -> Result<usize, ApiError> {
        let result = self
            .call(
                Method::GET,
                &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/connections"),
                None,
            )
            .await
            .map_err(|e| e.into_api("reading the tunnel connections"))?;
        Ok(count_connections(&result))
    }

    pub async fn delete_tunnel(&self, account_id: &str, tunnel_id: &str) -> Result<(), ApiError> {
        // Open connections block deletion; clean them up first.
        let _ = self
            .call(
                Method::DELETE,
                &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/connections"),
                None,
            )
            .await;
        self.call(
            Method::DELETE,
            &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}"),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("deleting the tunnel"))
    }

    pub async fn dns_records(&self, zone_id: &str, host: &str) -> Result<Vec<DnsRecord>, ApiError> {
        let result = self
            .call(
                Method::GET,
                &format!("/zones/{zone_id}/dns_records?name={host}&per_page=100"),
                None,
            )
            .await
            .map_err(|e| e.into_api("reading DNS records"))?;
        Ok(items(&result)
            .into_iter()
            .filter_map(|record| {
                Some(DnsRecord {
                    id: record.get("id")?.as_str()?.to_owned(),
                    name: record.get("name")?.as_str()?.to_ascii_lowercase(),
                    kind: record.get("type")?.as_str()?.to_owned(),
                    content: record
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            })
            .collect())
    }

    pub async fn create_cname(
        &self,
        zone_id: &str,
        host: &str,
        tunnel_id: &str,
    ) -> Result<(), ApiError> {
        self.call(
            Method::POST,
            &format!("/zones/{zone_id}/dns_records"),
            Some(&json!({
                "type": "CNAME",
                "name": host,
                "content": tunnel_cname(tunnel_id),
                "proxied": true,
                "ttl": 1,
                "comment": "Managed by Rudder (Cloudflare tunnel)",
            })),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("creating a DNS record"))
    }

    pub async fn delete_dns(&self, zone_id: &str, record_id: &str) -> Result<(), ApiError> {
        self.call(
            Method::DELETE,
            &format!("/zones/{zone_id}/dns_records/{record_id}"),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("deleting a DNS record"))
    }

    /// Create or update a self-hosted or infrastructure Access application.
    /// Returns the application id and its audience tag.
    pub async fn upsert_access_app(
        &self,
        account_id: &str,
        existing: Option<&str>,
        body: &Value,
    ) -> Result<(String, String), ApiError> {
        let (method, path) = match existing {
            Some(id) => (
                Method::PUT,
                format!("/accounts/{account_id}/access/apps/{id}"),
            ),
            None => (Method::POST, format!("/accounts/{account_id}/access/apps")),
        };
        let result = self
            .call(method, &path, Some(body))
            .await
            .map_err(|e| e.into_api("saving the Access application"))?;
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Uuid::parse_str(&id).map_err(|_| {
            ApiError::new(
                StatusCode::FAILED_DEPENDENCY,
                "cloudflare",
                "saving the Access application: Cloudflare returned no application id",
            )
        })?;
        let aud = result
            .get("aud")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok((id, aud))
    }

    pub async fn delete_access_app(&self, account_id: &str, app_id: &str) -> Result<(), ApiError> {
        self.call(
            Method::DELETE,
            &format!("/accounts/{account_id}/access/apps/{app_id}"),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("deleting an Access application"))
    }

    /// The account's SSH certificate authority public key, created when missing.
    pub async fn ssh_ca(&self, account_id: &str) -> Result<String, ApiError> {
        let path = format!("/accounts/{account_id}/access/gateway_ca");
        // An account that never had one answers 12112 `gateway_ca_not_found`
        // (HTTP 404) instead of an empty list; either way the next step creates it.
        let existing = match self.call(Method::GET, &path, None).await {
            Ok(value) => value,
            Err(error) if error.status == 404 || error.message.contains("code 12112") => {
                Value::Null
            }
            Err(error) => return Err(error.into_api("reading the SSH certificate authority")),
        };
        let key_of = |value: &Value| {
            value
                .get("public_key")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        if let Some(key) = items(&existing).into_iter().find_map(key_of) {
            return Ok(key);
        }
        let created = self
            .call(Method::POST, &path, None)
            .await
            .map_err(|e| e.into_api("creating the SSH certificate authority"))?;
        key_of(&created).ok_or_else(|| {
            ApiError::new(
                StatusCode::FAILED_DEPENDENCY,
                "cloudflare",
                "creating the SSH certificate authority: Cloudflare returned no public key",
            )
        })
    }

    pub async fn default_virtual_network(&self, account_id: &str) -> Result<String, ApiError> {
        let result = self
            .call(
                Method::GET,
                &format!("/accounts/{account_id}/teamnet/virtual_networks"),
                None,
            )
            .await
            .map_err(|e| e.into_api("reading virtual networks"))?;
        let networks = items(&result);
        let pick = networks
            .iter()
            .find(|network| {
                network
                    .get("is_default_network")
                    .or_else(|| network.get("is_default"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .or_else(|| networks.first());
        pick.and_then(|network| network.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::FAILED_DEPENDENCY,
                    "cloudflare",
                    "reading virtual networks: the account has none",
                )
            })
    }

    /// Route one address through the tunnel.
    pub async fn ensure_route(
        &self,
        account_id: &str,
        tunnel_id: &str,
        address: &str,
        vnet: &str,
    ) -> Result<(), ApiError> {
        let outcome = self
            .call(
                Method::POST,
                &format!("/accounts/{account_id}/teamnet/routes"),
                Some(&json!({
                    "network": format!("{address}/32"),
                    "tunnel_id": tunnel_id,
                    "virtual_network_id": vnet,
                    "comment": "Rudder SSH",
                })),
            )
            .await;
        match outcome {
            Ok(_) => Ok(()),
            Err(error) if error.already_exists() => Ok(()),
            Err(error) => Err(error.into_api("creating the tunnel route")),
        }
    }

    pub async fn delete_route(
        &self,
        account_id: &str,
        address: &str,
        vnet: &str,
    ) -> Result<(), ApiError> {
        self.call(
            Method::DELETE,
            &format!(
                "/accounts/{account_id}/teamnet/routes/network/{address}%2F32?virtual_network_id={vnet}"
            ),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("deleting the tunnel route"))
    }

    /// The infrastructure target for the server, created when missing.
    pub async fn ensure_target(
        &self,
        account_id: &str,
        hostname: &str,
        address: &str,
        vnet: &str,
    ) -> Result<String, ApiError> {
        let created = self
            .call(
                Method::POST,
                &format!("/accounts/{account_id}/infrastructure/targets"),
                Some(&json!({
                    "hostname": hostname,
                    "ip": { "ipv4": { "ip_addr": address, "virtual_network_id": vnet } },
                })),
            )
            .await;
        match created {
            Ok(result) => result
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::FAILED_DEPENDENCY,
                        "cloudflare",
                        "creating the SSH target: Cloudflare returned no target id",
                    )
                }),
            Err(error) if error.already_exists() => {
                let listed = self
                    .call(
                        Method::GET,
                        &format!(
                            "/accounts/{account_id}/infrastructure/targets?hostname={hostname}"
                        ),
                        None,
                    )
                    .await
                    .map_err(|e| e.into_api("reading the SSH target"))?;
                items(&listed)
                    .into_iter()
                    .find(|target| target.get("hostname").and_then(Value::as_str) == Some(hostname))
                    .and_then(|target| target.get("id").and_then(Value::as_str))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        ApiError::new(
                            StatusCode::FAILED_DEPENDENCY,
                            "cloudflare",
                            "reading the SSH target: it exists but could not be found",
                        )
                    })
            }
            Err(error) => Err(error.into_api("creating the SSH target")),
        }
    }

    pub async fn delete_target(&self, account_id: &str, target_id: &str) -> Result<(), ApiError> {
        self.call(
            Method::DELETE,
            &format!("/accounts/{account_id}/infrastructure/targets/{target_id}"),
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| e.into_api("deleting the SSH target"))
    }
}

/// The elements of a list result, whether Cloudflare returns an array or an
/// object wrapping one.
fn items(result: &Value) -> Vec<&Value> {
    match result {
        Value::Array(list) => list.iter().collect(),
        Value::Object(map) => map
            .values()
            .find_map(Value::as_array)
            .map(|list| list.iter().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A connector reports its connections in `conns`; a flat list counts each entry.
fn count_connections(result: &Value) -> usize {
    items(result)
        .into_iter()
        .map(|connector| {
            connector
                .get("conns")
                .and_then(Value::as_array)
                .map_or(1, Vec::len)
        })
        .sum()
}

/// Everything saved about the optional Cloudflare mode. Secrets stay encrypted.
#[derive(sqlx::FromRow, Clone)]
pub struct Settings {
    pub account_id: String,
    pub zone_id: String,
    pub zone_name: String,
    pub api_token_enc: Option<Vec<u8>>,
    pub tunnel_id: Option<String>,
    pub tunnel_name: String,
    pub tunnel_token_enc: Option<Vec<u8>>,
    pub replicas: i32,
    pub tunnel_published: bool,
    pub published_hosts: Vec<String>,
    pub team_domain: String,
    pub access_emails: Vec<String>,
    pub access_app_id: Option<String>,
    pub access_aud: Option<String>,
    pub access_enforced: bool,
    pub agent_bypass: bool,
    pub agent_bypass_app_ids: Vec<String>,
    pub ssh_address: String,
    pub ssh_users: Vec<String>,
    pub ssh_target_id: Option<String>,
    pub ssh_app_id: Option<String>,
    pub ssh_ca_public_key: String,
    pub ssh_ready: bool,
}

pub async fn load(pool: &PgPool) -> Result<Settings, ApiError> {
    sqlx::query_as::<_, Settings>(
        r#"SELECT account_id, zone_id, zone_name, api_token_enc, tunnel_id, tunnel_name,
                  tunnel_token_enc, replicas, tunnel_published, published_hosts,
                  team_domain, access_emails, access_app_id, access_aud, access_enforced,
                  agent_bypass, agent_bypass_app_ids, ssh_address, ssh_users, ssh_target_id,
                  ssh_app_id, ssh_ca_public_key, ssh_ready
           FROM cloudflare_settings WHERE id = TRUE"#,
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)
}

fn decrypt(
    state: &AppState,
    blob: &Option<Vec<u8>>,
    what: &'static str,
) -> Result<String, ApiError> {
    let blob = blob
        .as_deref()
        .ok_or_else(|| bad("not_configured", format!("the {what} is not saved yet")))?;
    let bytes = state.cipher.decrypt(blob).map_err(core_error)?;
    String::from_utf8(bytes).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "crypto failure",
        )
    })
}

/// API client built from the saved credentials.
pub fn client(state: &AppState, settings: &Settings) -> Result<Cf, ApiError> {
    let token = decrypt(state, &settings.api_token_enc, "Cloudflare API token")?;
    Cf::new(&state.config.cloudflare_api_url, &token)
}

pub fn tunnel_token(state: &AppState, settings: &Settings) -> Result<String, ApiError> {
    decrypt(state, &settings.tunnel_token_enc, "tunnel token")
}

pub fn require_tunnel(settings: &Settings) -> Result<&str, ApiError> {
    settings
        .tunnel_id
        .as_deref()
        .ok_or_else(|| bad("no_tunnel", "create the tunnel first"))
}

// ---------------------------------------------------------------- replicas

/// How many of the tunnel's replica containers are running.
pub async fn replicas_running(state: &AppState) -> usize {
    let mut running = 0;
    for index in 1..=MAX_TUNNEL_REPLICAS {
        let name = replica_container_name(index);
        if state
            .local_driver
            .container_running(&name)
            .await
            .unwrap_or(false)
        {
            running += 1;
        }
    }
    running
}

/// Bring the number of replicas to `wanted`. Replicas that already run are left
/// alone, so scaling never drops live connections.
pub async fn scale_replicas(
    state: &AppState,
    token: &str,
    wanted: u8,
    restart_all: bool,
) -> Result<(), ApiError> {
    let driver = &state.local_driver;
    driver
        .pull_image(&state.config.cloudflared_image, None)
        .await
        .map_err(core_error)?;
    for index in 1..=MAX_TUNNEL_REPLICAS {
        let name = replica_container_name(index);
        let running = driver.container_running(&name).await.unwrap_or(false);
        if index > wanted {
            let _ = driver.remove_container(&name, true).await;
            continue;
        }
        if running && !restart_all {
            continue;
        }
        let _ = driver.remove_container(&name, true).await;
        let spec =
            cloudflared_spec(token, &state.config.cloudflared_image, index).map_err(core_error)?;
        let id = driver.create_container(&spec).await.map_err(core_error)?;
        driver.start_container(&id).await.map_err(core_error)?;
    }
    Ok(())
}

pub async fn remove_replicas(state: &AppState) {
    for index in 1..=MAX_TUNNEL_REPLICAS {
        let _ = state
            .local_driver
            .remove_container(&replica_container_name(index), true)
            .await;
    }
}

// --------------------------------------------------------------- hostnames

#[derive(Serialize, ToSchema, Clone)]
pub struct SkippedHost {
    pub host: String,
    pub reason: String,
}

/// Hostnames the local server answers for: app domains, the panel domain, and
/// the Rudder Access portal.
pub async fn wanted_hosts(
    pool: &PgPool,
    zone: &str,
) -> Result<(Vec<String>, Vec<SkippedHost>), ApiError> {
    let rows: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT lower(d.host) FROM domains d
        JOIN services s ON s.id = d.service_id
        JOIN servers sv ON sv.id = s.server_id
        WHERE sv.kind = 'local'
        UNION
        SELECT lower(panel_domain) FROM panel_settings
        WHERE id = TRUE AND panel_domain IS NOT NULL AND panel_domain <> ''
        UNION
        SELECT lower(portal_host) FROM access_settings
        WHERE id = TRUE AND portal_host IS NOT NULL AND portal_host <> ''
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;
    let mut hosts = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for host in rows {
        if !seen.insert(host.clone()) {
            continue;
        }
        if host.starts_with('*') || validate_host(&host).is_err() {
            skipped.push(SkippedHost {
                host,
                reason: "wildcard or unusual names are not published through the tunnel".into(),
            });
        } else if !host_in_zone(&host, zone) {
            skipped.push(SkippedHost {
                host,
                reason: format!("not in the {zone} zone"),
            });
        } else {
            hosts.push(host);
        }
    }
    hosts.sort();
    skipped.sort_by(|a, b| a.host.cmp(&b.host));
    Ok((hosts, skipped))
}

#[derive(Serialize, ToSchema, Clone)]
pub struct DnsOutcome {
    pub host: String,
    /// `created`, `kept`, `replaced`, or `conflict`.
    pub action: String,
    pub detail: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SyncReport {
    pub hostnames: Vec<String>,
    pub skipped: Vec<SkippedHost>,
    pub dns: Vec<DnsOutcome>,
    /// Hostnames whose DNS record Rudder did not change.
    pub pending: usize,
}

/// Push the hostnames to the tunnel, point their DNS at it, and switch Traefik
/// to the tunnel variant.
pub async fn sync(state: &AppState, overwrite_dns: bool) -> Result<SyncReport, ApiError> {
    let settings = load(&state.pool).await?;
    let tunnel_id = require_tunnel(&settings)?.to_owned();
    if settings.zone_id.is_empty() {
        return Err(bad("not_configured", "save the Cloudflare zone first"));
    }
    let cf = client(state, &settings)?;
    let (hosts, skipped) = wanted_hosts(&state.pool, &settings.zone_name).await?;
    let config = tunnel_config(&hosts).map_err(core_error)?;
    cf.put_tunnel_config(&settings.account_id, &tunnel_id, &config)
        .await?;

    let mut existing = Vec::new();
    for host in &hosts {
        existing.extend(cf.dns_records(&settings.zone_id, host).await?);
    }
    let plan = plan_dns(&hosts, &tunnel_id, &existing, overwrite_dns);
    let mut dns = Vec::new();
    let mut published = Vec::new();
    for action in plan {
        match action {
            DnsAction::Create { host } => {
                cf.create_cname(&settings.zone_id, &host, &tunnel_id)
                    .await?;
                published.push(host.clone());
                dns.push(DnsOutcome {
                    host,
                    action: "created".into(),
                    detail: None,
                });
            }
            DnsAction::Keep { host } => {
                published.push(host.clone());
                dns.push(DnsOutcome {
                    host,
                    action: "kept".into(),
                    detail: None,
                });
            }
            DnsAction::Replace {
                host,
                record_id,
                was,
            } => {
                cf.delete_dns(&settings.zone_id, &record_id).await?;
                cf.create_cname(&settings.zone_id, &host, &tunnel_id)
                    .await?;
                published.push(host.clone());
                dns.push(DnsOutcome {
                    host,
                    action: "replaced".into(),
                    detail: Some(was),
                });
            }
            DnsAction::Conflict { host, was } => dns.push(DnsOutcome {
                host,
                action: "conflict".into(),
                detail: Some(was),
            }),
        }
    }
    // Hostnames that left the list lose the record Rudder made for them.
    for stale in settings
        .published_hosts
        .iter()
        .filter(|host| !hosts.contains(host))
    {
        for record in cf.dns_records(&settings.zone_id, stale).await? {
            if record.kind == "CNAME"
                && record
                    .content
                    .eq_ignore_ascii_case(&tunnel_cname(&tunnel_id))
            {
                cf.delete_dns(&settings.zone_id, &record.id).await?;
            }
        }
    }
    published.sort();
    sqlx::query(
        r#"UPDATE cloudflare_settings
           SET tunnel_published = TRUE, published_hosts = $1, updated_at = now()
           WHERE id = TRUE"#,
    )
    .bind(&published)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    crate::publish::publish_local(state).await?;
    let pending = dns.iter().filter(|d| d.action == "conflict").count();
    Ok(SyncReport {
        hostnames: hosts,
        skipped,
        dns,
        pending,
    })
}

// ----------------------------------------------------- Access token checks

struct Guard {
    host: String,
    issuer: String,
    audience: String,
    bypass: bool,
}

/// Panel-side state for the optional mode: the enforcement settings and the
/// team's signing keys, both cached briefly.
pub struct Runtime {
    guard: RwLock<Option<(Instant, Option<Guard>)>>,
    jwks: Mutex<Option<(String, Instant, String)>>,
    http: reqwest::Client,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            guard: RwLock::new(None),
            jwks: Mutex::new(None),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
        }
    }
}

/// Paths an AI agent or API client uses with its own bearer token. They skip
/// Access only when the operator turned the agent bypass on.
pub const BYPASS_PATHS: &[&str] = &["/mcp", "/.well-known", "/oauth", "/api/health"];

pub fn bypassed(path: &str) -> bool {
    BYPASS_PATHS.iter().any(|prefix| {
        path == *prefix
            || path
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

impl Runtime {
    /// Drop cached settings so the next request reads them again.
    pub async fn invalidate(&self) {
        *self.guard.write().await = None;
    }

    async fn guard_for(&self, pool: &PgPool, host: &str) -> Option<(String, String, bool)> {
        {
            let cached = self.guard.read().await;
            if let Some((loaded, entry)) = cached.as_ref() {
                if loaded.elapsed() < GUARD_TTL {
                    return entry
                        .as_ref()
                        .filter(|g| g.host == host)
                        .map(|g| (g.issuer.clone(), g.audience.clone(), g.bypass));
                }
            }
        }
        let row = sqlx::query_as::<_, (bool, String, Option<String>, bool, Option<String>)>(
            r#"SELECT c.access_enforced, c.team_domain, c.access_aud, c.agent_bypass,
                      NULLIF(p.panel_domain, '')
               FROM cloudflare_settings c, panel_settings p
               WHERE c.id = TRUE AND p.id = TRUE"#,
        )
        .fetch_one(pool)
        .await
        .ok();
        let loaded = row.and_then(|(enforced, team, aud, bypass, domain)| {
            let aud = aud.filter(|aud| !aud.is_empty())?;
            let domain = domain?.to_ascii_lowercase();
            (enforced && !team.is_empty()).then(|| Guard {
                host: domain,
                issuer: rudder_core::access_issuer(&team),
                audience: aud,
                bypass,
            })
        });
        let answer = loaded
            .as_ref()
            .filter(|g| g.host == host)
            .map(|g| (g.issuer.clone(), g.audience.clone(), g.bypass));
        *self.guard.write().await = Some((Instant::now(), loaded));
        answer
    }

    async fn keys(&self, issuer: &str) -> Option<String> {
        let mut cache = self.jwks.lock().await;
        if let Some((cached_issuer, fetched, keys)) = cache.as_ref() {
            if cached_issuer == issuer && fetched.elapsed() < JWKS_TTL {
                return Some(keys.clone());
            }
        }
        let url = format!("{issuer}/cdn-cgi/access/certs");
        let fetched = async {
            let response = self.http.get(&url).send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.text().await.ok()
        }
        .await;
        match fetched {
            Some(keys) => {
                *cache = Some((issuer.to_owned(), Instant::now(), keys.clone()));
                Some(keys)
            }
            // A stale key set is better than locking the operator out.
            None => cache
                .as_ref()
                .filter(|(cached_issuer, _, _)| cached_issuer == issuer)
                .map(|(_, _, keys)| keys.clone()),
        }
    }

    /// Whether `token` is a valid Access token for this team and application.
    /// Used before enforcement is turned on, to prove Access works end to end.
    pub async fn verify(&self, team_domain: &str, audience: &str, token: &str) -> bool {
        let issuer = rudder_core::access_issuer(team_domain);
        let Some(keys) = self.keys(&issuer).await else {
            return false;
        };
        rudder_core::verify_access_jwt(
            token,
            &keys,
            &issuer,
            audience,
            chrono::Utc::now().timestamp(),
        )
        .is_ok()
    }

    /// Decide whether a request for the panel domain may proceed.
    pub async fn check(
        &self,
        pool: &PgPool,
        host: &str,
        path: &str,
        token: Option<&str>,
    ) -> Verdict {
        let Some((issuer, audience, bypass)) = self.guard_for(pool, host).await else {
            return Verdict::NotGuarded;
        };
        if bypass && bypassed(path) {
            return Verdict::NotGuarded;
        }
        let Some(token) = token else {
            return Verdict::Denied;
        };
        let Some(keys) = self.keys(&issuer).await else {
            return Verdict::Unavailable;
        };
        match rudder_core::verify_access_jwt(
            token,
            &keys,
            &issuer,
            &audience,
            chrono::Utc::now().timestamp(),
        ) {
            Ok(_) => Verdict::Allowed,
            Err(_) => Verdict::Denied,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The request is not for the guarded host, or the path is exempt.
    NotGuarded,
    Allowed,
    Denied,
    /// The team's keys could not be fetched and none are cached.
    Unavailable,
}

/// Rejects requests for the panel domain that did not come through Cloudflare Access.
pub async fn enforce_access(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(':').next().unwrap_or("").to_ascii_lowercase())
        .unwrap_or_default();
    if host.is_empty() {
        return next.run(request).await;
    }
    let token = request
        .headers()
        .get("cf-access-jwt-assertion")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let verdict = state
        .cloudflare
        .check(&state.pool, &host, request.uri().path(), token.as_deref())
        .await;
    match verdict {
        Verdict::NotGuarded | Verdict::Allowed => next.run(request).await,
        Verdict::Denied => (
            StatusCode::FORBIDDEN,
            "This panel is protected by Cloudflare Access. Sign in through Cloudflare first.",
        )
            .into_response(),
        Verdict::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Cloudflare Access keys are not reachable right now.",
        )
            .into_response(),
    }
}

/// Align process-wide settings with the saved mode. Called at startup and after
/// every change.
pub async fn refresh_mode(state: &AppState) {
    let published: bool =
        sqlx::query_scalar("SELECT tunnel_published FROM cloudflare_settings WHERE id = TRUE")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(false);
    crate::auth::set_tunnel_mode(published);
    state.cloudflare.invalidate().await;
}

/// Whether the local server should render Traefik for the tunnel.
pub async fn tunnel_published(pool: &PgPool) -> bool {
    sqlx::query_scalar("SELECT tunnel_published FROM cloudflare_settings WHERE id = TRUE")
        .fetch_one(pool)
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_paths_match_whole_segments_only() {
        assert!(bypassed("/mcp"));
        assert!(bypassed("/mcp/"));
        assert!(bypassed("/oauth/token"));
        assert!(bypassed("/.well-known/oauth-authorization-server"));
        assert!(!bypassed("/mcpx"));
        assert!(!bypassed("/api/v1/users"));
        assert!(!bypassed("/oauthish"));
    }

    #[test]
    fn counts_connections_per_connector() {
        let result = json!([
            { "id": "a", "conns": [{ "id": "1" }, { "id": "2" }] },
            { "id": "b", "conns": [{ "id": "3" }] },
        ]);
        assert_eq!(count_connections(&result), 3);
        assert_eq!(count_connections(&json!([])), 0);
        assert_eq!(count_connections(&json!([{ "id": "x" }])), 1);
    }

    #[test]
    fn list_results_may_be_wrapped() {
        let wrapped = json!({ "targets": [{ "id": "1" }], "count": 1 });
        assert_eq!(items(&wrapped).len(), 1);
        assert!(items(&json!(null)).is_empty());
    }
}
