//! Integration tests for Rudder Image Store (Cloudflare R2 / S3), per-service
//! auto-deploy vs notify-only policy, R2 retention pruning, and re-pushing pruned images.

use std::collections::HashMap;
use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;

fn app(pool: PgPool, data_dir: &Path) -> axum::Router {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    let vars = HashMap::from([
        ("DATABASE_URL".into(), url),
        ("RUDDER_MASTER_KEY".into(), "dd".repeat(32)),
        ("RUDDER_COOKIE_SECURE".into(), "false".into()),
        ("RUDDER_PUBLIC_URL".into(), "https://panel.example".into()),
        ("RUDDER_DATA_DIR".into(), data_dir.display().to_string()),
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
    cookie: Option<String>,
}

async fn call(
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
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .and_then(|v| v.strip_prefix("rudder_session="))
        .map(str::to_owned);
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
        cookie,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn r2_image_store_push_retention_notify_and_repush(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("rudder-images-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let router = app(pool, &dir);

    let setup = call(
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
    assert_eq!(setup.status, StatusCode::OK);
    let cookie = setup.cookie.unwrap();
    let csrf = setup.body["csrf_token"].as_str().unwrap().to_owned();
    let session = Some((cookie.as_str(), csrf.as_str()));

    // Create server, project, environment, and service
    let server = call(
        &router,
        "POST",
        "/api/v1/servers",
        session,
        Some(json!({ "name": "local", "kind": "local" })),
    )
    .await;
    let project = call(
        &router,
        "POST",
        "/api/v1/projects",
        session,
        Some(json!({ "name": "acme" })),
    )
    .await;
    let env = call(
        &router,
        "POST",
        &format!(
            "/api/v1/projects/{}/environments",
            project.body["id"].as_str().unwrap()
        ),
        session,
        Some(json!({ "name": "prod" })),
    )
    .await;
    let service = call(
        &router,
        "POST",
        &format!(
            "/api/v1/environments/{}/services",
            env.body["id"].as_str().unwrap()
        ),
        session,
        Some(json!({
            "name": "api",
            "kind": "application",
            "server_id": server.body["id"],
            "spec": { "image": "rudder-image/api:initial" }
        })),
    )
    .await;
    assert_eq!(service.status, StatusCode::CREATED);
    let service_id = service.body["id"].as_str().unwrap().to_owned();

    // Pushing before configuring R2 fails with 409 image_store_unconfigured
    let unconfigured = call(
        &router,
        "POST",
        &format!("/api/v1/services/{service_id}/images"),
        session,
        Some(json!({
            "tag": "v1",
            "compression": "none",
            "archive_base64": base64::engine::general_purpose::STANDARD.encode(b"fake-tar-v1")
        })),
    )
    .await;
    assert_eq!(unconfigured.status, StatusCode::CONFLICT);
    assert_eq!(
        unconfigured.body["error"]["code"],
        "image_store_unconfigured"
    );

    // Create Cloudflare R2 destination (using local.r2.internal mock store)
    let dest = call(
        &router,
        "POST",
        "/api/v1/s3-destinations",
        session,
        Some(json!({
            "name": "cloudflare-r2",
            "endpoint": "http://local.r2.internal",
            "region": "auto",
            "bucket": "rudder-images",
            "access_key_id": "R2ACCESSKEY1234",
            "secret_access_key": "R2SECRETKEY567890",
            "path_style": true
        })),
    )
    .await;
    assert_eq!(dest.status, StatusCode::CREATED);
    let dest_id = dest.body["id"].as_str().unwrap().to_owned();

    // Configure Image Store settings
    let store_settings = call(
        &router,
        "PUT",
        "/api/v1/image-store/settings",
        session,
        Some(json!({
            "destination_id": dest_id,
            "path_prefix": "rudder-images",
            "r2_keep_count": 10
        })),
    )
    .await;
    assert_eq!(store_settings.status, StatusCode::OK);
    assert_eq!(store_settings.body["destination_name"], "cloudflare-r2");
    assert_eq!(store_settings.body["r2_keep_count"], 10);

    // Configure per-service policy: notify-only (image_auto_deploy = false) and keep last 2 in R2
    let policy = call(
        &router,
        "PUT",
        &format!("/api/v1/services/{service_id}/image-policy"),
        session,
        Some(json!({
            "image_auto_deploy": false,
            "image_r2_keep_count": 2,
            "image_r2_keep_days": null
        })),
    )
    .await;
    assert_eq!(policy.status, StatusCode::OK);
    assert_eq!(policy.body["image_auto_deploy"], false);
    assert_eq!(policy.body["image_r2_keep_count"], 2);

    // Push v1, v2, v3 and verify SHA-256 + R2 retention pruning
    for (tag, payload) in [
        ("v1", b"docker-image-archive-v1".as_slice()),
        ("v2", b"docker-image-archive-v2".as_slice()),
        ("v3", b"docker-image-archive-v3".as_slice()),
    ] {
        let sha = hex::encode(Sha256::digest(payload));
        let pushed = call(
            &router,
            "POST",
            &format!("/api/v1/services/{service_id}/images"),
            session,
            Some(json!({
                "tag": tag,
                "git_sha": "abc1234",
                "compression": "none",
                "expected_sha256": sha,
                "archive_base64": base64::engine::general_purpose::STANDARD.encode(payload)
            })),
        )
        .await;
        assert_eq!(
            pushed.status,
            StatusCode::CREATED,
            "push {tag}: {}",
            pushed.body
        );
        assert_eq!(pushed.body["deployed"], false);
        assert_eq!(pushed.body["image"]["sha256"], sha);
        if tag == "v3" {
            let pruned = pushed.body["pruned_tags"].as_array().unwrap();
            assert_eq!(pruned.len(), 1);
            assert_eq!(pruned[0], "v1");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Verify notification event was emitted in notify-only mode
    let events = call(&router, "GET", "/api/v1/events", session, None).await;
    assert_eq!(events.status, StatusCode::OK);
    assert!(events
        .body
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "image.pushed"));

    // Verify list_service_images shows v3 and v2 present in R2, and v1 pruned from R2 (r2_present = false)
    let list = call(
        &router,
        "GET",
        &format!("/api/v1/services/{service_id}/images"),
        session,
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    let images = list.body.as_array().unwrap();
    assert_eq!(images.len(), 3);
    let v1_row = images.iter().find(|i| i["tag"] == "v1").unwrap();
    let v2_row = images.iter().find(|i| i["tag"] == "v2").unwrap();
    let v3_row = images.iter().find(|i| i["tag"] == "v3").unwrap();
    assert_eq!(v1_row["r2_present"], false);
    assert_eq!(v2_row["r2_present"], true);
    assert_eq!(v3_row["r2_present"], true);

    // Deploying pruned v1 fails with 409 image_pruned_from_r2
    let deploy_pruned = call(
        &router,
        "POST",
        &format!("/api/v1/services/{service_id}/images/deploy"),
        session,
        Some(json!({ "tag": "v1" })),
    )
    .await;
    assert_eq!(deploy_pruned.status, StatusCode::CONFLICT);
    assert_eq!(deploy_pruned.body["error"]["code"], "image_pruned_from_r2");

    // Re-pushing v1 from local developer storage restores r2_present = true on v1 (and prunes v2)
    let re_pushed = call(
        &router,
        "POST",
        &format!("/api/v1/services/{service_id}/images"),
        session,
        Some(json!({
            "tag": "v1",
            "compression": "none",
            "archive_base64": base64::engine::general_purpose::STANDARD.encode(b"docker-image-archive-v1")
        })),
    )
    .await;
    assert_eq!(re_pushed.status, StatusCode::CREATED);
    assert_eq!(re_pushed.body["image"]["r2_present"], true);
    assert_eq!(re_pushed.body["pruned_tags"][0], "v2");

    // The dashboard overview reflects setup, totals, and the per-service rollup.
    let overview = call(
        &router,
        "GET",
        "/api/v1/image-store/overview",
        session,
        None,
    )
    .await;
    assert_eq!(overview.status, StatusCode::OK, "{}", overview.body);
    assert_eq!(overview.body["destination"]["name"], "cloudflare-r2");
    assert_eq!(overview.body["totals"]["images"], 3);
    assert_eq!(overview.body["totals"]["r2_images"], 2);
    assert_eq!(overview.body["totals"]["pruned_images"], 1);
    assert_eq!(overview.body["totals"]["pushes_24h"], 3);
    assert_eq!(overview.body["services"][0]["path"], "acme/prod/api");
    assert_eq!(overview.body["recent_pushes"].as_array().unwrap().len(), 3);
    assert_eq!(overview.body["daily"].as_array().unwrap().len(), 30);
    let step = |key: &str| {
        overview.body["setup"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == key)
            .unwrap()["state"]
            .clone()
    };
    assert_eq!(step("bucket"), "ok");
    assert_eq!(step("retention"), "ok");
    assert_eq!(step("first_push"), "ok");
    assert_eq!(step("client"), "todo");
    assert_eq!(step("auto_deploy"), "optional");

    let probe = call(&router, "POST", "/api/v1/image-store/test", session, None).await;
    assert_eq!(probe.status, StatusCode::OK, "{}", probe.body);
    assert_eq!(probe.body["ok"], true, "{}", probe.body);
}

#[sqlx::test(migrations = "./migrations")]
async fn compose_service_accepts_image_push(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("rudder-images-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let router = app(pool, &dir);

    let setup = call(
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
    assert_eq!(setup.status, StatusCode::OK);
    let cookie = setup.cookie.unwrap();
    let csrf = setup.body["csrf_token"].as_str().unwrap().to_owned();
    let session = Some((cookie.as_str(), csrf.as_str()));

    let server = call(
        &router,
        "POST",
        "/api/v1/servers",
        session,
        Some(json!({ "name": "local", "kind": "local" })),
    )
    .await;
    let project = call(
        &router,
        "POST",
        "/api/v1/projects",
        session,
        Some(json!({ "name": "acme" })),
    )
    .await;
    let env = call(
        &router,
        "POST",
        &format!(
            "/api/v1/projects/{}/environments",
            project.body["id"].as_str().unwrap()
        ),
        session,
        Some(json!({ "name": "prod" })),
    )
    .await;
    let stack = call(
        &router,
        "POST",
        &format!(
            "/api/v1/environments/{}/stacks",
            env.body["id"].as_str().unwrap()
        ),
        session,
        Some(json!({
            "name": "shop",
            "server_id": server.body["id"],
            "source": "inline",
            "compose_yaml": "services:\n  web:\n    image: rudder-image/shop:v1\n"
        })),
    )
    .await;
    assert_eq!(stack.status, StatusCode::CREATED, "{}", stack.body);
    let stack_id = stack.body["id"].as_str().unwrap().to_owned();

    let dest = call(
        &router,
        "POST",
        "/api/v1/s3-destinations",
        session,
        Some(json!({
            "name": "cloudflare-r2",
            "endpoint": "http://local.r2.internal",
            "region": "auto",
            "bucket": "rudder-images",
            "access_key_id": "R2ACCESSKEY1234",
            "secret_access_key": "R2SECRETKEY567890",
            "path_style": true
        })),
    )
    .await;
    assert_eq!(dest.status, StatusCode::CREATED);
    let store = call(
        &router,
        "PUT",
        "/api/v1/image-store/settings",
        session,
        Some(json!({ "destination_id": dest.body["id"], "path_prefix": "rudder-images" })),
    )
    .await;
    assert_eq!(store.status, StatusCode::OK);

    let pushed = call(
        &router,
        "POST",
        &format!("/api/v1/services/{stack_id}/images"),
        session,
        Some(json!({
            "tag": "v1",
            "compression": "none",
            "deploy": false,
            "archive_base64": base64::engine::general_purpose::STANDARD.encode(b"fake-tar")
        })),
    )
    .await;
    assert_eq!(pushed.status, StatusCode::CREATED, "{}", pushed.body);
    assert_eq!(pushed.body["image"]["tag"], "v1");
    assert_eq!(pushed.body["deployed"], false);
}
