//! Panel disaster recovery: settings export, encrypted panel backups, panel backup
//! jobs, and the offline restore into a fresh database.
//!
//! The backup and restore tests run `pg_dump`/`pg_restore` from `PATH` against the
//! test database and are skipped when the PostgreSQL client tools are missing.

use std::collections::HashMap;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

const MASTER_HEX: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
const PASSPHRASE: &str = "correct horse battery staple";
const SECRET_VALUE: &str = "s3cr3t-env-value-for-export";

fn database_url_for(pool: &PgPool) -> String {
    let base =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    let name = pool
        .connect_options()
        .get_database()
        .expect("test database name")
        .to_owned();
    let mut url = url::Url::parse(&base).unwrap();
    url.set_path(&name);
    url.to_string()
}

fn app(pool: PgPool, data_dir: &Path) -> axum::Router {
    let vars = HashMap::from([
        ("DATABASE_URL".into(), database_url_for(&pool)),
        ("RUDDER_MASTER_KEY".into(), MASTER_HEX.into()),
        ("RUDDER_COOKIE_SECURE".into(), "false".into()),
        ("RUDDER_DATA_DIR".into(), data_dir.display().to_string()),
    ]);
    let config = rudder_server::Config::from_map(&vars, |_| unreachable!()).unwrap();
    rudder_server::router(rudder_server::AppState::new(pool, config))
}

fn tools_available() -> bool {
    let major = |name: &str| {
        let output = Command::new(name).arg("--version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.split_whitespace()
            .find_map(|word| word.split('.').next()?.parse::<u32>().ok())
    };
    let available = ["pg_dump", "pg_restore"]
        .iter()
        .all(|name| major(name).is_some_and(|version| version >= 16));
    if !available {
        eprintln!("skipping: pg_dump/pg_restore 16 or newer are not installed");
    }
    available
}

struct Reply {
    status: StatusCode,
    body: Value,
    raw: Vec<u8>,
    headers: axum::http::HeaderMap,
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    session: Option<(&str, &str)>,
    body: Option<Value>,
) -> Reply {
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
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let raw = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    Reply {
        status,
        body,
        raw,
        headers,
    }
}

struct Session {
    cookie: String,
    csrf: String,
}

fn session_from(reply: &Reply) -> Session {
    let cookie = reply
        .headers
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .and_then(|pair| pair.strip_prefix("rudder_session="))
        .expect("session cookie")
        .to_owned();
    Session {
        cookie,
        csrf: reply.body["csrf_token"].as_str().unwrap().to_owned(),
    }
}

impl Session {
    fn pair(&self) -> Option<(&str, &str)> {
        Some((&self.cookie, &self.csrf))
    }
}

/// Owner, a viewer, a local server, a project, a service, and a secret env value.
async fn seed(app: &axum::Router) -> (Session, Session) {
    let created = send(
        app,
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
    let owner = session_from(&created);
    let viewer = send(
        app,
        "POST",
        "/api/v1/users",
        owner.pair(),
        Some(json!({
            "email": "viewer@example.com",
            "password": "viewer-password-ok",
            "role": "viewer"
        })),
    )
    .await;
    assert_eq!(viewer.status, StatusCode::CREATED, "{}", viewer.body);
    let login = send(
        app,
        "POST",
        "/api/v1/login",
        None,
        Some(json!({ "email": "viewer@example.com", "password": "viewer-password-ok" })),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    let viewer = session_from(&login);

    let local = send(
        app,
        "POST",
        "/api/v1/servers",
        owner.pair(),
        Some(json!({ "name": "local", "kind": "local" })),
    )
    .await;
    assert_eq!(local.status, StatusCode::CREATED);
    let project = send(
        app,
        "POST",
        "/api/v1/projects",
        owner.pair(),
        Some(json!({ "name": "blog" })),
    )
    .await;
    assert_eq!(project.status, StatusCode::CREATED);
    let environment = send(
        app,
        "POST",
        &format!(
            "/api/v1/projects/{}/environments",
            project.body["id"].as_str().unwrap()
        ),
        owner.pair(),
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(environment.status, StatusCode::CREATED);
    let service = send(
        app,
        "POST",
        &format!(
            "/api/v1/environments/{}/services",
            environment.body["id"].as_str().unwrap()
        ),
        owner.pair(),
        Some(json!({
            "name": "web",
            "kind": "application",
            "server_id": local.body["id"],
            "spec": { "image": "ghcr.io/acme/web:1" }
        })),
    )
    .await;
    assert_eq!(service.status, StatusCode::CREATED);
    let env = send(
        app,
        "PUT",
        &format!(
            "/api/v1/services/{}/env",
            service.body["id"].as_str().unwrap()
        ),
        owner.pair(),
        Some(json!({ "key": "API_TOKEN", "value": SECRET_VALUE, "is_build_secret": false })),
    )
    .await;
    assert_eq!(env.status, StatusCode::NO_CONTENT);
    (owner, viewer)
}

async fn unseal(sealed: &[u8], passphrase: &str) -> Vec<u8> {
    use tokio::io::AsyncReadExt;

    let mut decryptor = rudder_core::Decryptor::new(passphrase);
    let plain = decryptor.update(sealed).expect("decrypt");
    decryptor.finish().expect("complete envelope");
    let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(plain.as_slice());
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).await.unwrap();
    out
}

fn entries(tar_bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut out = HashMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        out.insert(name, data);
    }
    out
}

#[sqlx::test(migrations = "./migrations")]
async fn settings_export_and_access(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("rudder-panel-{}", uuid::Uuid::new_v4()));
    let app = app(pool, &dir);
    let (owner, viewer) = seed(&app).await;

    let export = send(
        &app,
        "GET",
        "/api/v1/system/settings-export",
        owner.pair(),
        None,
    )
    .await;
    assert_eq!(export.status, StatusCode::OK);
    assert!(export.headers["content-disposition"]
        .to_str()
        .unwrap()
        .contains("rudder-settings-"));
    assert_eq!(export.body["format"], "rudder-settings-export");
    assert_eq!(export.body["projects"][0]["name"], "blog");
    assert_eq!(export.body["services"][0]["name"], "web");
    assert_eq!(export.body["env_vars"][0]["key"], "API_TOKEN");
    assert_eq!(export.body["users"].as_array().unwrap().len(), 2);
    let text = String::from_utf8_lossy(&export.raw);
    for leaked in [
        "_enc\"",
        "_hash\"",
        "csrf_token",
        "totp_secret",
        SECRET_VALUE,
    ] {
        assert!(!text.contains(leaked), "export contains {leaked}");
    }

    let info = send(&app, "GET", "/api/v1/system", owner.pair(), None).await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.body["schema"], info.body["known_schema"]);
    assert_eq!(info.body["version"], rudder_core::VERSION);

    for path in ["/api/v1/system", "/api/v1/system/settings-export"] {
        let denied = send(&app, "GET", path, viewer.pair(), None).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{path}");
    }
    let denied = send(
        &app,
        "POST",
        "/api/v1/system/backup",
        viewer.pair(),
        Some(json!({ "passphrase": PASSPHRASE })),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let anonymous = send(&app, "GET", "/api/v1/system/settings-export", None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let short = send(
        &app,
        "POST",
        "/api/v1/system/backup",
        owner.pair(),
        Some(json!({ "passphrase": "short" })),
    )
    .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);

    let destination = send(
        &app,
        "POST",
        "/api/v1/s3-destinations",
        owner.pair(),
        Some(json!({
            "name": "offsite",
            "endpoint": "http://127.0.0.1:9",
            "bucket": "panel",
            "access_key_id": "AKIAEXAMPLE",
            "secret_access_key": "example-secret-key"
        })),
    )
    .await;
    assert_eq!(
        destination.status,
        StatusCode::CREATED,
        "{}",
        destination.body
    );
    let destination_id = destination.body["id"].clone();
    let job = |extra: Value| {
        let mut body = json!({
            "name": "panel",
            "target_kind": "panel_db",
            "destination_ids": [destination_id],
            "schedule_cron": "@daily"
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };
    let unencrypted = send(
        &app,
        "POST",
        "/api/v1/backup-jobs",
        owner.pair(),
        Some(job(json!({}))),
    )
    .await;
    assert_eq!(unencrypted.status, StatusCode::BAD_REQUEST);
    assert!(unencrypted.body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("master key"));
    let weak = send(
        &app,
        "POST",
        "/api/v1/backup-jobs",
        owner.pair(),
        Some(job(json!({ "encryption_passphrase": "only8chr" }))),
    )
    .await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST);
    let with_server = send(
        &app,
        "POST",
        "/api/v1/backup-jobs",
        owner.pair(),
        Some(job(json!({
            "encryption_passphrase": PASSPHRASE,
            "consistency": { "stop_service": true }
        }))),
    )
    .await;
    assert_eq!(with_server.status, StatusCode::BAD_REQUEST);
    let created = send(
        &app,
        "POST",
        "/api/v1/backup-jobs",
        owner.pair(),
        Some(job(json!({ "encryption_passphrase": PASSPHRASE }))),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["target_kind"], "panel_db");
    assert_eq!(created.body["encrypted"], true);
    let job_id = created.body["id"].as_str().unwrap();
    let cleared = send(
        &app,
        "PATCH",
        &format!("/api/v1/backup-jobs/{job_id}"),
        owner.pair(),
        Some(json!({ "encryption_passphrase": "" })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::BAD_REQUEST);
    let renamed = send(
        &app,
        "PATCH",
        &format!("/api/v1/backup-jobs/{job_id}"),
        owner.pair(),
        Some(json!({ "name": "panel-nightly" })),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    let settings_job = send(
        &app,
        "POST",
        "/api/v1/backup-jobs",
        owner.pair(),
        Some(job(
            json!({ "name": "settings", "target_kind": "panel_settings" }),
        )),
    )
    .await;
    assert_eq!(
        settings_job.status,
        StatusCode::CREATED,
        "{}",
        settings_job.body
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[sqlx::test(migrations = "./migrations")]
async fn panel_backup_restores_into_a_fresh_database(pool: PgPool) {
    if !tools_available() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("rudder-panel-{}", uuid::Uuid::new_v4()));
    let app = app(pool.clone(), &dir);
    let (owner, _viewer) = seed(&app).await;
    sqlx::query("INSERT INTO log_entries (logged_at, source, level, message) VALUES (now(), 'panel', 'info', 'history line')")
        .execute(&pool)
        .await
        .unwrap();

    let reply = send(
        &app,
        "POST",
        "/api/v1/system/backup",
        owner.pair(),
        Some(json!({ "passphrase": PASSPHRASE })),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&reply.raw)
    );
    let disposition = reply.headers["content-disposition"].to_str().unwrap();
    assert!(disposition.contains("rudder-panel-") && disposition.contains(".tar.zst.enc"));
    assert_eq!(&reply.raw[..8], b"RDRENC01");
    let files = entries(&unseal(&reply.raw, PASSPHRASE).await);
    let mut names: Vec<&str> = files.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "database.pgdump",
            "manifest.json",
            "master.key",
            "settings.json"
        ]
    );
    assert_eq!(
        String::from_utf8_lossy(&files["master.key"]).trim(),
        MASTER_HEX
    );
    let manifest: Value = serde_json::from_slice(&files["manifest.json"]).unwrap();
    assert_eq!(manifest["format"], "rudder-panel-backup");
    assert_eq!(manifest["migration"], rudder_server::known_migration());
    assert_eq!(manifest["includes_history"], false);
    assert!(files["database.pgdump"].starts_with(b"PGDMP"));
    assert!(!String::from_utf8_lossy(&files["settings.json"]).contains(SECRET_VALUE));
    let leftovers = std::fs::read_dir(dir.join("tmp")).unwrap().count();
    assert_eq!(leftovers, 0, "scratch directory not removed");

    let archive = dir.join("panel.tar.zst.enc");
    std::fs::write(&archive, &reply.raw).unwrap();
    let target = format!("rudder_restore_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {target}"))
        .execute(&pool)
        .await
        .unwrap();
    let mut target_url = url::Url::parse(&database_url_for(&pool)).unwrap();
    target_url.set_path(&target);
    let key_file = dir.join("etc").join("master.key");
    let options = |passphrase: &str, force: bool| rudder_server::RestoreOptions {
        archive: archive.clone(),
        passphrase: Some(passphrase.to_owned()),
        database_url: target_url.to_string(),
        master_key_out: key_file.clone(),
        force,
        pg_restore: "pg_restore".into(),
        known_migration: rudder_server::known_migration(),
    };

    let wrong = rudder_server::restore_panel(options("not the passphrase", false))
        .await
        .unwrap_err();
    assert!(format!("{wrong:#}").contains("passphrase"), "{wrong:#}");
    assert!(!key_file.exists());

    let mut too_new = options(PASSPHRASE, false);
    too_new.known_migration = 1;
    let refused = rudder_server::restore_panel(too_new).await.unwrap_err();
    assert!(format!("{refused:#}").contains("newer than this binary"));

    let report = rudder_server::restore_panel(options(PASSPHRASE, false))
        .await
        .unwrap();
    assert!(!report.replaced_existing);
    assert_eq!(
        std::fs::read_to_string(&key_file).unwrap().trim(),
        MASTER_HEX
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let restored = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(target_url.as_str())
        .await
        .unwrap();
    let project: String = sqlx::query_scalar("SELECT name FROM projects")
        .fetch_one(&restored)
        .await
        .unwrap();
    assert_eq!(project, "blog");
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&restored)
        .await
        .unwrap();
    assert_eq!(users, 2);
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&restored)
        .await
        .unwrap();
    assert_eq!(sessions, 0, "sessions are history and are not restored");
    let lines: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM log_entries")
        .fetch_one(&restored)
        .await
        .unwrap();
    assert_eq!(lines, 0);
    let value: Vec<u8> = sqlx::query_scalar("SELECT value_enc FROM env_vars")
        .fetch_one(&restored)
        .await
        .unwrap();
    let cipher = rudder_core::Cipher::new(hex::decode(MASTER_HEX).unwrap().try_into().unwrap());
    assert_eq!(cipher.decrypt(&value).unwrap(), SECRET_VALUE.as_bytes());
    let migrated: i64 =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(&restored)
            .await
            .unwrap();
    assert_eq!(migrated, rudder_server::known_migration());
    restored.close().await;

    let again = rudder_server::restore_panel(options(PASSPHRASE, false))
        .await
        .unwrap_err();
    assert!(format!("{again:#}").contains("--force"));
    std::fs::write(&key_file, "ab".repeat(32)).unwrap();
    let other_key = rudder_server::restore_panel(options(PASSPHRASE, false))
        .await
        .unwrap_err();
    assert!(format!("{other_key:#}").contains("different master key"));
    let forced = rudder_server::restore_panel(options(PASSPHRASE, true))
        .await
        .unwrap();
    assert!(forced.replaced_existing);
    assert_eq!(
        std::fs::read_to_string(&key_file).unwrap().trim(),
        MASTER_HEX
    );

    sqlx::query(&format!("DROP DATABASE {target} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.0])
            .output();
    }
}

async fn start_s3() -> (Container, String) {
    let name = format!(
        "rudder-s3-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    docker(&[
        "run",
        "-d",
        "--name",
        &name,
        "-p",
        "127.0.0.1::9000",
        "-e",
        "RUSTFS_ACCESS_KEY=rudderadmin",
        "-e",
        "RUSTFS_SECRET_KEY=rudder-secret-key",
        "rustfs/rustfs:1.0.1",
    ]);
    let guard = Container(name.clone());
    let port = docker(&["port", &name, "9000/tcp"]);
    let port = port
        .lines()
        .next()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .to_owned();
    let endpoint = format!("http://127.0.0.1:{port}");
    for _ in 0..60 {
        let made = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "host",
                "rclone/rclone:1",
                "mkdir",
                ":s3:panel",
                "--s3-provider",
                "Other",
                "--s3-endpoint",
                &endpoint,
                "--s3-access-key-id",
                "rudderadmin",
                "--s3-secret-access-key",
                "rudder-secret-key",
            ])
            .output()
            .unwrap();
        if made.status.success() {
            return (guard, endpoint);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("S3 test server never accepted the bucket");
}

async fn wait_run(app: &axum::Router, owner: &Session, run_id: &str) -> Value {
    for _ in 0..240 {
        let run = send(
            app,
            "GET",
            &format!("/api/v1/backup-runs/{run_id}"),
            owner.pair(),
            None,
        )
        .await;
        if !matches!(run.body["status"].as_str(), Some("queued" | "running")) {
            return run.body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("run {run_id} did not finish");
}

#[sqlx::test(migrations = "./migrations")]
async fn docker_panel_jobs_upload_to_s3(pool: PgPool) {
    if std::env::var("RUDDER_DOCKER_TESTS").as_deref() != Ok("1") || !tools_available() {
        eprintln!("skipping: set RUDDER_DOCKER_TESTS=1 to run the S3 panel job test");
        return;
    }
    let dir = std::env::temp_dir().join(format!("rudder-panel-{}", uuid::Uuid::new_v4()));
    let app = app(pool.clone(), &dir);
    let (owner, _viewer) = seed(&app).await;
    let (_s3, endpoint) = start_s3().await;
    let destination = send(
        &app,
        "POST",
        "/api/v1/s3-destinations",
        owner.pair(),
        Some(json!({
            "name": "offsite", "endpoint": endpoint, "region": "us-east-1",
            "bucket": "panel", "access_key_id": "rudderadmin",
            "secret_access_key": "rudder-secret-key", "path_style": true
        })),
    )
    .await;
    assert_eq!(destination.status, StatusCode::CREATED);
    let mut runs = HashMap::new();
    for (kind, passphrase) in [("panel_db", Some(PASSPHRASE)), ("panel_settings", None)] {
        let mut body = json!({
            "name": kind.replace('_', "-"),
            "target_kind": kind,
            "destination_ids": [destination.body["id"]],
            "verify_after_upload": true
        });
        if let Some(passphrase) = passphrase {
            body["encryption_passphrase"] = json!(passphrase);
        }
        let job = send(
            &app,
            "POST",
            "/api/v1/backup-jobs",
            owner.pair(),
            Some(body),
        )
        .await;
        assert_eq!(job.status, StatusCode::CREATED, "{}", job.body);
        let started = send(
            &app,
            "POST",
            &format!(
                "/api/v1/backup-jobs/{}/run",
                job.body["id"].as_str().unwrap()
            ),
            owner.pair(),
            None,
        )
        .await;
        assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.body);
        let run_id = started.body["run_id"].as_str().unwrap().to_owned();
        let run = wait_run(&app, &owner, &run_id).await;
        assert_eq!(run["status"], "succeeded", "{}", run["logs"]);
        assert!(!run["logs"].as_str().unwrap().contains(PASSPHRASE));
        runs.insert(kind, run_id);
    }

    let download = send(
        &app,
        "GET",
        &format!("/api/v1/backup-runs/{}/download", runs["panel_db"]),
        owner.pair(),
        None,
    )
    .await;
    assert_eq!(download.status, StatusCode::OK);
    assert!(download.headers["content-disposition"]
        .to_str()
        .unwrap()
        .contains(".tar.zst.enc"));
    let files = entries(&unseal(&download.raw, PASSPHRASE).await);
    assert_eq!(
        String::from_utf8_lossy(&files["master.key"]).trim(),
        MASTER_HEX
    );
    let refused = send(
        &app,
        "POST",
        &format!("/api/v1/backup-runs/{}/restore", runs["panel_db"]),
        owner.pair(),
        Some(json!({})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["error"]["code"], "offline_restore");

    let archive = dir.join("from-s3.tar.zst.enc");
    std::fs::write(&archive, &download.raw).unwrap();
    let target = format!("rudder_restore_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {target}"))
        .execute(&pool)
        .await
        .unwrap();
    let mut target_url = url::Url::parse(&database_url_for(&pool)).unwrap();
    target_url.set_path(&target);
    let report = rudder_server::restore_panel(rudder_server::RestoreOptions {
        archive,
        passphrase: Some(PASSPHRASE.into()),
        database_url: target_url.to_string(),
        master_key_out: dir.join("master.key"),
        force: false,
        pg_restore: "pg_restore".into(),
        known_migration: rudder_server::known_migration(),
    })
    .await
    .unwrap();
    assert_eq!(report.manifest.format, "rudder-panel-backup");

    let settings = send(
        &app,
        "GET",
        &format!("/api/v1/backup-runs/{}/download", runs["panel_settings"]),
        owner.pair(),
        None,
    )
    .await;
    assert_eq!(settings.status, StatusCode::OK);
    let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(settings.raw.as_slice());
    let mut json_bytes = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut decoder, &mut json_bytes)
        .await
        .unwrap();
    let export: Value = serde_json::from_slice(&json_bytes).unwrap();
    assert_eq!(export["projects"][0]["name"], "blog");

    sqlx::query(&format!("DROP DATABASE {target} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
