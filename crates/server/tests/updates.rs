//! Self-update: release discovery and verification against a local fake GitHub
//! API with real minisign signatures, update settings and access rules, and an
//! agent binary update over a real agent session.
//!
//! Replacing the panel container needs Docker and runs in the live drill, not
//! here; these tests check that the panel refuses to try when it cannot.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use rudder_core::{sha256_hex, ReleaseImages, ReleaseManifest, RELEASE_FORMAT};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rudder-updates-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Signer {
    public: String,
    secret: minisign::SecretKey,
}

impl Signer {
    fn new() -> Self {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        Self {
            public: pair.pk.to_base64(),
            secret: pair.sk,
        }
    }

    fn sign(&self, data: &[u8]) -> Vec<u8> {
        minisign::sign(None, &self.secret, data, None, None)
            .unwrap()
            .into_string()
            .into_bytes()
    }
}

/// Releases and their asset bytes, served like the GitHub API.
#[derive(Default)]
struct FakeGithub {
    releases: Vec<Value>,
    assets: HashMap<String, Vec<u8>>,
}

type Shared = Arc<Mutex<FakeGithub>>;

impl FakeGithub {
    fn publish(&mut self, base: &str, tag: &str, prerelease: bool, files: Vec<(&str, Vec<u8>)>) {
        let assets: Vec<Value> = files
            .iter()
            .map(|(name, bytes)| {
                json!({
                    "name": name,
                    "size": bytes.len(),
                    "browser_download_url": format!("{base}/download/{tag}/{name}"),
                })
            })
            .collect();
        for (name, bytes) in files {
            self.assets.insert(format!("{tag}/{name}"), bytes);
        }
        self.releases.retain(|release| release["tag_name"] != tag);
        self.releases.push(json!({
            "tag_name": tag,
            "name": format!("Rudder {tag}"),
            "body": format!("Notes for {tag}"),
            "draft": false,
            "prerelease": prerelease,
            "published_at": "2026-10-01T00:00:00Z",
            "html_url": format!("https://github.com/acme/rudder/releases/tag/{tag}"),
            "assets": assets,
        }));
    }
}

fn manifest(version: &str) -> ReleaseManifest {
    let digest = "ab".repeat(32);
    ReleaseManifest {
        format: RELEASE_FORMAT.into(),
        version: version.into(),
        images: ReleaseImages {
            panel: format!("ghcr.io/acme/rudder:{version}@sha256:{digest}"),
            cache: format!("ghcr.io/acme/rudder-cache:{version}@sha256:{digest}"),
        },
        agents: Default::default(),
    }
}

/// `SHA256SUMS`, its signature, and `release.json` for `version`.
fn signed_assets(signer: &Signer, version: &str) -> Vec<(&'static str, Vec<u8>)> {
    let body = serde_json::to_vec_pretty(&manifest(version)).unwrap();
    let sums = format!("{}  release.json\n", sha256_hex(&body)).into_bytes();
    let signature = signer.sign(&sums);
    vec![
        ("SHA256SUMS", sums),
        ("SHA256SUMS.minisig", signature),
        ("release.json", body),
    ]
}

async fn start_github() -> (String, Shared) {
    async fn releases(
        State(shared): State<Shared>,
        UrlPath((owner, repo)): UrlPath<(String, String)>,
    ) -> axum::response::Response {
        if (owner.as_str(), repo.as_str()) != ("acme", "rudder") {
            return StatusCode::NOT_FOUND.into_response();
        }
        let list = shared.lock().unwrap().releases.clone();
        axum::Json(list).into_response()
    }
    async fn download(
        State(shared): State<Shared>,
        UrlPath((tag, name)): UrlPath<(String, String)>,
    ) -> axum::response::Response {
        match shared.lock().unwrap().assets.get(&format!("{tag}/{name}")) {
            Some(bytes) => bytes.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
    let shared: Shared = Arc::default();
    let app = axum::Router::new()
        .route(
            "/repos/{owner}/{repo}/releases",
            axum::routing::get(releases),
        )
        .route("/download/{tag}/{name}", axum::routing::get(download))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

struct Panel {
    state: rudder_server::AppState,
    router: axum::Router,
    owner: (String, String),
    viewer: (String, String),
    _dir: TempDir,
}

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn send(
    router: &axum::Router,
    method: &str,
    path: &str,
    session: Option<&(String, String)>,
    body: Option<Value>,
) -> (Reply, axum::http::HeaderMap) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some((cookie, csrf)) = session {
        builder = builder
            .header("cookie", format!("rudder_session={cookie}"))
            .header("x-csrf-token", csrf);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        Reply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        },
        headers,
    )
}

fn session(reply: &Reply, headers: &axum::http::HeaderMap) -> (String, String) {
    let cookie = headers
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .and_then(|v| v.strip_prefix("rudder_session="))
        .unwrap()
        .to_owned();
    (
        cookie,
        reply.body["csrf_token"].as_str().unwrap().to_owned(),
    )
}

impl Panel {
    async fn start(pool: PgPool, github: &str, public_key: &str, extra: &[(&str, String)]) -> Self {
        let dir = TempDir::new();
        let url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
        let mut vars = HashMap::from([
            ("DATABASE_URL".to_owned(), url),
            ("RUDDER_MASTER_KEY".to_owned(), "dd".repeat(32)),
            ("RUDDER_COOKIE_SECURE".to_owned(), "false".to_owned()),
            (
                "RUDDER_DATA_DIR".to_owned(),
                dir.path().display().to_string(),
            ),
            ("RUDDER_UPDATE_API_URL".to_owned(), github.to_owned()),
            ("RUDDER_UPDATE_PUBKEY".to_owned(), public_key.to_owned()),
            ("RUDDER_UPDATE_CHECK_SECS".to_owned(), "0".to_owned()),
            (
                "RUDDER_DOCKER_SOCKET".to_owned(),
                dir.path().join("no-docker.sock").display().to_string(),
            ),
            (
                "RUDDER_DIST_DIR".to_owned(),
                dir.path().join("dist").display().to_string(),
            ),
        ]);
        for (key, value) in extra {
            vars.insert((*key).to_owned(), value.clone());
        }
        let config = rudder_server::Config::from_map(&vars, |_| unreachable!()).unwrap();
        let state = rudder_server::AppState::new(pool.clone(), config);
        let router = rudder_server::router(state.clone());
        sqlx::query("UPDATE panel_settings SET github_repo = 'acme/rudder'")
            .execute(&pool)
            .await
            .unwrap();
        let (created, headers) = send(
            &router,
            "POST",
            "/api/v1/setup",
            None,
            Some(json!({
                "email": "owner@example.com",
                "password": "correct-horse-battery",
                "display_name": "Owner"
            })),
        )
        .await;
        assert_eq!(created.status, StatusCode::OK);
        let owner = session(&created, &headers);
        let (made, _) = send(
            &router,
            "POST",
            "/api/v1/users",
            Some(&owner),
            Some(json!({
                "email": "viewer@example.com",
                "password": "viewer-password-ok",
                "role": "viewer"
            })),
        )
        .await;
        assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
        let (login, headers) = send(
            &router,
            "POST",
            "/api/v1/login",
            None,
            Some(json!({ "email": "viewer@example.com", "password": "viewer-password-ok" })),
        )
        .await;
        let viewer = session(&login, &headers);
        Self {
            state,
            router,
            owner,
            viewer,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        send(&self.router, method, path, Some(&self.owner), body)
            .await
            .0
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn release_checks_verify_signatures_and_follow_the_channel(pool: PgPool) {
    let (github, fake) = start_github().await;
    let signer = Signer::new();
    {
        let mut fake = fake.lock().unwrap();
        fake.publish(&github, "v0.0.9", false, signed_assets(&signer, "0.0.9"));
        fake.publish(&github, "v9.1.0", false, signed_assets(&signer, "9.1.0"));
        fake.publish(
            &github,
            "v9.2.0-rc.1",
            true,
            signed_assets(&signer, "9.2.0-rc.1"),
        );
    }
    let panel = Panel::start(pool.clone(), &github, &signer.public, &[]).await;

    let overview = panel.call("GET", "/api/v1/system/updates", None).await;
    assert_eq!(overview.status, StatusCode::OK, "{}", overview.body);
    assert_eq!(overview.body["current_version"], rudder_core::VERSION);
    assert_eq!(overview.body["channel"], "stable");
    assert_eq!(overview.body["signing_key"], true);
    assert_eq!(overview.body["last_check"], Value::Null);
    let blocker = overview.body["apply_blocker"].as_str().unwrap();
    assert!(blocker.contains("Docker is not reachable"), "{blocker}");

    let check = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(check.status, StatusCode::OK, "{}", check.body);
    assert_eq!(check.body["latest"]["version"], "9.1.0");
    assert_eq!(check.body["latest"]["notes"], "Notes for v9.1.0");
    assert_eq!(check.body["pending"].as_array().unwrap().len(), 1);
    assert_eq!(check.body["pending"][0]["version"], "9.1.0");
    assert_eq!(check.body["verified"], true, "{}", check.body);
    assert_eq!(check.body["error"], Value::Null);
    assert!(check.body.get("manifest").is_none());
    let stored = panel.call("GET", "/api/v1/system/updates", None).await;
    assert_eq!(stored.body["last_check"]["latest"]["version"], "9.1.0");
    assert!(stored.body["last_check"].get("manifest").is_none());

    let invalid = panel
        .call(
            "PUT",
            "/api/v1/system/updates/settings",
            Some(json!({ "channel": "nightly", "auto_update": false })),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    let invalid = panel
        .call(
            "PUT",
            "/api/v1/system/updates/settings",
            Some(json!({ "channel": "stable", "auto_update": true, "maintenance_window": "2am" })),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    let saved = panel
        .call(
            "PUT",
            "/api/v1/system/updates/settings",
            Some(json!({
                "channel": "prerelease",
                "auto_update": true,
                "maintenance_window": "02:00-05:00"
            })),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["channel"], "prerelease");
    assert_eq!(saved.body["auto_update"], true);
    assert_eq!(saved.body["maintenance_window"], "02:00-05:00");

    let check = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(check.body["latest"]["version"], "9.2.0-rc.1");
    assert_eq!(check.body["verified"], true, "{}", check.body);

    // A tampered manifest no longer matches the signed checksum list.
    fake.lock().unwrap().assets.insert(
        "v9.2.0-rc.1/release.json".into(),
        serde_json::to_vec(&manifest("9.2.0-rc.1")).unwrap(),
    );
    let check = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(check.body["verified"], false);
    assert!(
        check.body["error"].as_str().unwrap().contains("checksum"),
        "{}",
        check.body
    );

    // A release signed by another key is refused.
    let other = Signer::new();
    fake.lock().unwrap().publish(
        &github,
        "v9.2.0-rc.1",
        true,
        signed_assets(&other, "9.2.0-rc.1"),
    );
    let check = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(check.body["verified"], false);
    assert!(
        check.body["error"]
            .as_str()
            .unwrap()
            .contains("signing key"),
        "{}",
        check.body
    );

    // A manifest for a different version than its tag is refused.
    fake.lock().unwrap().publish(
        &github,
        "v9.2.0-rc.1",
        true,
        signed_assets(&signer, "9.1.0"),
    );
    let check = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(check.body["verified"], false);
    assert!(
        check.body["error"]
            .as_str()
            .unwrap()
            .contains("release is 9.2.0-rc.1"),
        "{}",
        check.body
    );

    // Applying needs the panel to run in its container; here Docker is absent.
    let apply = panel
        .call("POST", "/api/v1/system/updates/apply", None)
        .await;
    assert_eq!(apply.status, StatusCode::CONFLICT, "{}", apply.body);
    assert_eq!(
        apply.body["error"]["code"], "update_unavailable",
        "{}",
        apply.body
    );
    let history: i64 = sqlx::query_scalar("SELECT count(*) FROM update_history")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(history, 0);

    // Viewers cannot see or change updates; anonymous callers are not signed in.
    for (method, path) in [
        ("GET", "/api/v1/system/updates"),
        ("POST", "/api/v1/system/updates/check"),
        ("POST", "/api/v1/system/updates/apply"),
        ("POST", "/api/v1/system/updates/components"),
    ] {
        let (denied, _) = send(&panel.router, method, path, Some(&panel.viewer), None).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{method} {path}");
        let (anonymous, _) = send(&panel.router, method, path, None, None).await;
        assert_eq!(
            anonymous.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'panel.update_settings'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audited, 1);
    drop(panel.state);
}

#[sqlx::test(migrations = "./migrations")]
async fn release_checks_without_a_signing_key_cannot_verify(pool: PgPool) {
    let (github, fake) = start_github().await;
    let signer = Signer::new();
    fake.lock()
        .unwrap()
        .publish(&github, "v9.1.0", false, signed_assets(&signer, "9.1.0"));
    let panel = Panel::start(pool, &github, "", &[]).await;
    let check = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(check.body["latest"]["version"], "9.1.0");
    assert_eq!(check.body["verified"], false);
    assert!(check.body["error"]
        .as_str()
        .unwrap()
        .contains("no release signing key"));
    let overview = panel.call("GET", "/api/v1/system/updates", None).await;
    assert_eq!(overview.body["signing_key"], false);
    assert!(overview.body["apply_blocker"]
        .as_str()
        .unwrap()
        .contains("signing key"));

    // A failing API is reported in the check, not as an error response.
    sqlx::query("UPDATE panel_settings SET github_repo = 'acme/missing'")
        .execute(&panel.state.pool)
        .await
        .unwrap();
    let missing = panel
        .call("POST", "/api/v1/system/updates/check", None)
        .await;
    assert_eq!(missing.status, StatusCode::OK);
    assert_eq!(missing.body["latest"], Value::Null);
    assert!(
        missing.body["error"].as_str().unwrap().contains("404"),
        "{}",
        missing.body
    );
}

fn agent_script(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho rudder-agent {version}\n").into_bytes()
}

#[sqlx::test(migrations = "./migrations")]
async fn outdated_agents_receive_the_bundled_binary_and_reconnect(pool: PgPool) {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let (github, _fake) = start_github().await;
    let signer = Signer::new();
    let agent_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let agent_port = agent_listener.local_addr().unwrap().port();
    let panel = Panel::start(
        pool.clone(),
        &github,
        &signer.public,
        &[(
            "RUDDER_AGENT_URL",
            format!("https://127.0.0.1:{agent_port}"),
        )],
    )
    .await;
    tokio::spawn(rudder_server::serve_agents(
        panel.state.clone(),
        agent_listener,
    ));
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr: SocketAddr = http.local_addr().unwrap();
    let app = panel.router.clone();
    tokio::spawn(async move {
        axum::serve(
            http,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });

    // The binary the panel image would bundle for this machine's agent.
    let dist = panel.state.config.dist_dir.clone();
    std::fs::create_dir_all(&dist).unwrap();
    let target = rudder_agent::update::target();
    let bundled = agent_script(rudder_core::VERSION);
    let file = dist.join(format!("rudder-agent-{target}"));
    std::fs::write(&file, &bundled).unwrap();
    std::fs::write(
        file.with_extension("sha256"),
        format!("{}  rudder-agent-{target}\n", sha256_hex(&bundled)),
    )
    .unwrap();

    let created = panel
        .call(
            "POST",
            "/api/v1/servers",
            Some(json!({ "name": "edge-1", "kind": "remote" })),
        )
        .await;
    let server_id: Uuid = created.body["id"].as_str().unwrap().parse().unwrap();
    let token = created.body["join_token"].as_str().unwrap();
    let agent_dir = TempDir::new();
    let data_dir = agent_dir.path().join("agent");
    let identity = rudder_agent::enroll(&format!("http://{http_addr}"), token, &data_dir)
        .await
        .unwrap();

    // The agent's own executable, as installed on a server.
    let binary = agent_dir.path().join("rudder-agent");
    std::fs::write(&binary, agent_script("0.0.1")).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (restart_tx, mut restart_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let start = |identity: rudder_agent::Identity| {
        let restart = restart_tx.clone();
        let options = rudder_agent::RunOptions {
            identity,
            data_dir: data_dir.clone(),
            stats_interval: Duration::from_secs(60),
            self_update: Some(rudder_agent::SelfUpdate {
                binary: binary.clone(),
                data_dir: data_dir.clone(),
                restart: Arc::new(move || {
                    let _ = restart.send(());
                }),
                grace: Duration::from_secs(30),
            }),
        };
        let node_dir = agent_dir.path().join("node");
        std::fs::create_dir_all(&node_dir).unwrap();
        let driver: Arc<dyn rudder_core::NodeDriver> = Arc::new(rudder_node::LocalDriver::connect(
            &node_dir,
            Some(&node_dir.join("no-docker.sock")),
        ));
        tokio::spawn(rudder_agent::run_with(
            options,
            driver,
            Arc::new(rudder_agent::Buffer::default()),
            std::future::pending(),
        ))
    };
    let first = start(identity.clone());
    let online = async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM servers WHERE id = $1")
                .bind(server_id)
                .fetch_one(&pool)
                .await
                .unwrap();
            if status == "online" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), online)
        .await
        .unwrap();

    // Nothing to do while the agent reports the panel's version.
    let idle = panel
        .call("POST", "/api/v1/system/updates/components", None)
        .await;
    assert_eq!(idle.status, StatusCode::ACCEPTED, "{}", idle.body);
    assert_eq!(idle.body["queued"], 0);

    sqlx::query("UPDATE servers SET agent_version = '0.0.1' WHERE id = $1")
        .bind(server_id)
        .execute(&pool)
        .await
        .unwrap();
    let overview = panel.call("GET", "/api/v1/system/updates", None).await;
    let edge = overview.body["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["server_name"] == "edge-1")
        .unwrap()
        .clone();
    assert_eq!(edge["agent_outdated"], true, "{edge}");
    assert_eq!(edge["online"], true);

    let queued = panel
        .call("POST", "/api/v1/system/updates/components", None)
        .await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{}", queued.body);
    assert_eq!(queued.body["queued"], 1);

    // The agent installs the binary and asks to be restarted.
    tokio::time::timeout(Duration::from_secs(30), restart_rx.recv())
        .await
        .expect("agent asked to restart")
        .unwrap();
    assert_eq!(std::fs::read(&binary).unwrap(), bundled);
    assert_eq!(
        std::fs::read(rudder_agent::update::previous_path(&binary)).unwrap(),
        agent_script("0.0.1")
    );
    assert!(data_dir.join(rudder_agent::update::MARKER).exists());
    first.abort();
    let _second = start(identity);

    let finished = async {
        loop {
            let row: Option<(String, String)> = sqlx::query_as(
                "SELECT status, logs FROM update_history WHERE component = 'agent' AND status <> 'running'",
            )
            .fetch_optional(&pool)
            .await
            .unwrap();
            if let Some(row) = row {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    let (status, logs) = tokio::time::timeout(Duration::from_secs(60), finished)
        .await
        .unwrap();
    assert_eq!(status, "succeeded", "{logs}");
    assert!(logs.contains("sha256"), "{logs}");
    let marker_cleared = async {
        while data_dir.join(rudder_agent::update::MARKER).exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), marker_cleared)
        .await
        .expect("watchdog confirmed the update");

    let overview = panel.call("GET", "/api/v1/system/updates", None).await;
    let history = overview.body["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["component"], "agent");
    assert_eq!(history[0]["server_name"], "edge-1");
    assert_eq!(history[0]["from_version"], "0.0.1");
    assert_eq!(history[0]["to_version"], rudder_core::VERSION);
}
