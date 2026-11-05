//! Owner setup, sessions, TOTP, roles, API tokens, and the audit log.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rudder_server::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use totp_rs::{Builder, Secret};
use tower::ServiceExt;

fn test_config(database_url: &str) -> rudder_server::Config {
    let vars = HashMap::from([
        ("DATABASE_URL".into(), database_url.to_owned()),
        ("RUDDER_MASTER_KEY".into(), "bb".repeat(32)),
        ("RUDDER_COOKIE_SECURE".into(), "false".into()),
        ("RUDDER_LOG_FORMAT".into(), "pretty".into()),
    ]);
    rudder_server::Config::from_map(&vars, |_| unreachable!("no key file")).expect("config")
}

fn app(pool: PgPool) -> axum::Router {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    rudder_server::router(AppState::new(pool, test_config(&url)))
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
    cookie: Option<&str>,
    csrf: Option<&str>,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("rudder_session={cookie}"));
    }
    if let Some(csrf) = csrf {
        builder = builder.header("x-csrf-token", csrf);
    }
    if let Some(bearer) = bearer {
        builder = builder.header("authorization", format!("Bearer {bearer}"));
    }
    let request = if let Some(body) = body {
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .and_then(|pair| pair.strip_prefix("rudder_session="))
        .map(str::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    Reply {
        status,
        body,
        cookie,
    }
}

fn session_of(reply: &Reply) -> (String, String) {
    let cookie = reply.cookie.clone().expect("session cookie");
    let csrf = reply.body["csrf_token"].as_str().expect("csrf").to_owned();
    (cookie, csrf)
}

#[sqlx::test(migrations = "./migrations")]
async fn setup_login_logout_and_csrf(pool: PgPool) {
    let app = app(pool);
    let status = call(&app, "GET", "/api/v1/setup/status", None, None, None, None).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.body["needs_setup"], true);

    let created = call(
        &app,
        "POST",
        "/api/v1/setup",
        None,
        None,
        None,
        Some(json!({
            "email": "Owner@Example.com",
            "password": "correct-horse-battery",
            "display_name": "Owner"
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK);
    assert_eq!(created.body["user"]["email"], "owner@example.com");
    assert_eq!(created.body["user"]["role"], "admin");
    let (cookie, csrf) = session_of(&created);

    let again = call(
        &app,
        "POST",
        "/api/v1/setup",
        None,
        None,
        None,
        Some(json!({
            "email": "other@example.com",
            "password": "correct-horse-battery"
        })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    let me = call(&app, "GET", "/api/v1/me", Some(&cookie), None, None, None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["user"]["display_name"], "Owner");

    let missing_csrf = call(
        &app,
        "POST",
        "/api/v1/me/password",
        Some(&cookie),
        None,
        None,
        Some(json!({
            "current_password": "correct-horse-battery",
            "new_password": "another-strong-secret"
        })),
    )
    .await;
    assert_eq!(missing_csrf.status, StatusCode::FORBIDDEN);
    assert_eq!(missing_csrf.body["error"]["code"], "csrf");

    let changed = call(
        &app,
        "POST",
        "/api/v1/me/password",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({
            "current_password": "correct-horse-battery",
            "new_password": "another-strong-secret"
        })),
    )
    .await;
    assert_eq!(changed.status, StatusCode::NO_CONTENT);

    let stale = call(&app, "GET", "/api/v1/me", Some(&cookie), None, None, None).await;
    assert_eq!(stale.status, StatusCode::UNAUTHORIZED);

    let bad = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "not-the-password"
        })),
    )
    .await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);

    let login = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "another-strong-secret"
        })),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    let (cookie, csrf) = session_of(&login);
    let logout = call(
        &app,
        "POST",
        "/api/v1/logout",
        Some(&cookie),
        Some(&csrf),
        None,
        None,
    )
    .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    let after = call(&app, "GET", "/api/v1/me", Some(&cookie), None, None, None).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);

    let spec = call(&app, "GET", "/api/openapi.json", None, None, None, None).await;
    assert_eq!(spec.status, StatusCode::OK);
    let text = spec.body.to_string();
    assert!(text.contains("/api/v1/login"));
    assert!(text.contains("Rudder API"));
}

#[sqlx::test(migrations = "./migrations")]
async fn totp_setup_enable_and_login(pool: PgPool) {
    let app = app(pool);
    let created = call(
        &app,
        "POST",
        "/api/v1/setup",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery"
        })),
    )
    .await;
    let (cookie, csrf) = session_of(&created);
    let setup = call(
        &app,
        "POST",
        "/api/v1/me/totp/setup",
        Some(&cookie),
        Some(&csrf),
        None,
        None,
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK);
    let secret = setup.body["secret"].as_str().unwrap();
    assert!(setup.body["otpauth_url"]
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/"));
    let code = current_code(secret, "owner@example.com");
    let enabled = call(
        &app,
        "POST",
        "/api/v1/me/totp/enable",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({ "code": code })),
    )
    .await;
    assert_eq!(enabled.status, StatusCode::OK);
    let recovery = enabled.body["recovery_codes"].as_array().unwrap();
    assert_eq!(recovery.len(), 8);
    let backup = recovery[0].as_str().unwrap().to_owned();

    let needs_code = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery"
        })),
    )
    .await;
    assert_eq!(needs_code.status, StatusCode::UNAUTHORIZED);
    assert_eq!(needs_code.body["error"]["code"], "totp_required");

    let code = current_code(secret, "owner@example.com");
    let login = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery",
            "totp_code": code
        })),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(login.body["user"]["totp_enabled"], true);
    assert_eq!(login.body["user"]["is_owner"], true);

    let backup_login = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery",
            "totp_code": backup
        })),
    )
    .await;
    assert_eq!(backup_login.status, StatusCode::OK);
    let reused = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery",
            "totp_code": backup
        })),
    )
    .await;
    assert_eq!(reused.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn roles_and_api_tokens(pool: PgPool) {
    let app = app(pool);
    let created = call(
        &app,
        "POST",
        "/api/v1/setup",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery"
        })),
    )
    .await;
    let (cookie, csrf) = session_of(&created);
    let viewer = call(
        &app,
        "POST",
        "/api/v1/users",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({
            "email": "viewer@example.com",
            "password": "viewer-password-ok",
            "role": "viewer"
        })),
    )
    .await;
    assert_eq!(viewer.status, StatusCode::CREATED);
    let viewer_id = viewer.body["id"].as_str().unwrap().to_owned();

    let viewer_login = call(
        &app,
        "POST",
        "/api/v1/login",
        None,
        None,
        None,
        Some(json!({
            "email": "viewer@example.com",
            "password": "viewer-password-ok"
        })),
    )
    .await;
    let (viewer_cookie, viewer_csrf) = session_of(&viewer_login);
    let forbidden = call(
        &app,
        "POST",
        "/api/v1/users",
        Some(&viewer_cookie),
        Some(&viewer_csrf),
        None,
        Some(json!({
            "email": "extra@example.com",
            "password": "extra-password-ok",
            "role": "viewer"
        })),
    )
    .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);

    let read_token = call(
        &app,
        "POST",
        "/api/v1/tokens",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({ "name": "ci", "scopes": ["read"] })),
    )
    .await;
    assert_eq!(read_token.status, StatusCode::CREATED);
    let read = read_token.body["token"].as_str().unwrap().to_owned();
    assert!(read.starts_with("rdr_"));
    let listed = call(&app, "GET", "/api/v1/users", None, None, Some(&read), None).await;
    assert_eq!(listed.status, StatusCode::OK);
    let write_denied = call(
        &app,
        "DELETE",
        &format!("/api/v1/users/{viewer_id}"),
        None,
        None,
        Some(&read),
        None,
    )
    .await;
    assert_eq!(write_denied.status, StatusCode::FORBIDDEN);

    let write_token = call(
        &app,
        "POST",
        "/api/v1/tokens",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({ "name": "deploy", "scopes": ["read", "write"] })),
    )
    .await;
    let write = write_token.body["token"].as_str().unwrap();
    let removed = call(
        &app,
        "DELETE",
        &format!("/api/v1/users/{viewer_id}"),
        None,
        None,
        Some(write),
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);

    let owner_id = created.body["user"]["id"].as_str().unwrap();
    let demote = call(
        &app,
        "PATCH",
        &format!("/api/v1/users/{owner_id}"),
        None,
        None,
        Some(write),
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!(demote.status, StatusCode::CONFLICT);
    let remove_owner = call(
        &app,
        "DELETE",
        &format!("/api/v1/users/{owner_id}"),
        None,
        None,
        Some(write),
        None,
    )
    .await;
    assert_eq!(remove_owner.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn audit_log_records_mutations(pool: PgPool) {
    let app = app(pool);
    let created = call(
        &app,
        "POST",
        "/api/v1/setup",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery"
        })),
    )
    .await;
    let (cookie, csrf) = session_of(&created);
    let _ = call(
        &app,
        "POST",
        "/api/v1/tokens",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({ "name": "audit", "scopes": ["read"] })),
    )
    .await;
    let audit = call(
        &app,
        "GET",
        "/api/v1/audit",
        Some(&cookie),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(audit.status, StatusCode::OK);
    let actions: Vec<&str> = audit
        .body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["action"].as_str())
        .collect();
    assert!(actions.contains(&"user.setup"));
    assert!(actions.contains(&"token.create"));
    let leaked = audit.body.to_string();
    assert!(!leaked.contains("correct-horse-battery"));
    assert!(!leaked.contains("rdr_"));
}

#[sqlx::test(migrations = "./migrations")]
async fn cli_auth_device_code_browser_and_revocation(pool: PgPool) {
    let app = app(pool);
    let created = call(
        &app,
        "POST",
        "/api/v1/setup",
        None,
        None,
        None,
        Some(json!({
            "email": "owner@example.com",
            "password": "correct-horse-battery"
        })),
    )
    .await;
    let (cookie, csrf) = session_of(&created);

    // 1. Device code flow (8-character alphanumeric code with letters + digits)
    let start_dev = call(
        &app,
        "POST",
        "/api/v1/cli/auth/start",
        None,
        None,
        None,
        Some(json!({
            "method": "device_code",
            "client_name": "dev-macbook"
        })),
    )
    .await;
    assert_eq!(start_dev.status, StatusCode::CREATED);
    let dev_req_id = start_dev.body["request_id"].as_str().unwrap().to_owned();
    let dev_poll_secret = start_dev.body["poll_secret"].as_str().unwrap().to_owned();
    let device_code = start_dev.body["device_code"].as_str().unwrap().to_owned();
    assert_eq!(device_code.len(), 8);
    assert!(device_code.chars().any(|c| c.is_ascii_alphabetic()));
    assert!(device_code.chars().any(|c| c.is_ascii_digit()));

    let pending = call(
        &app,
        "POST",
        "/api/v1/cli/auth/poll",
        None,
        None,
        None,
        Some(json!({
            "request_id": dev_req_id,
            "poll_secret": dev_poll_secret
        })),
    )
    .await;
    assert_eq!(pending.status, StatusCode::OK);
    assert_eq!(pending.body["status"], "pending");

    let approved_dev = call(
        &app,
        "POST",
        "/api/v1/cli/auth/approve",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({
            "device_code": device_code.to_ascii_lowercase()
        })),
    )
    .await;
    assert_eq!(approved_dev.status, StatusCode::OK);
    let dev_token_id = approved_dev.body["token_id"].as_str().unwrap().to_owned();

    let polled_dev = call(
        &app,
        "POST",
        "/api/v1/cli/auth/poll",
        None,
        None,
        None,
        Some(json!({
            "request_id": dev_req_id,
            "poll_secret": dev_poll_secret
        })),
    )
    .await;
    assert_eq!(polled_dev.status, StatusCode::OK);
    assert_eq!(polled_dev.body["status"], "approved");
    let dev_token = polled_dev.body["token"].as_str().unwrap().to_owned();
    assert!(dev_token.starts_with("rdr_"));

    // Polling a second time must not return the raw token again
    let polled_again = call(
        &app,
        "POST",
        "/api/v1/cli/auth/poll",
        None,
        None,
        None,
        Some(json!({
            "request_id": dev_req_id,
            "poll_secret": dev_poll_secret
        })),
    )
    .await;
    assert_eq!(polled_again.status, StatusCode::OK);
    assert_eq!(polled_again.body["status"], "consumed");
    assert!(polled_again.body["token"].is_null());

    // 2. Browser callback flow
    let start_browser = call(
        &app,
        "POST",
        "/api/v1/cli/auth/start",
        None,
        None,
        None,
        Some(json!({
            "method": "browser",
            "client_name": "dev-windows",
            "callback_url": "http://127.0.0.1:45678/callback"
        })),
    )
    .await;
    assert_eq!(start_browser.status, StatusCode::CREATED);
    let browser_req_id = start_browser.body["request_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let inspect = call(
        &app,
        "GET",
        &format!("/api/v1/cli/auth/requests/{browser_req_id}"),
        Some(&cookie),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(inspect.status, StatusCode::OK);
    assert_eq!(inspect.body["client_name"], "dev-windows");
    assert_eq!(inspect.body["method"], "browser");

    let approved_browser = call(
        &app,
        "POST",
        "/api/v1/cli/auth/approve",
        Some(&cookie),
        Some(&csrf),
        None,
        Some(json!({
            "request_id": browser_req_id
        })),
    )
    .await;
    assert_eq!(approved_browser.status, StatusCode::OK);
    let browser_token = approved_browser.body["token"].as_str().unwrap().to_owned();
    let browser_token_id = approved_browser.body["token_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        approved_browser.body["callback_url"],
        "http://127.0.0.1:45678/callback"
    );

    // Both tokens work for authenticated API requests
    let me_dev = call(
        &app,
        "GET",
        "/api/v1/me",
        None,
        None,
        Some(&dev_token),
        None,
    )
    .await;
    assert_eq!(me_dev.status, StatusCode::OK);
    let me_browser = call(
        &app,
        "GET",
        "/api/v1/me",
        None,
        None,
        Some(&browser_token),
        None,
    )
    .await;
    assert_eq!(me_browser.status, StatusCode::OK);

    // List tokens shows auth_method for each login
    let tokens = call(
        &app,
        "GET",
        "/api/v1/tokens",
        Some(&cookie),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(tokens.status, StatusCode::OK);
    let token_arr = tokens.body.as_array().unwrap();
    assert!(token_arr
        .iter()
        .any(|t| t["id"] == dev_token_id && t["auth_method"] == "device_code"));
    assert!(token_arr
        .iter()
        .any(|t| t["id"] == browser_token_id && t["auth_method"] == "browser"));

    // Revoke device-code token and verify it is immediately rejected
    let revoke = call(
        &app,
        "DELETE",
        &format!("/api/v1/tokens/{dev_token_id}"),
        Some(&cookie),
        Some(&csrf),
        None,
        None,
    )
    .await;
    assert_eq!(revoke.status, StatusCode::NO_CONTENT);
    let after_revoke = call(
        &app,
        "GET",
        "/api/v1/me",
        None,
        None,
        Some(&dev_token),
        None,
    )
    .await;
    assert_eq!(after_revoke.status, StatusCode::UNAUTHORIZED);
}

fn current_code(secret_b32: &str, email: &str) -> String {
    let secret = Secret::try_from_base32(secret_b32).unwrap();
    let totp = Builder::new()
        .with_secret(secret)
        .with_account_name(email)
        .with_issuer(Some("Rudder"))
        .build()
        .unwrap();
    totp.generate_current().to_string()
}
