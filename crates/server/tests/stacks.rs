//! Compose stacks and app templates.
//!
//! `api_without_docker` runs everywhere. `docker_stack_lifecycle` runs real
//! `docker compose` projects and only runs with `RUDDER_DOCKER_TESTS=1`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

fn app(pool: PgPool, data_dir: &Path) -> axum::Router {
    let url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/rudder".into());
    let vars = HashMap::from([
        ("DATABASE_URL".into(), url),
        ("RUDDER_MASTER_KEY".into(), "cc".repeat(32)),
        ("RUDDER_COOKIE_SECURE".into(), "false".into()),
        ("RUDDER_PUBLIC_URL".into(), "https://panel.example".into()),
        ("RUDDER_DATA_DIR".into(), data_dir.display().to_string()),
    ]);
    let config = rudder_server::Config::from_map(&vars, |_| unreachable!()).unwrap();
    rudder_server::router(rudder_server::AppState::new(pool, config))
}

struct Reply {
    status: StatusCode,
    body: Value,
    raw: String,
}

struct Client {
    app: axum::Router,
    cookie: String,
    csrf: String,
}

impl Client {
    async fn owner(app: axum::Router) -> Self {
        let request = Request::builder()
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
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .and_then(|pair| pair.strip_prefix("rudder_session="))
            .unwrap()
            .to_owned();
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        Self {
            app,
            cookie,
            csrf: body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Reply {
        let builder = Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", format!("rudder_session={}", self.cookie))
            .header("x-csrf-token", &self.csrf);
        let request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            raw: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let reply = self.call(method, path, body).await;
        assert!(
            reply.status.is_success(),
            "{method} {path} -> {} {}",
            reply.status,
            reply.body
        );
        reply.body
    }

    async fn expect(
        &self,
        status: StatusCode,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Value {
        let reply = self.call(method, path, body).await;
        assert_eq!(reply.status, status, "{method} {path}: {}", reply.body);
        reply.body
    }

    async fn environment(&self, server_kind: &str) -> (String, String) {
        let server = self
            .ok(
                "POST",
                "/api/v1/servers",
                Some(json!({ "name": server_kind, "kind": server_kind })),
            )
            .await;
        let project = self
            .ok("POST", "/api/v1/projects", Some(json!({ "name": "apps" })))
            .await;
        let environment = self
            .ok(
                "POST",
                &format!(
                    "/api/v1/projects/{}/environments",
                    project["id"].as_str().unwrap()
                ),
                Some(json!({ "name": "prod" })),
            )
            .await;
        (
            server["id"].as_str().unwrap().to_owned(),
            environment["id"].as_str().unwrap().to_owned(),
        )
    }

    async fn wait_deployment(&self, service_id: &str, deployment_id: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        loop {
            let list = self
                .ok(
                    "GET",
                    &format!("/api/v1/services/{service_id}/deployments"),
                    None,
                )
                .await;
            let found = list
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == deployment_id)
                .cloned()
                .expect("deployment listed");
            if found["status"] != "running" && found["status"] != "queued" {
                return found;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "deployment still running"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn latest_deployment(&self, service_id: &str) -> Value {
        let list = self
            .ok(
                "GET",
                &format!("/api/v1/services/{service_id}/deployments"),
                None,
            )
            .await;
        list.as_array()
            .unwrap()
            .first()
            .cloned()
            .unwrap_or(Value::Null)
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rudder-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const COMPOSE: &str = r#"
version: "3.9"
services:
  web:
    image: nginx:1.27-alpine
    environment:
      GREETING: ${GREETING}
  worker:
    image: busybox:1.36
    command: ["sh", "-c", "echo token=$$TOKEN; echo ready; sleep 3600"]
    environment:
      TOKEN: ${TOKEN}
"#;

async fn rename_project(client: &Client, id: &str, project: &str) -> Reply {
    client
        .call(
            "PUT",
            &format!("/api/v1/services/{id}/stack"),
            Some(json!({
                "source": "inline", "main_service": "web",
                "compose_yaml": COMPOSE, "project": project
            })),
        )
        .await
}

#[sqlx::test(migrations = "./migrations")]
async fn api_without_docker(pool: PgPool) {
    let data_dir = temp_dir("stacks");
    let client = Client::owner(app(pool.clone(), &data_dir)).await;

    let templates = client.ok("GET", "/api/v1/templates", None).await;
    let slugs: Vec<&str> = templates
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["slug"].as_str().unwrap())
        .collect();
    assert!(slugs.len() >= 10, "{slugs:?}");
    for slug in [
        "n8n",
        "uptime-kuma",
        "gitea",
        "vaultwarden",
        "umami",
        "plausible",
        "ghost",
        "wordpress",
        "minio",
        "metabase",
    ] {
        assert!(slugs.contains(&slug), "{slug} missing");
    }
    let gitea = client.ok("GET", "/api/v1/templates/gitea", None).await;
    assert!(gitea["compose"].as_str().unwrap().contains("gitea/gitea"));
    assert_eq!(gitea["main_service"], "gitea");
    assert!(gitea["variables"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["kind"] == "domain"));
    client
        .expect(StatusCode::NOT_FOUND, "GET", "/api/v1/templates/nope", None)
        .await;

    let (server_id, environment_id) = client.environment("remote").await;
    let stacks_path = format!("/api/v1/environments/{environment_id}/stacks");
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &stacks_path,
            Some(json!({ "name": "bad", "server_id": server_id, "source": "inline", "compose_yaml": "services: [" })),
        )
        .await;
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &stacks_path,
            Some(json!({ "name": "bad", "server_id": server_id, "source": "inline", "compose_yaml": COMPOSE, "main_service": "api" })),
        )
        .await;
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &stacks_path,
            Some(
                json!({ "name": "bad", "server_id": server_id, "source": "git", "git": {
                    "provider_id": uuid::Uuid::nil(), "repo": "https://github.com/acme/app",
                    "branch": "main", "compose_file": "../compose.yml"
                }}),
            ),
        )
        .await;

    let created = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &stacks_path,
            Some(json!({
                "name": "blog", "server_id": server_id, "source": "inline",
                "compose_yaml": COMPOSE, "main_service": "web"
            })),
        )
        .await;
    let stack_id = created["service"]["id"].as_str().unwrap().to_owned();
    let project = created["project"].as_str().unwrap().to_owned();
    assert!(
        project.starts_with("blog-") && project.len() == 11,
        "{project}"
    );
    assert_eq!(created["service"]["kind"], "compose");
    let generated = created["generated_yaml"].as_str().unwrap();
    assert!(generated.contains(&format!("name: {project}")));
    assert!(generated.contains("rudder-proxy"));
    assert!(!generated.contains("version:"));
    assert!(
        created["compose_yaml"]
            .as_str()
            .unwrap()
            .contains("version:"),
        "source kept as written"
    );
    assert_eq!(created["missing_variables"], json!(["GREETING", "TOKEN"]));
    let services = created["services"].as_array().unwrap();
    let web = services.iter().find(|s| s["name"] == "web").unwrap();
    assert_eq!(web["container"], format!("{project}-web-1"));
    assert_eq!(web["routed"], true);
    assert!(web["running"].is_null(), "remote server is unreachable");
    let worker = services.iter().find(|s| s["name"] == "worker").unwrap();
    assert_eq!(worker["routed"], false);

    client
        .ok(
            "PUT",
            &format!("/api/v1/services/{stack_id}/env"),
            Some(json!({ "key": "TOKEN", "value": "tok-very-secret", "is_build_secret": false })),
        )
        .await;
    let view = client
        .ok("GET", &format!("/api/v1/services/{stack_id}/stack"), None)
        .await;
    assert_eq!(view["missing_variables"], json!(["GREETING"]));

    let up = client
        .expect(
            StatusCode::CONFLICT,
            "POST",
            &format!("/api/v1/services/{stack_id}/stack/up"),
            None,
        )
        .await;
    assert_eq!(up["error"]["code"], "remote_unavailable");
    let down = client
        .expect(
            StatusCode::CONFLICT,
            "POST",
            &format!("/api/v1/services/{stack_id}/stack/down"),
            None,
        )
        .await;
    assert_eq!(down["error"]["code"], "not_deployed");
    client
        .expect(
            StatusCode::NOT_FOUND,
            "POST",
            &format!("/api/v1/services/{stack_id}/stack/explode"),
            None,
        )
        .await;
    client
        .expect(
            StatusCode::NOT_FOUND,
            "GET",
            &format!("/api/v1/services/{stack_id}/stack/logs?service=nope"),
            None,
        )
        .await;

    let domains = format!("/api/v1/services/{stack_id}/domains");
    let domain = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &domains,
            Some(
                json!({ "host": "blog.example.com", "target_port": 80, "ssl_mode": "letsencrypt" }),
            ),
        )
        .await;
    assert_eq!(domain["compose_service"], "web");
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &domains,
            Some(json!({ "host": "x.example.com", "target_port": 80, "ssl_mode": "letsencrypt", "compose_service": "ghost" })),
        )
        .await;
    let worker_domain = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &domains,
            Some(json!({ "host": "worker.example.com", "target_port": 8080, "ssl_mode": "none", "compose_service": "worker" })),
        )
        .await;
    assert_eq!(worker_domain["compose_service"], "worker");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let view = client
            .ok("GET", &format!("/api/v1/services/{stack_id}/stack"), None)
            .await;
        let routed = view["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "worker")
            .unwrap()["routed"]
            .as_bool()
            .unwrap();
        if routed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker never joined the proxy network"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let removed_target = client
        .expect(
            StatusCode::BAD_REQUEST,
            "PUT",
            &format!("/api/v1/services/{stack_id}/stack"),
            Some(json!({
                "source": "inline", "main_service": "web",
                "compose_yaml": "services:\n  web:\n    image: nginx:1.27-alpine\n"
            })),
        )
        .await;
    assert!(removed_target["error"]["message"]
        .as_str()
        .unwrap()
        .contains("worker"));
    let updated = client
        .ok(
            "PUT",
            &format!("/api/v1/services/{stack_id}/stack"),
            Some(json!({
                "source": "inline", "main_service": "web",
                "compose_yaml": COMPOSE.replace("busybox:1.36", "busybox:1.37")
            })),
        )
        .await;
    assert_eq!(
        updated["project"],
        project.as_str(),
        "project name is stable"
    );
    assert!(updated["generated_yaml"]
        .as_str()
        .unwrap()
        .contains("busybox:1.37"));

    // A chosen project name takes over the containers and volumes already named that way.
    let adopted = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &stacks_path,
            Some(json!({
                "name": "adopted", "server_id": server_id, "source": "inline",
                "compose_yaml": COMPOSE, "main_service": "web", "project": "lele-center"
            })),
        )
        .await;
    assert_eq!(adopted["project"], "lele-center");
    assert!(adopted["generated_yaml"]
        .as_str()
        .unwrap()
        .contains("name: lele-center"));
    let adopted_id = adopted["service"]["id"].as_str().unwrap().to_owned();
    for (name, project, want, code) in [
        ("twin", "lele-center", StatusCode::CONFLICT, "project_taken"),
        ("loud", "Not_Valid", StatusCode::BAD_REQUEST, "invalid_name"),
    ] {
        let refused = client
            .expect(
                want,
                "POST",
                &stacks_path,
                Some(json!({
                    "name": name, "server_id": server_id, "source": "inline",
                    "compose_yaml": COMPOSE, "project": project
                })),
            )
            .await;
        assert_eq!(refused["error"]["code"], code, "{project}");
    }
    let renamed = rename_project(&client, &adopted_id, "lele-center-2").await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.body);
    assert_eq!(renamed.body["project"], "lele-center-2");
    assert!(renamed.body["generated_yaml"]
        .as_str()
        .unwrap()
        .contains("name: lele-center-2"));
    let clash = rename_project(&client, &adopted_id, &project).await;
    assert_eq!(clash.status, StatusCode::CONFLICT);
    assert_eq!(clash.body["error"]["code"], "project_taken");
    sqlx::query(
        r#"UPDATE services SET spec = spec || '{"compose_path":"stacks/x/rudder-compose.yml"}' WHERE id = $1::uuid"#,
    )
    .bind(&adopted_id)
    .execute(&pool)
    .await
    .unwrap();
    let locked = rename_project(&client, &adopted_id, "lele-center-3").await;
    assert_eq!(locked.status, StatusCode::CONFLICT);
    assert_eq!(locked.body["error"]["code"], "project_locked");

    let app_service = client
        .ok(
            "POST",
            &format!("/api/v1/environments/{environment_id}/services"),
            Some(json!({ "name": "plain", "kind": "application", "server_id": server_id })),
        )
        .await;
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &format!("/api/v1/services/{}/domains", app_service["id"].as_str().unwrap()),
            Some(json!({ "host": "plain.example.com", "target_port": 80, "ssl_mode": "none", "compose_service": "web" })),
        )
        .await;
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "GET",
            &format!(
                "/api/v1/services/{}/stack",
                app_service["id"].as_str().unwrap()
            ),
            None,
        )
        .await;

    let git = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &stacks_path,
            Some(
                json!({ "name": "from-git", "server_id": server_id, "source": "git", "git": {
                    "provider_id": uuid::Uuid::nil(), "repo": "https://github.com/acme/app",
                    "branch": "main", "compose_file": "deploy/compose.yml"
                }}),
            ),
        )
        .await;
    assert_eq!(
        git["generated_yaml"], "",
        "git stacks are planned after the first fetch"
    );
    let git_up = client
        .expect(
            StatusCode::CONFLICT,
            "POST",
            &format!(
                "/api/v1/services/{}/stack/up",
                git["service"]["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(
        git_up["error"]["code"], "remote_unavailable",
        "a git stack is allowed on a remote server, and waits for its agent"
    );
    let refresh_inline = client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &format!("/api/v1/services/{stack_id}/stack/refresh"),
            None,
        )
        .await;
    assert_eq!(refresh_inline["error"]["code"], "invalid");
    let refresh_git = client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &format!(
                "/api/v1/services/{}/stack/refresh",
                git["service"]["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(refresh_git["error"]["code"], "git_refresh_failed");
    assert!(refresh_git["error"]["message"]
        .as_str()
        .unwrap()
        .contains("git provider not found"));

    let deploy_path = format!("/api/v1/environments/{environment_id}/templates/minio/deploy");
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &deploy_path,
            Some(json!({ "name": "s3", "server_id": server_id, "values": {}, "start": false })),
        )
        .await;
    client
        .expect(
            StatusCode::BAD_REQUEST,
            "POST",
            &deploy_path,
            Some(
                json!({ "name": "s3", "server_id": server_id, "start": false,
                "values": { "DOMAIN": "s3.example.com", "SURPRISE": "1" } }),
            ),
        )
        .await;
    let deployed = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &deploy_path,
            Some(
                json!({ "name": "s3", "server_id": server_id, "start": false,
                "values": { "DOMAIN": "S3.Example.com" } }),
            ),
        )
        .await;
    assert_eq!(deployed["domain"], "s3.example.com");
    assert!(deployed["deployment"].is_null());
    let generated = deployed["generated"].as_array().unwrap();
    assert_eq!(generated.len(), 1);
    assert_eq!(generated[0]["key"], "MINIO_ROOT_PASSWORD");
    let password = generated[0]["value"].as_str().unwrap().to_owned();
    assert_eq!(password.len(), 40);
    let s3_id = deployed["service"]["id"].as_str().unwrap().to_owned();
    assert_eq!(deployed["service"]["spec"]["template"], "minio");
    let env = client
        .ok("GET", &format!("/api/v1/services/{s3_id}/env"), None)
        .await;
    let keys: Vec<&str> = env
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["DOMAIN", "MINIO_ROOT_PASSWORD", "MINIO_ROOT_USER"]);
    let template_domains = client
        .ok("GET", &format!("/api/v1/services/{s3_id}/domains"), None)
        .await;
    assert_eq!(template_domains[0]["compose_service"], "minio");
    assert_eq!(template_domains[0]["target_port"], 9000);
    for path in [
        format!("/api/v1/environments/{environment_id}/services"),
        format!("/api/v1/services/{s3_id}/stack"),
        format!("/api/v1/services/{s3_id}/env"),
        format!("/api/v1/services/{stack_id}/stack"),
    ] {
        let reply = client.call("GET", &path, None).await;
        assert!(
            !reply.raw.contains(&password),
            "{path} leaked a generated secret"
        );
        assert!(
            !reply.raw.contains("tok-very-secret"),
            "{path} leaked an env value"
        );
    }
    let taken = client
        .expect(
            StatusCode::CONFLICT,
            "POST",
            &deploy_path,
            Some(
                json!({ "name": "s3-two", "server_id": server_id, "start": false,
                "values": { "DOMAIN": "s3.example.com" } }),
            ),
        )
        .await;
    assert_eq!(taken["error"]["code"], "domain_taken");
    let services = client
        .ok(
            "GET",
            &format!("/api/v1/environments/{environment_id}/services"),
            None,
        )
        .await;
    assert!(
        !services
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "s3-two"),
        "failed template deploys roll back"
    );

    client
        .expect(
            StatusCode::NO_CONTENT,
            "DELETE",
            &format!("/api/v1/services/{s3_id}/stack"),
            None,
        )
        .await;
    client
        .expect(
            StatusCode::NOT_FOUND,
            "GET",
            &format!("/api/v1/services/{s3_id}/stack"),
            None,
        )
        .await;

    let openapi = client.ok("GET", "/api/openapi.json", None).await;
    for path in [
        "/api/v1/environments/{id}/stacks",
        "/api/v1/services/{id}/stack",
        "/api/v1/services/{id}/stack/{action}",
        "/api/v1/services/{id}/stack/logs",
        "/api/v1/templates",
        "/api/v1/environments/{id}/templates/{slug}/deploy",
    ] {
        assert!(
            openapi["paths"].get(path).is_some(),
            "{path} missing from OpenAPI"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn templates_render_valid_compose(pool: PgPool) {
    if std::env::var("RUDDER_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set RUDDER_DOCKER_TESTS=1 to check templates with docker compose");
        return;
    }
    let data_dir = temp_dir("templates");
    let client = Client::owner(app(pool, &data_dir)).await;
    let templates = client.ok("GET", "/api/v1/templates", None).await;
    for summary in templates.as_array().unwrap() {
        let slug = summary["slug"].as_str().unwrap();
        let detail = client
            .ok("GET", &format!("/api/v1/templates/{slug}"), None)
            .await;
        let mut env = Vec::new();
        for variable in detail["variables"].as_array().unwrap() {
            let value = match (
                variable["kind"].as_str().unwrap(),
                variable["default"].as_str(),
            ) {
                (_, Some(default)) => default.to_owned(),
                ("domain", None) => format!("{slug}.example.com"),
                ("email", None) => "admin@example.com".to_owned(),
                ("number", None) => "1".to_owned(),
                ("boolean", None) => "true".to_owned(),
                _ => "generated-value-123".to_owned(),
            };
            env.push((variable["name"].as_str().unwrap().to_owned(), value));
        }
        let plan = rudder_core::transform_compose(
            slug,
            detail["compose"].as_str().unwrap(),
            &[detail["main_service"].as_str().unwrap()],
            true,
        )
        .unwrap();
        let dir = data_dir.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rudder-compose.yml"), &plan.yaml).unwrap();
        std::fs::write(
            dir.join(".env"),
            rudder_core::render_env_file(&env).unwrap(),
        )
        .unwrap();
        let output = Command::new("docker")
            .args(["compose", "-p", slug, "-f"])
            .arg(dir.join("rudder-compose.yml"))
            .args(["config", "-q"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{slug}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker CLI");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[sqlx::test(migrations = "./migrations")]
async fn docker_stack_lifecycle(pool: PgPool) {
    if std::env::var("RUDDER_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set RUDDER_DOCKER_TESTS=1 to run Docker stack tests");
        return;
    }
    let _ = Command::new("docker")
        .args(["network", "create", "rudder-proxy"])
        .output();
    let data_dir = temp_dir("stacks-docker");
    let client = Client::owner(app(pool.clone(), &data_dir)).await;
    let (server_id, environment_id) = client.environment("local").await;
    let created = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &format!("/api/v1/environments/{environment_id}/stacks"),
            Some(json!({
                "name": "lifecycle", "server_id": server_id, "source": "inline",
                "compose_yaml": COMPOSE, "main_service": "web"
            })),
        )
        .await;
    let id = created["service"]["id"].as_str().unwrap().to_owned();
    let project = created["project"].as_str().unwrap().to_owned();
    let greeting = r#"it's $HOME and "quotes" \ back"#;
    for (key, value) in [("GREETING", greeting), ("TOKEN", "tok-'$x'-value")] {
        client
            .ok(
                "PUT",
                &format!("/api/v1/services/{id}/env"),
                Some(json!({ "key": key, "value": value, "is_build_secret": false })),
            )
            .await;
    }

    let up = client
        .expect(
            StatusCode::ACCEPTED,
            "POST",
            &format!("/api/v1/services/{id}/stack/up"),
            None,
        )
        .await;
    client
        .expect(
            StatusCode::CONFLICT,
            "POST",
            &format!("/api/v1/services/{id}/stack/restart"),
            None,
        )
        .await;
    let finished = client
        .wait_deployment(&id, up["id"].as_str().unwrap())
        .await;
    assert_eq!(finished["status"], "succeeded", "{}", finished["logs"]);
    assert_eq!(finished["trigger"], "manual:up");
    let web = format!("{project}-web-1");
    let worker = format!("{project}-worker-1");
    let env = docker(&[
        "inspect",
        "-f",
        "{{range .Config.Env}}{{println .}}{{end}}",
        &web,
    ]);
    assert!(
        env.lines()
            .any(|line| line == format!("GREETING={greeting}")),
        "{env}"
    );
    let networks = docker(&[
        "inspect",
        "-f",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}",
        &web,
    ]);
    assert!(networks.contains("rudder-proxy"), "{networks}");
    let worker_networks = docker(&[
        "inspect",
        "-f",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}",
        &worker,
    ]);
    assert!(
        !worker_networks.contains("rudder-proxy"),
        "{worker_networks}"
    );
    assert_eq!(
        docker(&[
            "inspect",
            "-f",
            "{{index .Config.Labels \"rudder.project\"}}",
            &web
        ]),
        project
    );
    let compose_file = data_dir.join(format!("stacks/{project}/rudder-compose.yml"));
    assert!(compose_file.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(data_dir.join(format!("stacks/{project}/.env")))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let view = client
        .ok("GET", &format!("/api/v1/services/{id}/stack"), None)
        .await;
    assert_eq!(view["service"]["status"], "running");
    assert!(
        view["services"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["running"] == true),
        "{view}"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let logs = client
            .ok(
                "GET",
                &format!("/api/v1/services/{id}/stack/logs?service=worker&tail=50"),
                None,
            )
            .await;
        let text = logs["logs"].as_str().unwrap();
        if text.contains("ready") {
            assert!(text.contains("token=***"), "{text}");
            assert!(!text.contains("tok-'$x'-value"));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "worker logs never appeared"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    client
        .expect(
            StatusCode::CREATED,
            "POST",
            &format!("/api/v1/services/{id}/domains"),
            Some(json!({ "host": "worker.example.com", "target_port": 8080, "ssl_mode": "none", "compose_service": "worker" })),
        )
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let latest = client.latest_deployment(&id).await;
        if latest["trigger"] == "domain:up" && latest["status"] == "succeeded" {
            break;
        }
        assert_ne!(latest["status"], "failed", "{}", latest["logs"]);
        assert!(
            tokio::time::Instant::now() < deadline,
            "domain refresh never ran"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let worker_networks = docker(&[
        "inspect",
        "-f",
        "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}",
        &worker,
    ]);
    assert!(
        worker_networks.contains("rudder-proxy"),
        "{worker_networks}"
    );
    let routes = std::fs::read_to_string(data_dir.join("traefik/dynamic/routes.yml")).unwrap();
    assert!(
        routes.contains(&format!("http://{worker}:8080")),
        "{routes}"
    );

    for action in ["restart", "pull", "down"] {
        let started = client
            .expect(
                StatusCode::ACCEPTED,
                "POST",
                &format!("/api/v1/services/{id}/stack/{action}"),
                None,
            )
            .await;
        let done = client
            .wait_deployment(&id, started["id"].as_str().unwrap())
            .await;
        assert_eq!(done["status"], "succeeded", "{action}: {}", done["logs"]);
    }
    assert!(docker(&[
        "ps",
        "-aq",
        "--filter",
        &format!("label=com.docker.compose.project={project}")
    ])
    .is_empty());
    let view = client
        .ok("GET", &format!("/api/v1/services/{id}/stack"), None)
        .await;
    assert_eq!(view["service"]["status"], "stopped");
    client
        .expect(
            StatusCode::NO_CONTENT,
            "DELETE",
            &format!("/api/v1/services/{id}/stack"),
            None,
        )
        .await;

    let deployed = client
        .expect(
            StatusCode::CREATED,
            "POST",
            &format!("/api/v1/environments/{environment_id}/templates/uptime-kuma/deploy"),
            Some(json!({ "name": "kuma", "server_id": server_id, "values": { "DOMAIN": "status.example.com" } })),
        )
        .await;
    let kuma = deployed["service"]["id"].as_str().unwrap().to_owned();
    let deployment = deployed["deployment"]["id"].as_str().unwrap().to_owned();
    let done = client.wait_deployment(&kuma, &deployment).await;
    assert_eq!(done["status"], "succeeded", "{}", done["logs"]);
    let kuma_project = deployed["service"]["spec"]["project"]
        .as_str()
        .unwrap()
        .to_owned();
    let container = docker(&[
        "ps",
        "-q",
        "--filter",
        &format!("label=com.docker.compose.project={kuma_project}"),
    ]);
    assert!(!container.is_empty(), "uptime kuma is running");
    let routes = std::fs::read_to_string(data_dir.join("traefik/dynamic/routes.yml")).unwrap();
    assert!(
        routes.contains(&format!("http://{kuma_project}-uptime-kuma-1:3001")),
        "{routes}"
    );
    let volumes = docker(&[
        "volume",
        "ls",
        "-q",
        "--filter",
        &format!("label=com.docker.compose.project={kuma_project}"),
    ]);
    assert!(!volumes.is_empty());
    client
        .expect(
            StatusCode::NO_CONTENT,
            "DELETE",
            &format!("/api/v1/services/{kuma}/stack?volumes=true"),
            None,
        )
        .await;
    let volumes = docker(&[
        "volume",
        "ls",
        "-q",
        "--filter",
        &format!("label=com.docker.compose.project={kuma_project}"),
    ]);
    assert!(volumes.is_empty(), "named volumes removed: {volumes}");
}
