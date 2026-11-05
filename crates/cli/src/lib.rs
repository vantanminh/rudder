//! Rudder command line client.
//!
//! API commands authenticate with a scoped API token (`Authorization: Bearer`).
//! `rudder login` supports three sign-in methods:
//! 1. API key (`RUDDER_TOKEN` or stdin)
//! 2. Browser callback (`rudder login --url <url> --browser`)
//! 3. 8-character alphanumeric Device Code (`rudder login --url <url> --device`)

pub mod config;
pub mod decrypt;
pub mod image;
pub mod project;
pub mod update;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

pub const HELP: &str = "\
Usage:
  rudder login --url <panel URL> [--browser | --device]   sign in with API token, browser, or 8-char device code
  rudder logout                                           forget the saved token
  rudder init [<service>] [--file rudder.toml]            create a rudder.toml project config
  rudder build [<service>] [--tag TAG] [--skip-checks]    run local CI checks, build Docker image & save locally
  rudder push [TAG] [--service S] [--deploy]              upload a local image archive to Cloudflare R2
  rudder deploy [<service>] [--image TAG] [--build]       deploy from R2 (or check+build+push+deploy with --build)
  rudder images [<service>] [list | prune --keep N | rollback TAG]
  rudder services                                         list services
  rudder logs <service> [-f] [-n LINES] [--since 15m|RFC3339] [--level L] [--source S] [--grep TEXT]
  rudder cache purge <service> (--all | --path P... | --prefix P) [--domain ID]
  rudder backup decrypt [--in FILE] [--out FILE]          (passphrase from RUDDER_BACKUP_PASSPHRASE)
  rudder update [--check] [--repo OWNER/REPO]             update rudder CLI natively from GitHub releases
  rudder --version

<service> is a service id, its name, project/name, or project/environment/name.
Create or revoke tokens under Account > API tokens; deploy, push, and purge need the write scope.";

/// Where the CLI reads its settings from; `main` fills it from the environment.
pub struct Context {
    pub config_path: PathBuf,
    pub url: Option<String>,
    pub token: Option<String>,
    /// Reads a token when `RUDDER_TOKEN` is not set (stdin in the binary).
    pub read_token: Box<dyn Fn() -> std::io::Result<String> + Send + Sync>,
    /// Poll interval for `deploy`, `login --device`, and `logs -f`.
    pub poll: Duration,
}

impl Context {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Self {
            config_path: config::default_path(),
            url: var("RUDDER_URL"),
            token: var("RUDDER_TOKEN"),
            read_token: Box::new(|| {
                use std::io::{BufRead, IsTerminal};
                if std::io::stdin().is_terminal() {
                    eprint!("API token: ");
                }
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                Ok(line.trim().to_owned())
            }),
            poll: Duration::from_secs(2),
        }
    }
}

pub async fn run(
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut args = args.into_iter();
    match args.next().as_deref() {
        None | Some("--help" | "-h" | "help") => {
            writeln!(out, "rudder {}\n\n{HELP}", rudder_core::VERSION)?;
            Ok(())
        }
        Some("--version" | "-V") => {
            writeln!(out, "rudder {}", rudder_core::VERSION)?;
            Ok(())
        }
        Some("backup") => match args.next().as_deref() {
            Some("decrypt") => decrypt::run(decrypt::parse(args)?),
            _ => bail!("usage: rudder backup decrypt [--in FILE] [--out FILE]"),
        },
        Some("login") => login(args.collect(), ctx, out).await,
        Some("logout") => {
            match std::fs::remove_file(&ctx.config_path) {
                Ok(()) => writeln!(out, "Removed {}", ctx.config_path.display())?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    writeln!(out, "Not logged in")?
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        }
        Some("init") => init_project(args.collect(), out),
        Some("build") => build_cmd(args.collect(), ctx, out),
        Some("push") => push_cmd(&Api::from_context(ctx)?, args.collect(), ctx, out).await,
        Some("images") => images_cmd(&Api::from_context(ctx)?, args.collect(), ctx, out).await,
        Some("update" | "self-update") => update::run(args.collect(), out).await,
        Some("services") => services(&Api::from_context(ctx)?, out).await,
        Some("deploy") => deploy(&Api::from_context(ctx)?, args.collect(), ctx, out).await,
        Some("logs") => logs(&Api::from_context(ctx)?, args.collect(), ctx, out).await,
        Some("cache") => match args.next().as_deref() {
            Some("purge") => purge(&Api::from_context(ctx)?, args.collect(), out).await,
            _ => bail!("usage: rudder cache purge <service> (--all | --path P... | --prefix P)"),
        },
        Some(other) => bail!("unknown command '{other}'; see rudder --help"),
    }
}

struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Api {
    fn new(base: &str, token: &str) -> anyhow::Result<Self> {
        if !token.starts_with("rdr_") {
            bail!("that is not a Rudder API token (they start with rdr_)");
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(format!("rudder-cli/{}", rudder_core::VERSION))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(900))
                .build()?,
            base: config::normalize_url(base)?,
            token: token.to_owned(),
        })
    }

    fn from_context(ctx: &Context) -> anyhow::Result<Self> {
        let saved = config::load(&ctx.config_path)?;
        let url = ctx
            .url
            .clone()
            .or_else(|| saved.as_ref().map(|s| s.url.clone()))
            .ok_or_else(|| anyhow!("not logged in; run `rudder login --url <panel URL>`"))?;
        let token = ctx
            .token
            .clone()
            .or_else(|| saved.map(|s| s.token))
            .ok_or_else(|| anyhow!("no API token; run `rudder login` or set RUDDER_TOKEN"))?;
        Self::new(&url, &token)
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<url::Url> {
        let mut url = url::Url::parse(&format!("{}{path}", self.base))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let url = self.url(path, query)?;
        let mut request = self
            .http
            .request(method.clone(), url)
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("{method} {}{path}", self.base))?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(value);
        }
        let message = value["error"]["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_owned());
        match status.as_u16() {
            401 => bail!("the panel rejected the token: {message}"),
            403 => bail!("not allowed: {message}"),
            _ => bail!("{message} ({status})"),
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
        self.call(reqwest::Method::GET, path, query, None).await
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(reqwest::Method::POST, path, &[], Some(body))
            .await
    }

    async fn post_stream(
        &self,
        path: &str,
        query: &[(&str, String)],
        file_path: &Path,
    ) -> anyhow::Result<Value> {
        let url = self.url(path, query)?;
        let file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("open {}", file_path.display()))?;
        let stream = ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {}{path}", self.base))?;
        let status = response.status();
        let bytes = response.bytes().await?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(value);
        }
        let message = value["error"]["message"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_owned());
        match status.as_u16() {
            401 => bail!("the panel rejected the token: {message}"),
            403 => bail!("not allowed: {message}"),
            _ => bail!("{message} ({status})"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoginMode {
    ApiKey,
    Browser,
    DeviceCode,
}

async fn login(
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut url = ctx.url.clone();
    let mut mode = LoginMode::ApiKey;
    let mut client_name = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => url = Some(args.next().context("--url needs a value")?),
            "--browser" => mode = LoginMode::Browser,
            "--device" => mode = LoginMode::DeviceCode,
            "--name" => client_name = Some(args.next().context("--name needs a value")?),
            "--token" => bail!(
                "pass the token in RUDDER_TOKEN or on stdin, not as an argument, so it stays out of shell history and ps"
            ),
            other => bail!("unknown option {other}"),
        }
    }
    let raw_url = url
        .or_else(|| config::load(&ctx.config_path).ok().flatten().map(|s| s.url))
        .context("usage: rudder login --url <panel URL> [--browser | --device]")?;
    let base = config::normalize_url(&raw_url)?;

    let token = match mode {
        LoginMode::ApiKey => match &ctx.token {
            Some(token) => token.clone(),
            None => (ctx.read_token)()?,
        },
        LoginMode::Browser => login_via_browser(&base, client_name.as_deref(), ctx, out).await?,
        LoginMode::DeviceCode => login_via_device(&base, client_name.as_deref(), ctx, out).await?,
    };

    let api = Api::new(&base, token.trim())?;
    let me = api.get("/api/v1/me", &[]).await?;
    config::save(
        &ctx.config_path,
        &config::Saved {
            url: api.base.clone(),
            token: api.token.clone(),
        },
    )?;
    writeln!(
        out,
        "Logged in to {} as {} ({}). Saved to {}",
        api.base,
        me["user"]["email"].as_str().unwrap_or("?"),
        me["user"]["role"].as_str().unwrap_or("?"),
        ctx.config_path.display()
    )?;
    Ok(())
}

async fn login_via_device(
    base: &str,
    client_name: Option<&str>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<String> {
    let http = reqwest::Client::builder()
        .user_agent(format!("rudder-cli/{}", rudder_core::VERSION))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let start_resp = http
        .post(format!("{base}/api/v1/cli/auth/start"))
        .json(&json!({
            "method": "device_code",
            "client_name": client_name.unwrap_or("rudder-cli"),
        }))
        .send()
        .await
        .context("start device-code login")?;
    if !start_resp.status().is_success() {
        bail!(
            "could not start device-code login ({})",
            start_resp.status()
        );
    }
    let started: Value = start_resp.json().await?;
    let request_id = started["request_id"]
        .as_str()
        .context("missing request_id")?
        .to_owned();
    let poll_secret = started["poll_secret"]
        .as_str()
        .context("missing poll_secret")?
        .to_owned();
    let device_code = started["device_code"]
        .as_str()
        .context("missing device_code")?
        .to_owned();
    let verification_path = started["verification_path"]
        .as_str()
        .unwrap_or("/tokens?device=1");
    let verify_url = format!("{base}{verification_path}");

    writeln!(
        out,
        "Open this URL in your browser to authorize rudder CLI:\n  {verify_url}\n\nEnter 8-character device code: {device_code}\nWaiting for approval..."
    )?;
    out.flush()?;

    poll_for_cli_token(&http, base, &request_id, &poll_secret, ctx.poll).await
}

async fn login_via_browser(
    base: &str,
    client_name: Option<&str>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind local callback listener")?;
    let port = listener.local_addr()?.port();
    let callback_url = format!("http://127.0.0.1:{port}/callback");

    let http = reqwest::Client::builder()
        .user_agent(format!("rudder-cli/{}", rudder_core::VERSION))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?;
    let start_resp = http
        .post(format!("{base}/api/v1/cli/auth/start"))
        .json(&json!({
            "method": "browser",
            "client_name": client_name.unwrap_or("rudder-cli"),
            "callback_url": callback_url,
        }))
        .send()
        .await
        .context("start browser login")?;
    if !start_resp.status().is_success() {
        bail!("could not start browser login ({})", start_resp.status());
    }
    let started: Value = start_resp.json().await?;
    let request_id = started["request_id"]
        .as_str()
        .context("missing request_id")?
        .to_owned();
    let poll_secret = started["poll_secret"]
        .as_str()
        .context("missing poll_secret")?
        .to_owned();
    let verification_path = started["verification_path"].as_str().unwrap_or("/tokens");
    let authorize_url = format!("{base}{verification_path}");

    writeln!(
        out,
        "Open this URL in your browser to authorize rudder CLI:\n  {authorize_url}\nCallback listener: {callback_url}"
    )?;
    out.flush()?;

    if std::env::var("RUDDER_NO_BROWSER").as_deref() != Ok("1") {
        let _ = open_browser(&authorize_url);
    }

    let callback_fut = accept_browser_callback(listener);
    let poll_fut = poll_for_cli_token(&http, base, &request_id, &poll_secret, ctx.poll);

    tokio::select! {
        res = callback_fut => {
            if let Ok(token) = res {
                return Ok(token);
            }
            poll_for_cli_token(&http, base, &request_id, &poll_secret, ctx.poll).await
        }
        res = poll_fut => res,
    }
}

async fn accept_browser_callback(listener: tokio::net::TcpListener) -> anyhow::Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await?;
        let req_text = String::from_utf8_lossy(&buf[..n]);
        let first_line = req_text.lines().next().unwrap_or_default();
        let path = first_line.split_whitespace().nth(1).unwrap_or("/");
        if let Ok(parsed) = url::Url::parse(&format!("http://127.0.0.1{path}")) {
            if let Some((_, token)) = parsed.query_pairs().find(|(k, _)| k == "token") {
                let body = "<html><body style=\"font-family:sans-serif;padding:2rem\"><h2>Rudder CLI authorized</h2><p>You can close this window and return to your terminal.</p></body></html>";
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
                return Ok(token.into_owned());
            }
        }
        let reply = "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(reply.as_bytes()).await;
    }
}

async fn poll_for_cli_token(
    http: &reqwest::Client,
    base: &str,
    request_id: &str,
    poll_secret: &str,
    interval: Duration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        tokio::time::sleep(interval).await;
        let resp = http
            .post(format!("{base}/api/v1/cli/auth/poll"))
            .json(&json!({
                "request_id": request_id,
                "poll_secret": poll_secret,
            }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("CLI login poll failed ({})", resp.status());
        }
        let body: Value = resp.json().await?;
        match body["status"].as_str().unwrap_or("pending") {
            "approved" => {
                let token = body["token"]
                    .as_str()
                    .context("approved response missing token")?;
                return Ok(token.to_owned());
            }
            "expired" => bail!("CLI login request expired before it was approved"),
            "consumed" => bail!("CLI login token was already consumed"),
            _ => {}
        }
        if tokio::time::Instant::now() > deadline {
            bail!("timed out waiting for CLI login approval");
        }
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

fn init_project(args: Vec<String>, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut service = None;
    let mut file = PathBuf::from(project::DEFAULT_FILE);
    let mut force = false;
    let mut r2_keep = Some(10u32);
    let mut local_keep = Some(20u32);
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--file" | "-f" => {
                file = PathBuf::from(iter.next().context("--file needs a path")?);
            }
            "--force" => force = true,
            "--service" | "-s" => service = Some(iter.next().context("--service needs a name")?),
            "--r2-keep" => {
                r2_keep = Some(iter.next().context("--r2-keep needs a count")?.parse()?);
            }
            "--local-keep" => {
                local_keep = Some(iter.next().context("--local-keep needs a count")?.parse()?);
            }
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other if service.is_none() => service = Some(other.to_owned()),
            other => bail!("unexpected argument {other}"),
        }
    }
    if file.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            file.display()
        );
    }
    let cfg = project::ProjectConfig {
        service: service.unwrap_or_else(|| "my-project/prod/web".into()),
        dockerfile: "Dockerfile".into(),
        context: ".".into(),
        checks: Vec::new(),
        r2_keep_count: r2_keep,
        local_keep_count: local_keep,
        deploy_build: false,
    };
    std::fs::write(&file, cfg.render_toml())
        .with_context(|| format!("write {}", file.display()))?;
    writeln!(
        out,
        "Created {} for service '{}'",
        file.display(),
        cfg.service
    )?;
    Ok(())
}

fn build_cmd(args: Vec<String>, ctx: &Context, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut service = None;
    let mut tag = None;
    let mut dockerfile = None;
    let mut context_dir = None;
    let mut checks = Vec::new();
    let mut skip_checks = false;
    let mut local_keep = None;
    let mut file_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--service" | "-s" => service = Some(iter.next().context("--service needs a value")?),
            "--tag" | "-t" => tag = Some(iter.next().context("--tag needs a value")?),
            "--dockerfile" => dockerfile = Some(iter.next().context("--dockerfile needs a path")?),
            "--context" => context_dir = Some(iter.next().context("--context needs a path")?),
            "--check" => checks.push(iter.next().context("--check needs a command")?),
            "--skip-checks" | "--no-checks" | "--no-check" => skip_checks = true,
            "--local-keep" => {
                local_keep = Some(
                    iter.next()
                        .context("--local-keep needs a count")?
                        .parse()
                        .context("--local-keep needs a number")?,
                )
            }
            "--file" | "-f" => {
                file_path = Some(PathBuf::from(iter.next().context("--file needs a path")?))
            }
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other if service.is_none() => service = Some(other.to_owned()),
            other => bail!("unexpected argument {other}"),
        }
    }

    let loaded = project::load_optional(file_path.as_deref())?;
    let cfg = loaded.map(|(_, c)| c).unwrap_or_default();
    let service = service
        .or_else(|| (!cfg.service.is_empty()).then_some(cfg.service.clone()))
        .context("specify a <service> or set `service = \"...\"` in rudder.toml")?;
    let dockerfile = dockerfile.unwrap_or_else(|| cfg.dockerfile.clone());
    let context_path = PathBuf::from(context_dir.unwrap_or_else(|| cfg.context.clone()));
    let effective_checks = if checks.is_empty() {
        cfg.checks
    } else {
        checks
    };
    let effective_keep = local_keep.or(cfg.local_keep_count);
    let git_sha = image::detect_git_sha(&context_path);
    let tag = tag.unwrap_or_else(|| image::default_tag(git_sha.as_deref()));

    let images_root = config::images_dir(&ctx.config_path);
    let _ = image::build_and_store_local(
        &image::BuildParams {
            images_root: &images_root,
            service: &service,
            tag: &tag,
            dockerfile: &dockerfile,
            context_dir: &context_path,
            checks: &effective_checks,
            skip_checks,
            local_keep_count: effective_keep,
        },
        out,
    )?;
    Ok(())
}

async fn push_cmd(
    api: &Api,
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut tag = None;
    let mut service_spec = None;
    let mut deploy_now = None;
    let mut r2_keep = None;
    let mut file_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--service" | "-s" => {
                service_spec = Some(iter.next().context("--service needs a value")?)
            }
            "--tag" | "-t" => tag = Some(iter.next().context("--tag needs a value")?),
            "--deploy" => deploy_now = Some(true),
            "--no-deploy" => deploy_now = Some(false),
            "--r2-keep" => {
                r2_keep = Some(
                    iter.next()
                        .context("--r2-keep needs a count")?
                        .parse::<u32>()
                        .context("--r2-keep needs a positive number")?,
                )
            }
            "--file" | "-f" => {
                file_path = Some(PathBuf::from(iter.next().context("--file needs a path")?))
            }
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other if tag.is_none() => tag = Some(other.to_owned()),
            other => bail!("unexpected argument {other}"),
        }
    }

    let loaded = project::load_optional(file_path.as_deref())?;
    let cfg = loaded.map(|(_, c)| c).unwrap_or_default();
    let service_spec = service_spec
        .or_else(|| (!cfg.service.is_empty()).then_some(cfg.service.clone()))
        .context("specify --service <service> or set `service = \"...\"` in rudder.toml")?;
    let r2_keep = r2_keep.or(cfg.r2_keep_count);

    let service = resolve(api, &service_spec).await?;
    let images_root = config::images_dir(&ctx.config_path);
    let mut local_records = image::load_local_images(&images_root, &service_spec)?;
    if local_records.is_empty() {
        local_records = image::load_local_images(&images_root, &service.path())?;
    }
    if local_records.is_empty() {
        local_records = image::load_local_images(&images_root, &service.name)?;
    }
    if local_records.is_empty() {
        bail!(
            "no local built images found for '{}'; run `rudder build` first",
            service.path()
        );
    }

    let record = match tag.as_deref() {
        Some(requested) => local_records
            .into_iter()
            .find(|r| r.tag == requested)
            .ok_or_else(|| {
                anyhow!(
                    "local image tag '{requested}' not found for {}; see `rudder images {}`",
                    service.path(),
                    service.path()
                )
            })?,
        None => local_records.into_iter().next().unwrap(),
    };

    let archive = image::archive_path(&images_root, &record.service, &record);
    if !archive.exists() {
        bail!("local archive file is missing: {}", archive.display());
    }

    writeln!(
        out,
        "==> Pushing {} (tag {}, {} bytes) to Cloudflare R2 via {}...",
        record.image_ref, record.tag, record.size_bytes, api.base
    )?;

    let mut query = vec![
        ("tag", record.tag.clone()),
        ("image_ref", record.image_ref.clone()),
        ("compression", record.compression.clone()),
        ("expected_sha256", record.sha256.clone()),
    ];
    if let Some(sha) = &record.git_sha {
        query.push(("git_sha", sha.clone()));
    }
    if let Some(d) = deploy_now {
        query.push(("deploy", d.to_string()));
    }
    if let Some(keep) = r2_keep {
        query.push(("r2_keep_count", keep.to_string()));
    }

    let resp = api
        .post_stream(
            &format!("/api/v1/services/{}/images", service.id),
            &query,
            &archive,
        )
        .await?;

    writeln!(
        out,
        "Pushed {} (tag {}) to Cloudflare R2 [sha256: {}]",
        service.path(),
        record.tag,
        &record.sha256[..12]
    )?;
    if let Some(pruned) = resp["pruned_tags"].as_array() {
        let tags: Vec<&str> = pruned.iter().filter_map(|v| v.as_str()).collect();
        if !tags.is_empty() {
            writeln!(
                out,
                "Pruned old versions from R2 (still kept locally): {}",
                tags.join(", ")
            )?;
        }
    }
    if resp["deployed"].as_bool() == Some(true) {
        if let Some(dep) = resp.get("deployment") {
            report(dep, &mut 0, out)?;
        }
    } else if let Some(err) = resp["deploy_error"].as_str() {
        bail!("image stored in R2, but deployment failed: {err}");
    } else {
        writeln!(
            out,
            "Image {} is stored in R2 (auto-deploy is off; run `rudder deploy {}` when ready).",
            record.tag,
            service.path()
        )?;
    }
    Ok(())
}

async fn images_cmd(
    api: &Api,
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut subcmd = "list".to_owned();
    let mut service_spec = None;
    let mut target_tag = None;
    let mut keep_count: Option<u32> = None;
    let mut file_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "list" | "prune" | "rollback" => subcmd = arg,
            "--service" | "-s" => {
                service_spec = Some(iter.next().context("--service needs a value")?)
            }
            "--keep" => {
                keep_count = Some(
                    iter.next()
                        .context("--keep needs a count")?
                        .parse()
                        .context("--keep needs a positive number")?,
                )
            }
            "--file" | "-f" => {
                file_path = Some(PathBuf::from(iter.next().context("--file needs a path")?))
            }
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other => {
                if subcmd == "rollback" && target_tag.is_none() {
                    target_tag = Some(other.to_owned());
                } else if service_spec.is_none() {
                    service_spec = Some(other.to_owned());
                } else if target_tag.is_none() {
                    target_tag = Some(other.to_owned());
                } else {
                    bail!("unexpected argument {other}");
                }
            }
        }
    }

    let loaded = project::load_optional(file_path.as_deref())?;
    let cfg = loaded.map(|(_, c)| c).unwrap_or_default();
    let service_spec = service_spec
        .or_else(|| (!cfg.service.is_empty()).then_some(cfg.service.clone()))
        .context("specify <service> or set `service = \"...\"` in rudder.toml")?;
    let service = resolve(api, &service_spec).await?;
    let images_root = config::images_dir(&ctx.config_path);

    match subcmd.as_str() {
        "prune" => {
            let keep = keep_count
                .or(cfg.local_keep_count)
                .context("specify --keep <N> or set local_keep_count in rudder.toml")?;
            let mut removed = image::prune_local_store(&images_root, &service_spec, keep)?;
            if removed.is_empty() {
                removed = image::prune_local_store(&images_root, &service.path(), keep)?;
            }
            writeln!(
                out,
                "Pruned {} local image archive(s) for {}",
                removed.len(),
                service.path()
            )?;
            Ok(())
        }
        "rollback" => {
            let tag = target_tag.context("usage: rudder images <service> rollback <tag>")?;
            deploy_or_repush_r2_image(api, &service, &service_spec, Some(&tag), ctx, out).await
        }
        _ => {
            let remote_val = api
                .get(&format!("/api/v1/services/{}/images", service.id), &[])
                .await?;
            let remote_list = remote_val.as_array().cloned().unwrap_or_default();
            let mut local_list = image::load_local_images(&images_root, &service_spec)?;
            if local_list.is_empty() {
                local_list = image::load_local_images(&images_root, &service.path())?;
            }

            let mut seen = BTreeSet::new();
            let mut tags = Vec::new();
            for r in &remote_list {
                if let Some(t) = r["tag"].as_str() {
                    if seen.insert(t.to_owned()) {
                        tags.push(t.to_owned());
                    }
                }
            }
            for l in &local_list {
                if seen.insert(l.tag.clone()) {
                    tags.push(l.tag.clone());
                }
            }

            writeln!(
                out,
                "{:<24}  {:<12}  {:<10}  {:<12}",
                "TAG", "SHA256", "SIZE", "STORAGE"
            )?;
            for t in tags {
                let r_row = remote_list.iter().find(|r| r["tag"].as_str() == Some(&t));
                let l_row = local_list.iter().find(|l| l.tag == t);
                let r2_present = r_row
                    .and_then(|r| r["r2_present"].as_bool())
                    .unwrap_or(false);
                let local_present = l_row.is_some();
                let storage = match (local_present, r2_present) {
                    (true, true) => "local + R2",
                    (true, false) => "local only",
                    (false, true) => "R2 only",
                    (false, false) => "pruned",
                };
                let sha = l_row
                    .map(|l| l.sha256.as_str())
                    .or_else(|| r_row.and_then(|r| r["sha256"].as_str()))
                    .unwrap_or("-");
                let sha_short = sha.get(..12).unwrap_or(sha);
                let size = l_row
                    .map(|l| l.size_bytes)
                    .or_else(|| r_row.and_then(|r| r["size_bytes"].as_u64()))
                    .unwrap_or(0);
                writeln!(
                    out,
                    "{:<24}  {:<12}  {:<10}  {:<12}",
                    t, sha_short, size, storage
                )?;
            }
            Ok(())
        }
    }
}

async fn deploy_or_repush_r2_image(
    api: &Api,
    service: &Service,
    service_spec: &str,
    tag: Option<&str>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let remote_val = api
        .get(&format!("/api/v1/services/{}/images", service.id), &[])
        .await?;
    let remote_list = remote_val.as_array().cloned().unwrap_or_default();

    if let Some(requested_tag) = tag {
        let r_row = remote_list
            .iter()
            .find(|r| r["tag"].as_str() == Some(requested_tag));
        let needs_repush = match r_row {
            Some(row) => row["r2_present"].as_bool() != Some(true),
            None => true,
        };
        if needs_repush {
            let images_root = config::images_dir(&ctx.config_path);
            let local_has = image::load_local_images(&images_root, service_spec)?
                .iter()
                .any(|l| l.tag == requested_tag)
                || image::load_local_images(&images_root, &service.path())?
                    .iter()
                    .any(|l| l.tag == requested_tag);
            if local_has {
                writeln!(
                    out,
                    "Tag '{requested_tag}' is not currently in R2; re-uploading from local store..."
                )?;
                return push_cmd(
                    api,
                    vec![
                        requested_tag.to_owned(),
                        "--service".into(),
                        service_spec.to_owned(),
                        "--deploy".into(),
                    ],
                    ctx,
                    out,
                )
                .await;
            }
        }
    }

    writeln!(
        out,
        "Deploying {} from Cloudflare R2 ({})",
        service.path(),
        tag.unwrap_or("latest")
    )?;
    let done = api
        .post(
            &format!("/api/v1/services/{}/images/deploy", service.id),
            json!({ "tag": tag }),
        )
        .await?;
    report(&done, &mut 0, out)
}

#[derive(Debug, Clone, Deserialize)]
struct Service {
    id: String,
    name: String,
    kind: String,
    status: String,
    project_name: String,
    environment_name: String,
}

impl Service {
    fn path(&self) -> String {
        format!(
            "{}/{}/{}",
            self.project_name, self.environment_name, self.name
        )
    }
}

async fn list_services(api: &Api) -> anyhow::Result<Vec<Service>> {
    Ok(serde_json::from_value(
        api.get("/api/v1/services", &[]).await?,
    )?)
}

async fn services(api: &Api, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let list = list_services(api).await?;
    let width = list
        .iter()
        .map(|s| s.path().len())
        .max()
        .unwrap_or(4)
        .max(4);
    writeln!(
        out,
        "{:<36}  {:<width$}  {:<11}  STATUS",
        "ID", "NAME", "KIND"
    )?;
    for service in list {
        writeln!(
            out,
            "{:<36}  {:<width$}  {:<11}  {}",
            service.id,
            service.path(),
            service.kind,
            service.status
        )?;
    }
    Ok(())
}

/// Find a service by id, name, `project/name`, or `project/environment/name`.
async fn resolve(api: &Api, spec: &str) -> anyhow::Result<Service> {
    let list = list_services(api).await?;
    let parts: Vec<&str> = spec.split('/').collect();
    let matches: Vec<&Service> = list
        .iter()
        .filter(|s| match parts.as_slice() {
            [one] => s.id == *one || s.name == *one,
            [project, name] => s.project_name == *project && s.name == *name,
            [project, env, name] => {
                s.project_name == *project && s.environment_name == *env && s.name == *name
            }
            _ => false,
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no service matches '{spec}'; see `rudder services`"),
        many => bail!(
            "'{spec}' matches {} services ({}); use project/environment/name or the id",
            many.len(),
            many.iter().map(|s| s.path()).collect::<Vec<_>>().join(", ")
        ),
    }
}

async fn deploy(
    api: &Api,
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut spec = None;
    let mut image = None;
    let mut build_flag = None;
    let mut skip_checks = false;
    let mut file_path = None;
    let mut wait = true;
    let mut timeout = Duration::from_secs(1800);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--image" => image = Some(args.next().context("--image needs a value")?),
            "--build" => build_flag = Some(true),
            "--no-build" => build_flag = Some(false),
            "--skip-checks" | "--no-checks" | "--no-check" => skip_checks = true,
            "--file" | "-f" => {
                file_path = Some(PathBuf::from(args.next().context("--file needs a path")?))
            }
            "--no-wait" => wait = false,
            "--timeout" => {
                timeout = Duration::from_secs(
                    args.next()
                        .context("--timeout needs seconds")?
                        .parse()
                        .context("--timeout needs seconds")?,
                )
            }
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other if spec.is_none() => spec = Some(other.to_owned()),
            other => bail!("unexpected argument {other}"),
        }
    }

    let loaded = project::load_optional(file_path.as_deref())?;
    let cfg = loaded.map(|(_, c)| c).unwrap_or_default();
    let spec = spec
        .or_else(|| (!cfg.service.is_empty()).then_some(cfg.service.clone()))
        .context("usage: rudder deploy <service> [--image IMAGE] [--build]")?;
    let should_build = build_flag.unwrap_or(cfg.deploy_build);

    if should_build {
        let context_path = PathBuf::from(&cfg.context);
        let git_sha = image::detect_git_sha(&context_path);
        let tag = image.unwrap_or_else(|| image::default_tag(git_sha.as_deref()));
        let images_root = config::images_dir(&ctx.config_path);
        let (record, _) = image::build_and_store_local(
            &image::BuildParams {
                images_root: &images_root,
                service: &spec,
                tag: &tag,
                dockerfile: &cfg.dockerfile,
                context_dir: &context_path,
                checks: &cfg.checks,
                skip_checks,
                local_keep_count: cfg.local_keep_count,
            },
            out,
        )?;
        let mut push_args = vec![record.tag, "--service".into(), spec, "--deploy".into()];
        if let Some(keep) = cfg.r2_keep_count {
            push_args.push("--r2-keep".into());
            push_args.push(keep.to_string());
        }
        return push_cmd(api, push_args, ctx, out).await;
    }

    let service = resolve(api, &spec).await?;
    let id = &service.id;
    let deployment = match service.kind.as_str() {
        "compose" => {
            if image.is_some() {
                bail!("--image does not apply to compose stacks");
            }
            writeln!(out, "Starting stack {}", service.path())?;
            api.post(&format!("/api/v1/services/{id}/stack/up"), json!({}))
                .await?["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        }
        "application" => {
            // Check if this service has Rudder Images stored in R2 (or if --image matches a tag in R2/local)
            let r2_images = api
                .get(&format!("/api/v1/services/{id}/images"), &[])
                .await
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default();
            let has_r2 = !r2_images.is_empty();
            let image_looks_like_tag = image
                .as_deref()
                .is_some_and(|img| !img.contains('/') && !img.contains(':'));
            if has_r2 && (image.is_none() || image_looks_like_tag) {
                return deploy_or_repush_r2_image(api, &service, &spec, image.as_deref(), ctx, out)
                    .await;
            }

            let detail = api.get(&format!("/api/v1/services/{id}"), &[]).await?;
            let from_git = image.is_none() && detail["spec"]["git"]["repo"].is_string();
            if from_git {
                writeln!(
                    out,
                    "Building {} from {}",
                    service.path(),
                    detail["spec"]["git"]["repo"].as_str().unwrap_or_default()
                )?;
                api.post(&format!("/api/v1/services/{id}/build"), json!({}))
                    .await?["deployment_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            } else {
                writeln!(
                    out,
                    "Deploying {} ({})",
                    service.path(),
                    image
                        .as_deref()
                        .or(detail["spec"]["image"].as_str())
                        .unwrap_or("current image")
                )?;
                let done = api
                    .post(
                        &format!("/api/v1/services/{id}/deploy"),
                        json!({ "image": image }),
                    )
                    .await?;
                return report(&done, &mut 0, out);
            }
        }
        other => bail!(
            "{} is a {other} service; deploy applies to applications and compose stacks",
            service.path()
        ),
    };
    if !wait {
        writeln!(out, "Queued deployment {deployment}")?;
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut printed = 0;
    loop {
        let list = api
            .get(&format!("/api/v1/services/{id}/deployments"), &[])
            .await?;
        if let Some(current) = list
            .as_array()
            .and_then(|items| items.iter().find(|d| d["id"] == deployment.as_str()))
        {
            if current["finished_at"].is_string() {
                return report(current, &mut printed, out);
            }
            print_new_logs(current, &mut printed, out)?;
        }
        if tokio::time::Instant::now() > deadline {
            bail!(
                "deployment {deployment} is still running after {}s",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(ctx.poll).await;
    }
}

fn print_new_logs(
    deployment: &Value,
    printed: &mut usize,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let logs = deployment["logs"].as_str().unwrap_or_default();
    if let Some(new) = logs.get(*printed..) {
        out.write_all(new.as_bytes())?;
        if !new.is_empty() && !new.ends_with('\n') {
            writeln!(out)?;
        }
    }
    *printed = logs.len();
    Ok(())
}

fn report(
    deployment: &Value,
    printed: &mut usize,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    print_new_logs(deployment, printed, out)?;
    let status = deployment["status"].as_str().unwrap_or("unknown");
    if status == "succeeded" {
        writeln!(
            out,
            "Deployment {} succeeded{}",
            deployment["id"].as_str().unwrap_or_default(),
            deployment["image_ref"]
                .as_str()
                .map(|image| format!(" ({image})"))
                .unwrap_or_default()
        )?;
        return Ok(());
    }
    bail!(
        "deployment {} {status}{}",
        deployment["id"].as_str().unwrap_or_default(),
        deployment["error"]
            .as_str()
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    )
}

/// `15m`, `2h`, `7d`, `30s`, or an RFC 3339 time.
fn parse_since(value: &str) -> anyhow::Result<String> {
    if chrono::DateTime::parse_from_rfc3339(value).is_ok() {
        return Ok(value.to_owned());
    }
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let amount: i64 = number.parse().with_context(|| {
        format!("--since '{value}' is not like 15m, 2h, 7d, or an RFC 3339 time")
    })?;
    let seconds = match unit {
        "s" => amount,
        "m" => amount * 60,
        "h" => amount * 3600,
        "d" => amount * 86_400,
        _ => bail!("--since '{value}' is not like 15m, 2h, 7d, or an RFC 3339 time"),
    };
    Ok((chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339())
}

async fn logs(
    api: &Api,
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut spec = None;
    let mut follow = false;
    let mut lines: usize = 100;
    let mut filters: Vec<(&str, String)> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-f" | "--follow" => follow = true,
            "-n" | "--lines" => {
                lines = value("-n")?.parse().context("-n needs a number")?;
                if !(1..=1000).contains(&lines) {
                    bail!("-n must be between 1 and 1000");
                }
            }
            "--since" => filters.push(("since", parse_since(&value("--since")?)?)),
            "--level" => filters.push(("level", value("--level")?)),
            "--source" => filters.push(("source", value("--source")?)),
            "--grep" => filters.push(("q", value("--grep")?)),
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other if spec.is_none() => spec = Some(other.to_owned()),
            other => bail!("unexpected argument {other}"),
        }
    }
    let spec = spec.context("usage: rudder logs <service> [-f] [-n LINES]")?;
    let service = resolve(api, &spec).await?;
    let mut query = filters.clone();
    query.push(("service_id", service.id.clone()));
    query.push(("limit", lines.to_string()));
    let page = api.get("/api/v1/logs", &query).await?;
    let mut entries: Vec<Value> = page["entries"].as_array().cloned().unwrap_or_default();
    entries.reverse();
    let mut last_id = 0;
    let mut last_time = None;
    for entry in &entries {
        print_entry(entry, out)?;
        last_id = last_id.max(entry["id"].as_i64().unwrap_or(0));
        last_time = entry["logged_at"].as_str().map(str::to_owned);
    }
    if !follow {
        return Ok(());
    }
    out.flush()?;
    loop {
        tokio::time::sleep(ctx.poll).await;
        let mut query: Vec<(&str, String)> = filters
            .iter()
            .filter(|(key, _)| *key != "since")
            .cloned()
            .collect();
        query.push(("service_id", service.id.clone()));
        query.push(("limit", "1000".into()));
        if let Some(time) = &last_time {
            query.push(("since", time.clone()));
        }
        let page = api.get("/api/v1/logs", &query).await?;
        let mut fresh: Vec<Value> = page["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry["id"].as_i64().unwrap_or(0) > last_id)
            .collect();
        fresh.reverse();
        for entry in &fresh {
            print_entry(entry, out)?;
            last_id = last_id.max(entry["id"].as_i64().unwrap_or(0));
            last_time = entry["logged_at"].as_str().map(str::to_owned);
        }
        out.flush()?;
    }
}

fn print_entry(entry: &Value, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    writeln!(
        out,
        "{} {:<5} [{}] {}",
        entry["logged_at"].as_str().unwrap_or_default(),
        entry["level"].as_str().unwrap_or_default().to_uppercase(),
        entry["source"].as_str().unwrap_or_default(),
        entry["message"].as_str().unwrap_or_default()
    )?;
    Ok(())
}

async fn purge(api: &Api, args: Vec<String>, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut spec = None;
    let mut all = false;
    let mut paths = Vec::new();
    let mut prefix = None;
    let mut domain = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--all" => all = true,
            "--path" => paths.push(args.next().context("--path needs a value")?),
            "--prefix" => prefix = Some(args.next().context("--prefix needs a value")?),
            "--domain" => domain = Some(args.next().context("--domain needs a domain id")?),
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other if spec.is_none() => spec = Some(other.to_owned()),
            other => bail!("unexpected argument {other}"),
        }
    }
    let spec =
        spec.context("usage: rudder cache purge <service> (--all | --path P... | --prefix P)")?;
    if !all && paths.is_empty() && prefix.is_none() {
        bail!("say what to purge: --all, --path P (repeatable), or --prefix P");
    }
    let service = resolve(api, &spec).await?;
    let result = api
        .post(
            &format!("/api/v1/services/{}/cache/purge", service.id),
            json!({
                "all": all,
                "paths": if paths.is_empty() { None } else { Some(paths) },
                "prefix": prefix,
                "domain_id": domain,
            }),
        )
        .await?;
    writeln!(
        out,
        "Purged {} cached objects for {}",
        result["removed"].as_u64().unwrap_or(0),
        service.path()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_accepts_durations_and_timestamps() {
        assert_eq!(
            parse_since("2026-10-11T10:00:00Z").unwrap(),
            "2026-10-11T10:00:00Z"
        );
        let fifteen = chrono::DateTime::parse_from_rfc3339(&parse_since("15m").unwrap()).unwrap();
        let ago = chrono::Utc::now().signed_duration_since(fifteen);
        assert!((899..=901).contains(&ago.num_seconds()), "{ago:?}");
        assert!(parse_since("15").is_err());
        assert!(parse_since("tomorrow").is_err());
        assert!(parse_since("5w").is_err());
    }

    #[test]
    fn tokens_must_look_like_rudder_tokens() {
        assert!(Api::new("http://127.0.0.1:3000", "rdr_abc").is_ok());
        assert!(Api::new("http://127.0.0.1:3000", "ghp_abc").is_err());
        assert!(Api::new("not a url", "rdr_abc").is_err());
    }

    #[test]
    fn version_constant_matches_the_product() {
        assert_eq!(rudder_core::PRODUCT, "rudder");
        assert!(!rudder_core::VERSION.is_empty());
    }
}
