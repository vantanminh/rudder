use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

const REQUIRED_TABLES: &[&str] = &[
    "users",
    "sessions",
    "api_tokens",
    "audit_log",
    "servers",
    "projects",
    "environments",
    "services",
    "env_vars",
    "domains",
    "certificates",
    "cache_policies",
    "cache_rules",
    "deployments",
    "service_containers",
    "registries",
    "git_providers",
    "deploy_keys",
    "backup_jobs",
    "backup_runs",
    "s3_destinations",
    "notification_channels",
    "alerts",
    "metrics_samples",
    "metrics_rollups",
    "log_entries",
    "retention_settings",
    "update_history",
    "panel_settings",
    "internal_ca",
    "build_queue",
    "webhook_deliveries",
    "cleanup_schedules",
    "oauth_clients",
    "mcp_grants",
    "oauth_codes",
    "mcp_refresh_tokens",
    "mcp_access_tokens",
    "cache_stat_samples",
    "access_settings",
    "access_departments",
    "access_ranks",
    "access_identities",
    "access_memberships",
    "access_admins",
    "access_policies",
    "access_rules",
    "access_sessions",
    "access_codes",
    "access_service_tokens",
    "rate_limit_rules",
    "rate_limit_samples",
    "rate_limit_blocks",
    "security_scans",
    "security_findings",
    "ssl_events",
    "edge_counters",
    "cloudflare_settings",
];

fn test_config(database_url: &str) -> rudder_server::Config {
    let vars = HashMap::from([
        ("DATABASE_URL".into(), database_url.to_owned()),
        ("RUDDER_MASTER_KEY".into(), "aa".repeat(32)),
        ("RUDDER_COOKIE_SECURE".into(), "false".into()),
        ("RUDDER_LOG_FORMAT".into(), "pretty".into()),
    ]);
    rudder_server::Config::from_map(&vars, |_| unreachable!("no key file")).expect("config")
}

#[sqlx::test(migrations = "./migrations")]
async fn migration_creates_required_tables(pool: PgPool) {
    let names = sqlx::query_scalar::<_, String>(
        r#"
        SELECT table_name
        FROM information_schema.tables
        WHERE table_schema = 'public'
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    for required in REQUIRED_TABLES {
        assert!(
            names.iter().any(|name| name == required),
            "missing table {required}"
        );
    }

    let settings =
        sqlx::query!(r#"SELECT github_repo, auto_update FROM panel_settings WHERE id = TRUE"#)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settings.github_repo, "vantanminh/rudder");
    assert!(!settings.auto_update);

    rudder_server::ensure_partitions(&pool).await.unwrap();
    let partition: String = sqlx::query_scalar(
        r#"
        SELECT inhrelid::regclass::text
        FROM pg_inherits
        WHERE inhparent = 'log_entries'::regclass
          AND inhrelid::regclass::text LIKE 'log_entries_%'
          AND inhrelid::regclass::text <> 'log_entries_default'
        ORDER BY 1
        LIMIT 1
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(partition.starts_with("log_entries_"));
}

#[sqlx::test(migrations = "./migrations")]
async fn health_endpoint_reports_ok(pool: PgPool) {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    let state = rudder_server::AppState::new(pool, test_config(&url));
    let app = rudder_server::router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        response
            .headers()
            .get("x-frame-options")
            .and_then(|value| value.to_str().ok()),
        Some("DENY")
    );
    assert!(response.headers().get("content-security-policy").is_some());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["product"], "rudder");
    assert_eq!(json["version"], rudder_core::VERSION);
}
