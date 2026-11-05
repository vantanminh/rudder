//! The `rudder` CLI against a real panel listener: login, services, deploy, logs, and cache purge.
//!
//! Docker is not available to the panel here, so deploys fail; the test checks that the
//! CLI waits for them and reports the failure. The cache admin API is a small in-test server.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Output {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Panel {
    base: String,
    http: reqwest::Client,
    cookie: String,
    csrf: String,
    _dir: std::path::PathBuf,
}

impl Panel {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut request = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .header("cookie", format!("rudder_session={}", self.cookie))
            .header("x-csrf-token", &self.csrf);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (status, value) = self.call(method, path, Some(body)).await;
        assert!(status < 300, "{method} {path}: {status} {value}");
        value
    }

    async fn token(&self, scopes: &[&str]) -> String {
        let created = self
            .ok(
                "POST",
                "/api/v1/tokens",
                json!({ "name": format!("cli {}", scopes.join(" ")), "scopes": scopes }),
            )
            .await;
        created["token"].as_str().unwrap().to_owned()
    }
}

fn docker_tests() -> bool {
    std::env::var("RUDDER_DOCKER_TESTS").as_deref() == Ok("1")
}

async fn start(pool: PgPool, cache_admin: &str) -> Panel {
    let dir = std::env::temp_dir().join(format!("rudder-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    let mut vars = HashMap::from([
        ("DATABASE_URL".to_owned(), url),
        ("RUDDER_MASTER_KEY".to_owned(), "ee".repeat(32)),
        ("RUDDER_COOKIE_SECURE".to_owned(), "false".to_owned()),
        ("RUDDER_DATA_DIR".to_owned(), dir.display().to_string()),
        ("RUDDER_CACHE_ADMIN_URL".to_owned(), cache_admin.to_owned()),
        ("RUDDER_UPDATE_CHECK_SECS".to_owned(), "0".to_owned()),
    ]);
    if !docker_tests() {
        vars.insert(
            "RUDDER_DOCKER_SOCKET".to_owned(),
            dir.join("no-docker.sock").display().to_string(),
        );
    }
    let config = rudder_server::Config::from_map(&vars, |_| unreachable!()).unwrap();
    let app = rudder_server::router(rudder_server::AppState::new(pool, config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let http = reqwest::Client::new();
    let response = http
        .post(format!("{base}/api/v1/setup"))
        .json(&json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery",
            "display_name": "Owner"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .and_then(|v| v.strip_prefix("rudder_session="))
        .unwrap()
        .to_owned();
    let body: Value = response.json().await.unwrap();
    Panel {
        base,
        http,
        cookie,
        csrf: body["csrf_token"].as_str().unwrap().to_owned(),
        _dir: dir,
    }
}

fn context(
    config: &std::path::Path,
    token: Option<String>,
    url: Option<String>,
) -> rudder_cli::Context {
    rudder_cli::Context {
        config_path: config.to_path_buf(),
        url,
        token,
        read_token: Box::new(|| Err(std::io::Error::other("no stdin in tests"))),
        poll: Duration::from_millis(100),
    }
}

async fn cli(ctx: &rudder_cli::Context, args: &[&str]) -> Result<String, String> {
    let mut out = Output::default();
    let result = rudder_cli::run(args.iter().map(|a| a.to_string()).collect(), ctx, &mut out).await;
    match result {
        Ok(()) => Ok(out.text()),
        Err(error) => Err(format!("{error:#}")),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn cli_logs_in_deploys_reads_logs_and_purges(pool: PgPool) {
    let purges: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = purges.clone();
    let admin = axum::Router::new().route(
        "/purge",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body);
                axum::Json(json!({ "removed": 3 }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, admin).await.unwrap() });

    let panel = start(pool, &admin_url).await;
    let write_token = panel.token(&["read", "write"]).await;
    let read_token = panel.token(&["read"]).await;

    let server = panel
        .ok(
            "POST",
            "/api/v1/servers",
            json!({ "name": "local", "kind": "local" }),
        )
        .await;
    let project = panel
        .ok("POST", "/api/v1/projects", json!({ "name": "shop" }))
        .await;
    let environment = panel
        .ok(
            "POST",
            &format!(
                "/api/v1/projects/{}/environments",
                project["id"].as_str().unwrap()
            ),
            json!({ "name": "prod" }),
        )
        .await;
    let env_id = environment["id"].as_str().unwrap();
    let web = panel
        .ok(
            "POST",
            &format!("/api/v1/environments/{env_id}/services"),
            json!({ "name": "web", "kind": "application", "server_id": server["id"], "spec": { "image": "nginx:1.27-alpine" } }),
        )
        .await;
    let web_id = web["id"].as_str().unwrap().to_owned();
    let blog = panel
        .ok(
            "POST",
            &format!("/api/v1/environments/{env_id}/stacks"),
            json!({ "name": "blog", "server_id": server["id"], "source": "inline", "compose_yaml": "services:\n  web:\n    image: nginx:1.27-alpine\n" }),
        )
        .await;
    let blog_id = blog["service"]["id"].as_str().unwrap().to_owned();

    let dir = std::env::temp_dir().join(format!("rudder-cli-config-{}", uuid::Uuid::new_v4()));
    let config = dir.join("config.json");

    let anonymous = context(&config, None, None);
    let error = cli(&anonymous, &["services"]).await.unwrap_err();
    assert!(error.contains("not logged in"), "{error}");
    let error = cli(
        &context(&config, Some(write_token.clone()), None),
        &["login"],
    )
    .await
    .unwrap_err();
    assert!(error.contains("--url"), "{error}");
    let error = cli(
        &context(&config, Some("rdr_wrong".into()), None),
        &["login", "--url", &panel.base],
    )
    .await
    .unwrap_err();
    assert!(error.contains("rejected the token"), "{error}");
    assert!(!config.exists());
    let error = cli(
        &anonymous,
        &["login", "--url", &panel.base, "--token", "rdr_x"],
    )
    .await
    .unwrap_err();
    assert!(error.contains("RUDDER_TOKEN"), "{error}");

    let login = cli(
        &context(&config, Some(write_token.clone()), None),
        &["login", "--url", &format!("{}/", panel.base)],
    )
    .await
    .unwrap();
    assert!(login.contains("as owner@example.com (admin)"), "{login}");
    assert!(!login.contains(&write_token));
    let saved: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(saved["url"], panel.base.as_str());
    {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        #[cfg(unix)]
        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        #[cfg(unix)]
        assert_eq!(mode, 0o600);
    }

    let saved_ctx = context(&config, None, None);
    let listing = cli(&saved_ctx, &["services"]).await.unwrap();
    assert!(listing.contains("shop/prod/web"), "{listing}");
    assert!(listing.contains("shop/prod/blog"), "{listing}");
    assert!(listing.contains(&web_id));

    let error = cli(&saved_ctx, &["deploy", "missing"]).await.unwrap_err();
    assert!(error.contains("no service matches"), "{error}");
    let error = cli(
        &context(&config, Some(read_token.clone()), None),
        &["deploy", "shop/web"],
    )
    .await
    .unwrap_err();
    assert!(error.contains("not allowed"), "{error}");
    if docker_tests() {
        let deployed = cli(&saved_ctx, &["deploy", "shop/web"]).await.unwrap();
        assert!(
            deployed.contains("Deploying shop/prod/web (nginx:1.27-alpine)"),
            "{deployed}"
        );
        assert!(deployed.contains("succeeded"), "{deployed}");
        let stack = cli(
            &saved_ctx,
            &["deploy", "shop/prod/blog", "--timeout", "120"],
        )
        .await
        .unwrap();
        assert!(stack.contains("Starting stack shop/prod/blog"), "{stack}");
        assert!(stack.contains("succeeded"), "{stack}");
    } else {
        let error = cli(&saved_ctx, &["deploy", "shop/web"]).await.unwrap_err();
        assert!(error.contains("docker"), "{error}");
        let error = cli(&saved_ctx, &["deploy", "shop/prod/blog"])
            .await
            .unwrap_err();
        assert!(error.contains("docker"), "{error}");
    }
    let error = cli(
        &saved_ctx,
        &["deploy", "shop/web", "--image", "Not An Image!"],
    )
    .await
    .unwrap_err();
    assert!(!error.is_empty());
    let (_, deployments) = panel
        .call(
            "GET",
            &format!("/api/v1/services/{web_id}/deployments"),
            None,
        )
        .await;
    assert!(!deployments.as_array().unwrap().is_empty());

    let ingest = |message: &str, level: &str| json!({ "entries": [{ "source": "app", "level": level, "message": message, "service_id": web_id }] });
    panel
        .ok("POST", "/api/v1/logs/ingest", ingest("GET / 200", "info"))
        .await;
    panel
        .ok(
            "POST",
            "/api/v1/logs/ingest",
            ingest("database timeout", "error"),
        )
        .await;
    let mut lines = String::new();
    for _ in 0..50 {
        lines = cli(&saved_ctx, &["logs", "web", "-n", "10"]).await.unwrap();
        if lines.contains("database timeout") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let get = lines.find("GET / 200").expect(&lines);
    let timeout = lines.find("database timeout").expect(&lines);
    assert!(get < timeout, "oldest first: {lines}");
    assert!(lines.contains("ERROR [app] database timeout"), "{lines}");
    let errors = cli(&saved_ctx, &["logs", "web", "--level", "error"])
        .await
        .unwrap();
    assert!(
        errors.contains("database timeout") && !errors.contains("GET /"),
        "{errors}"
    );
    let error = cli(&saved_ctx, &["logs", "web", "--since", "soon"])
        .await
        .unwrap_err();
    assert!(error.contains("--since"), "{error}");

    let follow_out = Output::default();
    let follow = {
        let config = config.clone();
        let mut out = follow_out.clone();
        tokio::spawn(async move {
            let ctx = context(&config, None, None);
            let _ = rudder_cli::run(
                vec![
                    "logs".into(),
                    "web".into(),
                    "-f".into(),
                    "-n".into(),
                    "1".into(),
                ],
                &ctx,
                &mut out,
            )
            .await;
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    panel
        .ok(
            "POST",
            "/api/v1/logs/ingest",
            ingest("followed line", "warn"),
        )
        .await;
    let mut followed = false;
    for _ in 0..50 {
        if follow_out.text().contains("WARN  [app] followed line") {
            followed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    follow.abort();
    assert!(followed, "{}", follow_out.text());
    assert_eq!(follow_out.text().matches("database timeout").count(), 1);

    let domain = panel
        .ok(
            "POST",
            &format!("/api/v1/services/{web_id}/domains"),
            json!({ "host": "shop.example.com", "target_port": 80, "ssl_mode": "none" }),
        )
        .await;
    let policy = panel
        .ok(
            "POST",
            "/api/v1/cache-policies",
            json!({ "name": "Static", "preset": "static-assets" }),
        )
        .await;
    panel
        .ok(
            "PUT",
            &format!("/api/v1/domains/{}/cache", domain["id"].as_str().unwrap()),
            json!({ "enabled": true, "policy_id": policy["id"] }),
        )
        .await;
    let error = cli(&saved_ctx, &["cache", "purge", "web"])
        .await
        .unwrap_err();
    assert!(error.contains("--all"), "{error}");
    let error = cli(
        &context(&config, Some(read_token), None),
        &["cache", "purge", "web", "--all"],
    )
    .await
    .unwrap_err();
    assert!(error.contains("not allowed"), "{error}");
    let purged = cli(
        &saved_ctx,
        &[
            "cache", "purge", "web", "--path", "/a.css", "--path", "/b.js",
        ],
    )
    .await
    .unwrap();
    assert!(
        purged.contains("Purged 3 cached objects for shop/prod/web"),
        "{purged}"
    );
    {
        let purges = purges.lock().unwrap();
        assert_eq!(purges.len(), 1);
        assert_eq!(purges[0]["host"], "shop.example.com");
        assert_eq!(purges[0]["paths"], json!(["/a.css", "/b.js"]));
    }

    let override_url = cli(
        &context(&config, None, Some("http://127.0.0.1:9".into())),
        &["services"],
    )
    .await
    .unwrap_err();
    assert!(override_url.contains("127.0.0.1:9"), "{override_url}");

    if docker_tests() {
        for service in [web_id.as_str(), blog_id.as_str()] {
            let (status, body) = panel
                .call("DELETE", &format!("/api/v1/services/{service}"), None)
                .await;
            assert_eq!(status, 204, "{body}");
        }
    }

    let out = cli(&saved_ctx, &["logout"]).await.unwrap();
    assert!(out.contains("Removed"));
    assert!(!config.exists());
    std::fs::remove_dir_all(dir).ok();
}

#[sqlx::test(migrations = "./migrations")]
async fn cli_device_login_image_build_push_retention_and_self_update(pool: PgPool) {
    let panel = start(pool, "http://127.0.0.1:9").await;

    let server = panel
        .ok(
            "POST",
            "/api/v1/servers",
            json!({ "name": "local", "kind": "local" }),
        )
        .await;
    let project = panel
        .ok("POST", "/api/v1/projects", json!({ "name": "shop" }))
        .await;
    let environment = panel
        .ok(
            "POST",
            &format!(
                "/api/v1/projects/{}/environments",
                project["id"].as_str().unwrap()
            ),
            json!({ "name": "prod" }),
        )
        .await;
    let env_id = environment["id"].as_str().unwrap();
    let web = panel
        .ok(
            "POST",
            &format!("/api/v1/environments/{env_id}/services"),
            json!({ "name": "web", "kind": "application", "server_id": server["id"], "spec": { "image": "nginx:1.27-alpine" } }),
        )
        .await;
    let web_id = web["id"].as_str().unwrap().to_owned();

    // Configure Cloudflare R2 destination & image store settings + per-service retention=1
    let r2 = panel
        .ok(
            "POST",
            "/api/v1/s3-destinations",
            json!({
                "name": "cloudflare-r2",
                "preset": "cloudflare_r2",
                "endpoint": "http://local.r2.internal",
                "region": "auto",
                "bucket": "rudder-images",
                "access_key_id": "test-key",
                "secret_access_key": "test-secret",
                "path_style": true
            }),
        )
        .await;
    panel
        .ok(
            "PUT",
            "/api/v1/image-store/settings",
            json!({
                "destination_id": r2["id"],
                "path_prefix": "rudder-images",
                "r2_keep_count": 2
            }),
        )
        .await;
    panel
        .ok(
            "PUT",
            &format!("/api/v1/services/{web_id}/image-policy"),
            json!({
                "image_auto_deploy": false,
                "image_r2_keep_count": 1
            }),
        )
        .await;

    let dir = std::env::temp_dir().join(format!("rudder-cli-img-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");

    // 1. Test `rudder login --url <base> --device` (8-char alphanumeric device code)
    let login_out = Output::default();
    let login_task = {
        let config = config.clone();
        let base = panel.base.clone();
        let mut out = login_out.clone();
        tokio::spawn(async move {
            let ctx = context(&config, None, None);
            rudder_cli::run(
                vec!["login".into(), "--url".into(), base, "--device".into()],
                &ctx,
                &mut out,
            )
            .await
        })
    };

    let mut device_code = None;
    for _ in 0..50 {
        let text = login_out.text();
        for line in text.lines() {
            if let Some(code) = line.strip_prefix("Enter 8-character device code: ") {
                device_code = Some(code.trim().to_owned());
                break;
            }
        }
        if device_code.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let code = device_code.expect("CLI should print 8-character device code");
    assert_eq!(code.len(), 8, "device code must be 8 chars: {code}");
    assert!(code.chars().any(|c| c.is_ascii_alphabetic()));
    assert!(code.chars().any(|c| c.is_ascii_digit()));

    panel
        .ok(
            "POST",
            "/api/v1/cli/auth/approve",
            json!({ "device_code": code }),
        )
        .await;

    login_task.await.unwrap().unwrap();
    assert!(
        login_out.text().contains("Logged in to"),
        "{}",
        login_out.text()
    );

    let ctx = context(&config, None, None);

    // 2. Test `rudder init`, `rudder build`, `rudder push`, and `rudder images list`
    let project_file = dir.join("rudder.toml");
    let fake_archive = dir.join("fake-docker.tar");
    std::fs::write(&fake_archive, b"fake docker save tar payload v1").unwrap();
    std::env::set_var("RUDDER_FAKE_DOCKER_ARCHIVE", &fake_archive);

    let init_out = cli(
        &ctx,
        &[
            "init",
            "--file",
            project_file.to_str().unwrap(),
            "--service",
            "shop/prod/web",
            "--r2-keep",
            "1",
            "--local-keep",
            "3",
        ],
    )
    .await
    .unwrap();
    assert!(init_out.contains("Created"), "{init_out}");

    let build_v1 = cli(
        &ctx,
        &[
            "build",
            "--file",
            project_file.to_str().unwrap(),
            "--tag",
            "v1.0.0",
            "--no-check",
        ],
    )
    .await
    .unwrap();
    assert!(
        build_v1.contains("Built rudder-image/web:v1.0.0"),
        "{build_v1}"
    );

    let push_v1 = cli(
        &ctx,
        &["push", "v1.0.0", "--file", project_file.to_str().unwrap()],
    )
    .await
    .unwrap();
    assert!(
        push_v1.contains("Image v1.0.0 is stored in R2"),
        "{push_v1}"
    );

    // Build and push v1.0.1 -> v1.0.0 should be pruned from R2 (keep=1) while kept locally
    std::fs::write(&fake_archive, b"fake docker save tar payload v2").unwrap();
    cli(
        &ctx,
        &[
            "build",
            "--file",
            project_file.to_str().unwrap(),
            "--tag",
            "v1.0.1",
            "--no-check",
        ],
    )
    .await
    .unwrap();
    let push_v2 = cli(
        &ctx,
        &["push", "v1.0.1", "--file", project_file.to_str().unwrap()],
    )
    .await
    .unwrap();
    assert!(
        push_v2.contains("Pruned old versions from R2 (still kept locally): v1.0.0"),
        "{push_v2}"
    );

    let images_out = cli(
        &ctx,
        &["images", "list", "--file", project_file.to_str().unwrap()],
    )
    .await
    .unwrap();
    assert!(images_out.contains("v1.0.1"), "{images_out}");
    assert!(images_out.contains("local + R2"), "{images_out}");
    assert!(images_out.contains("v1.0.0"), "{images_out}");
    assert!(images_out.contains("local only"), "{images_out}");

    std::env::remove_var("RUDDER_FAKE_DOCKER_ARCHIVE");

    // 3. Test `rudder update` against a mock GitHub release server
    let asset_name = rudder_cli::update::current_target_asset().unwrap();
    let new_binary = b"#!/bin/sh\necho rudder-0.99.0\n".to_vec();
    let sha = rudder_core::sha256_hex(&new_binary);
    let sums_body = format!("{sha}  {asset_name}\n");

    let gh_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gh_base = format!("http://{}", gh_listener.local_addr().unwrap());
    let gh_base_for_route = gh_base.clone();
    let asset_for_route = asset_name.clone();
    let bin_for_route = new_binary.clone();
    let gh_router = axum::Router::new()
        .route(
            "/repos/vantanminh/rudder/releases/latest",
            axum::routing::get(move || {
                let base = gh_base_for_route.clone();
                let asset = asset_for_route.clone();
                async move {
                    axum::Json(json!({
                        "tag_name": "v0.99.0",
                        "assets": [
                            {
                                "name": "SHA256SUMS",
                                "browser_download_url": format!("{base}/download/SHA256SUMS")
                            },
                            {
                                "name": asset,
                                "browser_download_url": format!("{base}/download/bin")
                            }
                        ]
                    }))
                }
            }),
        )
        .route(
            "/download/SHA256SUMS",
            axum::routing::get(move || {
                let sums = sums_body.clone();
                async move { sums }
            }),
        )
        .route(
            "/download/bin",
            axum::routing::get(move || {
                let bin = bin_for_route.clone();
                async move { bin }
            }),
        );
    tokio::spawn(async move { axum::serve(gh_listener, gh_router).await.unwrap() });

    let target_bin = dir.join("rudder-bin");
    std::fs::write(&target_bin, b"old-binary").unwrap();

    let check_out = cli(
        &ctx,
        &[
            "update",
            "--check",
            "--api-url",
            &gh_base,
            "--current-version",
            "0.5.1",
        ],
    )
    .await
    .unwrap();
    assert!(
        check_out.contains("Update available: rudder 0.5.1 -> 0.99.0"),
        "{check_out}"
    );

    let update_out = cli(
        &ctx,
        &[
            "update",
            "--api-url",
            &gh_base,
            "--current-version",
            "0.5.1",
            "--target-path",
            target_bin.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    assert!(
        update_out.contains("Updated rudder to 0.99.0"),
        "{update_out}"
    );
    assert_eq!(std::fs::read(&target_bin).unwrap(), new_binary);

    std::fs::remove_dir_all(dir).ok();
}
