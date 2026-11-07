//! The optional Cloudflare Zero Trust mode against a fake Cloudflare API: saving
//! credentials, the tunnel, publishing hostnames and DNS, Access in front of the
//! panel, SSH through Access for Infrastructure, the readiness report, and
//! removing everything again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, Request as AxumRequest, State};
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Json;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const ZONE: &str = "fedcba9876543210fedcba9876543210";
const OTHER_ZONE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TOKEN: &str = "cfapitoken_1234567890";
const TUNNEL: &str = "4f1c5f1e-2f64-4a5b-9d0f-3a8e7d9c1b22";
const CA_KEY: &str =
    "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBExample";

#[derive(Default)]
struct Fake {
    dns: Vec<Value>,
    configs: Vec<Value>,
    apps: HashMap<String, Value>,
    calls: Vec<String>,
    next: u32,
}

type Shared = Arc<Mutex<Fake>>;

fn ok(result: Value) -> Json<Value> {
    Json(json!({ "success": true, "errors": [], "result": result }))
}

fn record(fake: &Shared, call: impl Into<String>) {
    fake.lock().unwrap().calls.push(call.into());
}

async fn authenticate(State(fake): State<Shared>, request: AxumRequest, next: Next) -> Response {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let path = request.uri().path().to_owned();
    record(&fake, format!("{} {}", request.method(), path));
    if header == "Bearer denied" && path.contains("/cfd_tunnel") {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "errors": [{ "code": 10000, "message": "Authentication error" }],
            })),
        )
            .into_response();
    }
    if header != format!("Bearer {TOKEN}") && header != "Bearer denied" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "success": false,
                "errors": [{ "code": 9109, "message": "Invalid access token" }],
            })),
        )
            .into_response();
    }
    next.run(request).await
}

async fn zone(Path(id): Path<String>) -> Json<Value> {
    let account = if id == OTHER_ZONE {
        "ffffffffffffffffffffffffffffffff"
    } else {
        ACCOUNT
    };
    ok(json!({ "id": id, "name": "example.com", "account": { "id": account } }))
}

async fn empty_list() -> Json<Value> {
    ok(json!([]))
}

async fn create_tunnel(State(fake): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    record(&fake, format!("tunnel {}", body["config_src"]));
    ok(json!({ "id": TUNNEL, "name": body["name"] }))
}

async fn tunnel_token() -> Json<Value> {
    ok(json!("dGVzdC10dW5uZWwtdG9rZW4="))
}

async fn put_config(State(fake): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    fake.lock().unwrap().configs.push(body.clone());
    ok(body)
}

async fn connections() -> Json<Value> {
    ok(json!([{ "id": "a", "conns": [{ "id": "1" }, { "id": "2" }] }]))
}

async fn nothing() -> Json<Value> {
    ok(Value::Null)
}

async fn list_dns(
    State(fake): State<Shared>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let name = query.get("name").cloned().unwrap_or_default();
    let found: Vec<Value> = fake
        .lock()
        .unwrap()
        .dns
        .iter()
        .filter(|record| record["name"] == name)
        .cloned()
        .collect();
    ok(Value::Array(found))
}

async fn create_dns(State(fake): State<Shared>, Json(mut body): Json<Value>) -> Json<Value> {
    let mut fake = fake.lock().unwrap();
    fake.next += 1;
    body["id"] = json!(format!("dns-{}", fake.next));
    fake.dns.push(body.clone());
    ok(body)
}

async fn delete_dns(
    State(fake): State<Shared>,
    Path((_, id)): Path<(String, String)>,
) -> Json<Value> {
    fake.lock().unwrap().dns.retain(|record| record["id"] != id);
    ok(json!({ "id": id }))
}

async fn create_app(State(fake): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    let id = Uuid::new_v4().to_string();
    let mut stored = body.clone();
    stored["id"] = json!(id);
    stored["aud"] = json!(format!("aud-{}", &id[..8]));
    fake.lock().unwrap().apps.insert(id, stored.clone());
    ok(stored)
}

async fn update_app(
    State(fake): State<Shared>,
    Path((_, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let mut fake = fake.lock().unwrap();
    let aud = fake.apps.get(&id).map(|app| app["aud"].clone());
    let mut stored = body;
    stored["id"] = json!(id);
    stored["aud"] = aud.unwrap_or(json!("aud-new"));
    fake.apps.insert(id, stored.clone());
    ok(stored)
}

async fn delete_app(
    State(fake): State<Shared>,
    Path((_, id)): Path<(String, String)>,
) -> Json<Value> {
    fake.lock().unwrap().apps.remove(&id);
    ok(json!({ "id": id }))
}

/// The real API reports an account without an SSH CA as an error, not an empty list.
async fn gateway_ca_list() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "success": false,
            "errors": [{ "code": 12112, "message": "access.api.error.gateway_ca_not_found" }],
            "result": null
        })),
    )
}

async fn gateway_ca_create() -> Json<Value> {
    ok(json!({ "id": "ca", "public_key": CA_KEY }))
}

async fn virtual_networks() -> Json<Value> {
    ok(json!([
        { "id": "11111111-1111-1111-1111-111111111111", "is_default_network": false },
        { "id": "22222222-2222-2222-2222-222222222222", "is_default_network": true },
    ]))
}

async fn create_route(State(fake): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    record(
        &fake,
        format!(
            "route {} via {} on {}",
            body["network"], body["tunnel_id"], body["virtual_network_id"]
        ),
    );
    ok(body)
}

async fn delete_route(
    State(fake): State<Shared>,
    Path((_, cidr)): Path<(String, String)>,
) -> Json<Value> {
    record(&fake, format!("route deleted {cidr}"));
    ok(Value::Null)
}

async fn create_target(State(fake): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    record(&fake, format!("target {}", body["hostname"]));
    ok(json!({ "id": "target-1", "hostname": body["hostname"] }))
}

async fn delete_target(
    State(fake): State<Shared>,
    Path((_, id)): Path<(String, String)>,
) -> Json<Value> {
    record(&fake, format!("target deleted {id}"));
    ok(Value::Null)
}

async fn fake_cloudflare() -> (String, Shared) {
    let fake = Shared::default();
    let router = axum::Router::new()
        .route("/zones/{id}", get(zone))
        .route("/zones/{id}/dns_records", get(list_dns).post(create_dns))
        .route("/zones/{id}/dns_records/{record}", delete(delete_dns))
        .route(
            "/accounts/{account}/cfd_tunnel",
            get(empty_list).post(create_tunnel),
        )
        .route("/accounts/{account}/cfd_tunnel/{id}", delete(nothing))
        .route(
            "/accounts/{account}/cfd_tunnel/{id}/token",
            get(tunnel_token),
        )
        .route(
            "/accounts/{account}/cfd_tunnel/{id}/configurations",
            put(put_config),
        )
        .route(
            "/accounts/{account}/cfd_tunnel/{id}/connections",
            get(connections).delete(nothing),
        )
        .route(
            "/accounts/{account}/access/apps",
            get(empty_list).post(create_app),
        )
        .route(
            "/accounts/{account}/access/apps/{id}",
            put(update_app).delete(delete_app),
        )
        .route(
            "/accounts/{account}/access/gateway_ca",
            get(gateway_ca_list).post(gateway_ca_create),
        )
        .route(
            "/accounts/{account}/teamnet/virtual_networks",
            get(virtual_networks),
        )
        .route("/accounts/{account}/teamnet/routes", post(create_route))
        .route(
            "/accounts/{account}/teamnet/routes/network/{cidr}",
            delete(delete_route),
        )
        .route(
            "/accounts/{account}/infrastructure/targets",
            post(create_target),
        )
        .route(
            "/accounts/{account}/infrastructure/targets/{id}",
            delete(delete_target),
        )
        .layer(axum::middleware::from_fn_with_state(
            fake.clone(),
            authenticate,
        ))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, fake)
}

fn app(pool: PgPool, data_dir: &std::path::Path, cloudflare: &str) -> axum::Router {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    let vars = HashMap::from([
        ("DATABASE_URL".into(), url),
        ("RUDDER_MASTER_KEY".into(), "cc".repeat(32)),
        ("RUDDER_COOKIE_SECURE".into(), "false".into()),
        ("RUDDER_DATA_DIR".into(), data_dir.display().to_string()),
        ("RUDDER_CLOUDFLARE_API_URL".into(), cloudflare.to_owned()),
        // These assertions describe a host without Docker, even when one is running.
        (
            "RUDDER_DOCKER_SOCKET".into(),
            data_dir.join("no-docker.sock").display().to_string(),
        ),
    ]);
    let config = rudder_server::Config::from_map(&vars, |_| unreachable!()).unwrap();
    rudder_server::router(rudder_server::AppState::new(pool, config))
}

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn call_with(
    app: &axum::Router,
    method: &str,
    path: &str,
    session: Option<&(String, String)>,
    body: Option<Value>,
    host: Option<&str>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some((cookie, csrf)) = session {
        builder = builder
            .header("cookie", format!("rudder_session={cookie}"))
            .header("x-csrf-token", csrf);
    }
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    session: &(String, String),
    body: Option<Value>,
) -> Reply {
    call_with(app, method, path, Some(session), body, None).await
}

async fn owner(app: &axum::Router) -> (String, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "email": "owner@example.com",
                        "password": "correct-horse-battery",
                        "display_name": "Owner"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .and_then(|pair| pair.strip_prefix("rudder_session="))
        .unwrap()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    (cookie, body["csrf_token"].as_str().unwrap().to_owned())
}

fn scratch_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rudder-cloudflare-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn calls(fake: &Shared) -> Vec<String> {
    fake.lock().unwrap().calls.clone()
}

#[sqlx::test(migrations = "./migrations")]
async fn credentials_are_checked_and_never_returned(pool: PgPool) {
    let (cloudflare, fake) = fake_cloudflare().await;
    let data_dir = scratch_dir();
    let app = app(pool.clone(), &data_dir, &cloudflare);

    let anonymous = call_with(&app, "GET", "/api/v1/cloudflare", None, None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let session = owner(&app).await;
    let fresh = call(&app, "GET", "/api/v1/cloudflare", &session, None).await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
    assert_eq!(fresh.body["token_saved"], false);
    assert_eq!(fresh.body["tunnel_id"], Value::Null);
    assert_eq!(fresh.body["max_replicas"], 5);

    let save = |account: &str, zone: &str, token: Option<&str>| {
        let mut body = json!({ "account_id": account, "zone_id": zone });
        if let Some(token) = token {
            body["api_token"] = json!(token);
        }
        body
    };
    let short = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save("abc", ZONE, Some(TOKEN))),
    )
    .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);
    let missing = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save(ACCOUNT, ZONE, None)),
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    let foreign = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save(ACCOUNT, OTHER_ZONE, Some(TOKEN))),
    )
    .await;
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST);
    assert_eq!(foreign.body["error"]["code"], "zone_account_mismatch");
    let denied = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save(ACCOUNT, ZONE, Some("denied"))),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FAILED_DEPENDENCY);
    assert_eq!(denied.body["error"]["code"], "cloudflare_permission");
    assert!(
        denied.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("listing tunnels"),
        "{}",
        denied.body
    );
    let wrong = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save(ACCOUNT, ZONE, Some("nottheone"))),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::FAILED_DEPENDENCY);
    assert!(!wrong.body.to_string().contains("nottheone"));

    let saved = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save(ACCOUNT, ZONE, Some(TOKEN))),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["token_saved"], true);
    assert_eq!(saved.body["zone_name"], "example.com");
    assert!(!saved.body.to_string().contains(TOKEN));
    // The saved token is reused when the field is left out.
    let again = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(save(ACCOUNT, ZONE, None)),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);

    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT api_token_enc FROM cloudflare_settings WHERE id = TRUE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&stored).contains(TOKEN));
    assert!(calls(&fake)
        .iter()
        .any(|call| call.starts_with("GET /zones/")));
}

#[sqlx::test(migrations = "./migrations")]
async fn the_whole_mode_can_be_set_up_and_removed(pool: PgPool) {
    let (cloudflare, fake) = fake_cloudflare().await;
    let data_dir = scratch_dir();
    let app = app(pool.clone(), &data_dir, &cloudflare);
    let session = owner(&app).await;

    // A local server with one app domain inside the zone and one outside it.
    let server = call(
        &app,
        "POST",
        "/api/v1/servers",
        &session,
        Some(json!({ "name": "local", "kind": "local", "public_ip": "203.0.113.10" })),
    )
    .await;
    assert_eq!(server.status, StatusCode::CREATED);
    let project = call(
        &app,
        "POST",
        "/api/v1/projects",
        &session,
        Some(json!({ "name": "web" })),
    )
    .await;
    let project_id = project.body["id"].as_str().unwrap().to_owned();
    let environment = call(
        &app,
        "POST",
        &format!("/api/v1/projects/{project_id}/environments"),
        &session,
        Some(json!({ "name": "prod" })),
    )
    .await;
    let environment_id = environment.body["id"].as_str().unwrap().to_owned();
    let service = call(
        &app,
        "POST",
        &format!("/api/v1/environments/{environment_id}/services"),
        &session,
        Some(json!({
            "name": "web",
            "kind": "application",
            "server_id": server.body["id"],
            "spec": { "image": "nginx:1.27" }
        })),
    )
    .await;
    let service_id = service.body["id"].as_str().unwrap().to_owned();
    for host in ["app.example.com", "app.other.org"] {
        let domain = call(
            &app,
            "POST",
            &format!("/api/v1/services/{service_id}/domains"),
            &session,
            Some(json!({ "host": host, "target_port": 8080, "ssl_mode": "letsencrypt" })),
        )
        .await;
        assert_eq!(domain.status, StatusCode::CREATED, "{}", domain.body);
    }
    sqlx::query("UPDATE panel_settings SET panel_domain = 'panel.example.com' WHERE id = TRUE")
        .execute(&pool)
        .await
        .unwrap();
    let routes_path = data_dir.join("traefik/dynamic/routes.yml");
    let direct = std::fs::read_to_string(&routes_path).unwrap();
    assert!(direct.contains("certResolver"), "{direct}");

    let credentials = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/credentials",
        &session,
        Some(json!({ "account_id": ACCOUNT, "zone_id": ZONE, "api_token": TOKEN })),
    )
    .await;
    assert_eq!(credentials.status, StatusCode::OK, "{}", credentials.body);

    // The tunnel is created at Cloudflare and saved even without Docker; the
    // status says why no replica runs.
    let too_many = call(
        &app,
        "POST",
        "/api/v1/cloudflare/tunnel",
        &session,
        Some(json!({ "replicas": 9 })),
    )
    .await;
    assert_eq!(too_many.status, StatusCode::BAD_REQUEST);
    let tunnel = call(
        &app,
        "POST",
        "/api/v1/cloudflare/tunnel",
        &session,
        Some(json!({ "replicas": 3 })),
    )
    .await;
    assert_eq!(tunnel.status, StatusCode::CREATED, "{}", tunnel.body);
    assert_eq!(tunnel.body["tunnel_id"], TUNNEL);
    assert_eq!(tunnel.body["replicas"], 3);
    assert_eq!(tunnel.body["replicas_running"], 0);
    assert_eq!(tunnel.body["connections"], 2);
    assert!(tunnel.body["replica_error"].is_string(), "{}", tunnel.body);
    assert!(!tunnel.body.to_string().contains("dGVzdC10dW5uZWwtdG9rZW4"));
    let twice = call(
        &app,
        "POST",
        "/api/v1/cloudflare/tunnel",
        &session,
        Some(json!({})),
    )
    .await;
    assert_eq!(twice.status, StatusCode::CONFLICT);
    let scaled = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/tunnel/replicas",
        &session,
        Some(json!({ "replicas": 0 })),
    )
    .await;
    assert_eq!(scaled.status, StatusCode::BAD_REQUEST);

    // An A record already sits on the app hostname.
    fake.lock().unwrap().dns.push(json!({
        "id": "old-a", "type": "A", "name": "app.example.com", "content": "203.0.113.10",
    }));
    let first = call(
        &app,
        "POST",
        "/api/v1/cloudflare/tunnel/publish",
        &session,
        Some(json!({})),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(
        first.body["hostnames"],
        json!(["app.example.com", "panel.example.com"])
    );
    assert_eq!(first.body["skipped"][0]["host"], "app.other.org");
    assert_eq!(first.body["pending"], 1);
    let actions: HashMap<String, String> = first.body["dns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["host"].as_str().unwrap().to_owned(),
                item["action"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(actions["app.example.com"], "conflict");
    assert_eq!(actions["panel.example.com"], "created");
    assert!(fake
        .lock()
        .unwrap()
        .dns
        .iter()
        .any(|record| record["id"] == "old-a"));
    let config = fake.lock().unwrap().configs.last().cloned().unwrap();
    let ingress = config["config"]["ingress"].as_array().unwrap();
    assert_eq!(ingress.len(), 3);
    assert_eq!(ingress[0]["hostname"], "app.example.com");
    assert_eq!(ingress[0]["service"], "https://rudder-traefik:443");
    assert_eq!(ingress[2]["service"], "http_status:404");
    let tunnelled = std::fs::read_to_string(&routes_path).unwrap();
    assert!(!tunnelled.contains("certResolver"), "{tunnelled}");
    assert!(tunnelled.contains("tls: {}"));

    let report = call(&app, "GET", "/api/v1/cloudflare/preflight", &session, None).await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    assert_eq!(report.body["ready"], false);
    let failing: Vec<&str> = report.body["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["ok"] == false)
        .map(|check| check["id"].as_str().unwrap())
        .collect();
    assert!(failing.contains(&"published"));
    assert!(failing.contains(&"access"));
    assert!(failing.contains(&"replicas"));

    let second = call(
        &app,
        "POST",
        "/api/v1/cloudflare/tunnel/publish",
        &session,
        Some(json!({ "overwrite_dns": true })),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(second.body["pending"], 0);
    {
        let fake = fake.lock().unwrap();
        assert_eq!(fake.dns.len(), 2);
        assert!(fake.dns.iter().all(|record| {
            record["type"] == "CNAME"
                && record["content"] == format!("{TUNNEL}.cfargotunnel.com")
                && record["proxied"] == true
        }));
    }
    let settled = call(&app, "GET", "/api/v1/cloudflare", &session, None).await;
    assert_eq!(settled.body["tunnel_published"], true);
    assert_eq!(
        settled.body["published_hosts"],
        json!(["app.example.com", "panel.example.com"])
    );

    // Access in front of the panel.
    for body in [
        json!({ "team_domain": "bad domain!", "emails": ["a@example.com"] }),
        json!({ "team_domain": "https://myteam.cloudflareaccess.com/", "emails": [] }),
        json!({ "team_domain": "https://myteam.cloudflareaccess.com/", "emails": ["not-an-email"] }),
    ] {
        let rejected = call(
            &app,
            "PUT",
            "/api/v1/cloudflare/access",
            &session,
            Some(body),
        )
        .await;
        assert_eq!(
            rejected.status,
            StatusCode::BAD_REQUEST,
            "{}",
            rejected.body
        );
    }
    let access = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/access",
        &session,
        Some(json!({
            "team_domain": "https://myteam.cloudflareaccess.com/",
            "emails": ["Admin@Example.com", "admin@example.com"],
            "agent_bypass": true
        })),
    )
    .await;
    assert_eq!(access.status, StatusCode::OK, "{}", access.body);
    assert_eq!(access.body["team_domain"], "myteam.cloudflareaccess.com");
    assert_eq!(access.body["access_emails"], json!(["admin@example.com"]));
    assert_eq!(access.body["access_configured"], true);
    assert_eq!(access.body["access_enforced"], false);
    {
        let fake = fake.lock().unwrap();
        // The panel plus one bypass application for each agent path.
        assert_eq!(fake.apps.len(), 5);
        let panel = fake
            .apps
            .values()
            .find(|app| app["name"] == "Rudder panel")
            .unwrap();
        assert_eq!(panel["domain"], "panel.example.com");
        assert_eq!(panel["policies"][0]["decision"], "allow");
        assert!(fake
            .apps
            .values()
            .any(|app| app["domain"] == "panel.example.com/mcp"
                && app["policies"][0]["decision"] == "bypass"));
    }

    // Enforcement needs proof that Access works when asked through the panel domain.
    let unproven = call_with(
        &app,
        "PUT",
        "/api/v1/cloudflare/access/enforcement",
        Some(&session),
        Some(json!({ "enforced": true })),
        Some("panel.example.com"),
    )
    .await;
    assert_eq!(unproven.status, StatusCode::CONFLICT, "{}", unproven.body);
    assert_eq!(unproven.body["error"]["code"], "access_not_verified");
    let enforced = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/access/enforcement",
        &session,
        Some(json!({ "enforced": true })),
    )
    .await;
    assert_eq!(enforced.status, StatusCode::OK, "{}", enforced.body);
    assert_eq!(enforced.body["access_enforced"], true);
    // The panel domain now refuses requests without an Access token, except the
    // agent paths and CLI/Bearer API paths; other hostnames (the raw address, localhost) are untouched.
    let blocked = call_with(
        &app,
        "GET",
        "/api/v1/me",
        Some(&session),
        None,
        Some("panel.example.com"),
    )
    .await;
    assert_eq!(blocked.status, StatusCode::FORBIDDEN);
    let blocked_mcp_cookie = call_with(
        &app,
        "GET",
        "/mcp/api/v1/me",
        Some(&session),
        None,
        Some("panel.example.com"),
    )
    .await;
    assert_eq!(blocked_mcp_cookie.status, StatusCode::FORBIDDEN);
    let cli_start = call_with(
        &app,
        "POST",
        "/mcp/api/v1/cli/auth/start",
        None,
        Some(json!({ "method": "device_code", "client_name": "rudder-cli" })),
        Some("panel.example.com"),
    )
    .await;
    assert_eq!(cli_start.status, StatusCode::OK, "{}", cli_start.body);
    let exempt = call_with(
        &app,
        "GET",
        "/api/health",
        None,
        None,
        Some("panel.example.com"),
    )
    .await;
    assert_eq!(exempt.status, StatusCode::OK);
    let local = call_with(
        &app,
        "GET",
        "/api/v1/me",
        Some(&session),
        None,
        Some("localhost:3000"),
    )
    .await;
    assert_eq!(local.status, StatusCode::OK);
    let preflight = call(&app, "GET", "/api/v1/cloudflare/preflight", &session, None).await;
    let access_check = preflight.body["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "access")
        .unwrap();
    assert_eq!(access_check["ok"], true);

    // Turning the bypass off removes its applications.
    let narrowed = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/access",
        &session,
        Some(json!({ "team_domain": "https://myteam.cloudflareaccess.com/", "emails": ["admin@example.com"] })),
    )
    .await;
    assert_eq!(narrowed.status, StatusCode::OK, "{}", narrowed.body);
    assert_eq!(fake.lock().unwrap().apps.len(), 1);

    // SSH through Access for Infrastructure.
    for body in [
        json!({ "address": "127.0.0.1", "users": ["deploy"] }),
        json!({ "address": "not-an-ip", "users": ["deploy"] }),
        json!({ "address": "172.18.0.1", "users": [] }),
        json!({ "address": "172.18.0.1", "users": ["bad user"] }),
    ] {
        let rejected = call(&app, "PUT", "/api/v1/cloudflare/ssh", &session, Some(body)).await;
        assert_eq!(
            rejected.status,
            StatusCode::BAD_REQUEST,
            "{}",
            rejected.body
        );
    }
    let ssh = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/ssh",
        &session,
        Some(json!({ "address": "172.18.0.1", "users": ["deploy", "root"] })),
    )
    .await;
    assert_eq!(ssh.status, StatusCode::OK, "{}", ssh.body);
    assert!(ssh.body["script"]
        .as_str()
        .unwrap()
        .contains("TrustedUserCAKeys"));
    assert!(ssh.body["script"].as_str().unwrap().contains(CA_KEY));
    assert_eq!(ssh.body["status"]["ssh_configured"], true);
    assert_eq!(ssh.body["status"]["ssh_ready"], false);
    assert!(calls(&fake)
        .iter()
        .any(|call| call.contains("route \"172.18.0.1/32\"")
            && call.contains("22222222-2222-2222-2222-222222222222")));
    {
        let fake = fake.lock().unwrap();
        let infra = fake
            .apps
            .values()
            .find(|app| app["type"] == "infrastructure")
            .unwrap();
        assert_eq!(
            infra["policies"][0]["connection_rules"]["ssh"]["usernames"],
            json!(["deploy", "root"])
        );
        assert_eq!(infra["target_criteria"][0]["port"], 22);
    }
    let script = call(&app, "GET", "/api/v1/cloudflare/ssh/script", &session, None).await;
    assert_eq!(script.status, StatusCode::OK);
    assert!(script.body["script"].as_str().unwrap().contains(CA_KEY));
    let confirmed = call(
        &app,
        "PUT",
        "/api/v1/cloudflare/ssh/confirmation",
        &session,
        Some(json!({ "confirmed": true })),
    )
    .await;
    assert_eq!(confirmed.body["ssh_ready"], true, "{}", confirmed.body);

    let removed_ssh = call(&app, "DELETE", "/api/v1/cloudflare/ssh", &session, None).await;
    assert_eq!(removed_ssh.status, StatusCode::NO_CONTENT);
    assert!(calls(&fake)
        .iter()
        .any(|call| call.starts_with("route deleted 172.18.0.1")));
    assert!(calls(&fake)
        .iter()
        .any(|call| call == "target deleted target-1"));
    let no_script = call(&app, "GET", "/api/v1/cloudflare/ssh/script", &session, None).await;
    assert_eq!(no_script.status, StatusCode::BAD_REQUEST);

    // Removing the tunnel undoes everything and returns Traefik to direct mode.
    let gone = call(&app, "DELETE", "/api/v1/cloudflare/tunnel", &session, None).await;
    assert_eq!(gone.status, StatusCode::OK, "{}", gone.body);
    assert_eq!(
        gone.body["removed_dns"],
        json!(["app.example.com", "panel.example.com"])
    );
    {
        let fake = fake.lock().unwrap();
        assert!(fake.dns.is_empty());
        assert!(fake.apps.is_empty());
    }
    let after = call(&app, "GET", "/api/v1/cloudflare", &session, None).await;
    assert_eq!(after.body["tunnel_id"], Value::Null);
    assert_eq!(after.body["tunnel_published"], false);
    assert_eq!(after.body["access_enforced"], false);
    assert_eq!(after.body["token_saved"], true);
    let restored = std::fs::read_to_string(&routes_path).unwrap();
    assert!(restored.contains("certResolver"), "{restored}");
    let open = call_with(
        &app,
        "GET",
        "/api/v1/me",
        Some(&session),
        None,
        Some("panel.example.com"),
    )
    .await;
    assert_eq!(open.status, StatusCode::OK);
}
