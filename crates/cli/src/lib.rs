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

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
  rudder build [<service>] [--tag TAG] [--platform P] [--no-cache] [--progress M] [--build-arg K=V] [--skip-checks]
  rudder push [TAG] [--service S] [--deploy]              upload a local image archive to Cloudflare R2
  rudder deploy [<service>] [--image TAG] [--build]       deploy from R2 (or check+build+push+deploy with --build)
  rudder images [<service>] [list | prune --keep N | rollback TAG | remove TAG [--r2-only]]
  rudder status [--check]                                 Rudder Image setup, bucket check, push and build statistics
  rudder whoami                                           show the panel, account, and role this CLI is signed in to
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
            Some("decrypt") => {
                let rest: Vec<String> = args.collect();
                if rest.iter().any(|a| a == "--help" || a == "-h") {
                    writeln!(
                        out,
                        "Usage: rudder backup decrypt [--in FILE] [--out FILE]\n\nDecrypt a client-side encrypted Rudder backup archive.\nSet RUDDER_BACKUP_PASSPHRASE in the environment (or enter it on stdin when --in is used)."
                    )?;
                    return Ok(());
                }
                decrypt::run(decrypt::parse(rest.into_iter())?)
            }
            Some("--help" | "-h") => {
                writeln!(out, "Usage: rudder backup decrypt [--in FILE] [--out FILE]")?;
                Ok(())
            }
            _ => bail!("usage: rudder backup decrypt [--in FILE] [--out FILE]"),
        },
        Some("login") => login(args.collect(), ctx, out).await,
        Some("logout") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                writeln!(
                    out,
                    "Usage: rudder logout\n\nRemove the saved panel URL and API token from {}.",
                    ctx.config_path.display()
                )?;
                return Ok(());
            }
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
        Some("build") => build_cmd(args.collect(), ctx, out).await,
        Some("push") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                print_push_help(out)?;
                return Ok(());
            }
            push_cmd(&Api::from_context(ctx)?, rest, ctx, out).await
        }
        Some("images") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                print_images_help(out)?;
                return Ok(());
            }
            images_cmd(&Api::from_context(ctx)?, rest, ctx, out).await
        }
        Some("update" | "self-update") => update::run(args.collect(), out).await,
        Some("services") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                writeln!(
                    out,
                    "Usage: rudder services\n\nList all services across projects and environments."
                )?;
                return Ok(());
            }
            services(&Api::from_context(ctx)?, out).await
        }
        Some("whoami") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                writeln!(out, "Usage: rudder whoami\n\nShow the panel URL, user email, and role for the active CLI session.")?;
                return Ok(());
            }
            whoami(&Api::from_context(ctx)?, out).await
        }
        Some("status") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                writeln!(out, "Usage: rudder status [--check]\n\nShow Rudder Image store configuration, R2 version counts, push/build statistics, and signed-in clients.\nPass --check to run a live write/read/delete probe against the R2 bucket.")?;
                return Ok(());
            }
            status(&Api::from_context(ctx)?, rest, out).await
        }
        Some("deploy") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                print_deploy_help(out)?;
                return Ok(());
            }
            deploy(&Api::from_context(ctx)?, rest, ctx, out).await
        }
        Some("logs") => {
            let rest: Vec<String> = args.collect();
            if rest.iter().any(|a| a == "--help" || a == "-h") {
                writeln!(out, "Usage: rudder logs <service> [-f] [-n LINES] [--since 15m|RFC3339] [--level L] [--source S] [--grep TEXT]")?;
                return Ok(());
            }
            logs(&Api::from_context(ctx)?, rest, ctx, out).await
        }
        Some("cache") => match args.next().as_deref() {
            Some("purge") => {
                let rest: Vec<String> = args.collect();
                if rest.iter().any(|a| a == "--help" || a == "-h") {
                    writeln!(out, "Usage: rudder cache purge <service> (--all | --path P... | --prefix P) [--domain ID]")?;
                    return Ok(());
                }
                purge(&Api::from_context(ctx)?, rest, out).await
            }
            _ => bail!("usage: rudder cache purge <service> (--all | --path P... | --prefix P)"),
        },
        Some(other) => bail!("unknown command '{other}'; see rudder --help"),
    }
}

fn print_build_help(out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    writeln!(
        out,
        "\
Usage: rudder build [<service>] [options]

Run local CI checks, build the Docker image with BuildKit, auto-inject GIT_SHA and BUILD_TIME,
and save a compressed archive to the local Rudder Image store (~/.config/rudder/images/).

Options:
  -s, --service <SERVICE>     Target service (id, name, project/name, or project/env/name)
  -t, --tag <TAG>             Image tag (default: YYYYMMDD-HHMMSS-<short-git-sha>)
      --dockerfile <PATH>     Dockerfile path (default: from rudder.toml or Dockerfile)
      --context <DIR>         Build context directory (default: from rudder.toml or .)
      --platform <PLATFORM>   Target platform, e.g. linux/amd64 or linux/arm64
      --no-cache              Do not use Docker layer cache when building
      --progress <MODE>       BuildKit progress output: auto, plain, or tty
      --build-arg <KEY=VAL>   Additional Docker build-arg (repeatable; GIT_SHA & BUILD_TIME are passed automatically)
      --check <CMD>           Local check command to run before building (repeatable)
      --skip-checks           Skip local CI checks configured in rudder.toml
      --local-keep <N>        Number of local image archives to retain
  -f, --file <PATH>           Path to project config file (default: rudder.toml)
  -h, --help                  Print this help message"
    )?;
    Ok(())
}

fn print_push_help(out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    writeln!(
        out,
        "\
Usage: rudder push [TAG] [options]

Upload a locally built image archive to the Rudder Image store (Cloudflare R2) via the panel.
Verifies the SHA-256 digest on upload and skips uploading if the exact tag + digest is already in R2.

Options:
  -s, --service <SERVICE>     Target service (default: from rudder.toml)
  -t, --tag <TAG>             Tag to push (default: most recently built local image)
      --deploy                Deploy the image immediately after upload completes
      --no-deploy             Store the image in R2 without deploying, even if auto-deploy is enabled
      --r2-keep <N>           Retain the newest N versions in R2 (never prunes the deployed image)
  -f, --file <PATH>           Path to project config file (default: rudder.toml)
  -h, --help                  Print this help message"
    )?;
    Ok(())
}

fn print_deploy_help(out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    writeln!(
        out,
        "\
Usage: rudder deploy [<service>] [options]

Deploy a service from Cloudflare R2, a container registry image, a Git build, or a compose stack.

Options:
      --image <TAG|IMAGE>     Tag in R2 (or full registry image reference) to deploy
      --build                 Run `rudder build` + `rudder push --deploy` in one step
      --no-build              Skip building locally even if `deploy_build = true` in rudder.toml
      --skip-checks           Skip local CI checks when --build is active
      --platform <PLATFORM>   Target platform when --build is active (e.g. linux/amd64)
      --no-cache              Disable Docker cache when --build is active
      --progress <MODE>       BuildKit progress mode (auto, plain, tty) when --build is active
      --build-arg <KEY=VAL>   Pass extra --build-arg when --build is active (repeatable)
      --no-wait               Return as soon as the deployment is queued
      --timeout <SECONDS>     Maximum time to wait for deployment completion (default: 1800)
  -f, --file <PATH>           Path to project config file (default: rudder.toml)
  -h, --help                  Print this help message"
    )?;
    Ok(())
}

fn print_images_help(out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    writeln!(
        out,
        "\
Usage:
  rudder images [<service>] [list]
  rudder images [<service>] prune --keep <N>
  rudder images [<service>] rollback <TAG>
  rudder images [<service>] remove <TAG> [--r2-only]

Manage local and Cloudflare R2 image versions for a service."
    )?;
    Ok(())
}

fn build_http_client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let url = attempt.url();
        let host = url.host_str().unwrap_or_default();
        if url.path().starts_with("/cdn-cgi/access/login")
            || host.ends_with(".cloudflareaccess.com")
        {
            attempt.stop()
        } else if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });
    Ok(reqwest::Client::builder()
        .user_agent(format!("rudder-cli/{}", rudder_core::VERSION))
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .redirect(policy)
        .build()?)
}

fn apply_cf_access_headers(mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if let (Ok(id), Ok(secret)) = (
        std::env::var("CF_ACCESS_CLIENT_ID"),
        std::env::var("CF_ACCESS_CLIENT_SECRET"),
    ) {
        if !id.is_empty() && !secret.is_empty() {
            req = req
                .header("CF-Access-Client-Id", id)
                .header("CF-Access-Client-Secret", secret);
        }
    }
    req
}

fn is_cloudflare_access_interception(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    bytes: &[u8],
) -> bool {
    let location = headers
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let www_auth = headers
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let body_preview = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let _ = status;

    location.contains("/cdn-cgi/access/login")
        || location.contains("cloudflareaccess.com")
        || www_auth.contains("Cloudflare-Access")
        || (content_type.contains("text/html") && body_preview.contains("cloudflareaccess.com"))
}

fn check_cloudflare_access_interception(
    base: &str,
    path: &str,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    bytes: &[u8],
) -> anyhow::Result<()> {
    if is_cloudflare_access_interception(status, headers, bytes) {
        let location = headers
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        bail!(
            "Cloudflare Access intercepted `{base}{path}` ({status} -> {location}).\n\
             Ensure 'Let AI agents and API clients through' is enabled under Security -> Cloudflare in the panel, or set `CF_ACCESS_CLIENT_ID` and `CF_ACCESS_CLIENT_SECRET`."
        );
    }
    Ok(())
}

struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
    use_mcp_prefix: std::sync::atomic::AtomicBool,
}

impl Api {
    fn new(base: &str, token: &str) -> anyhow::Result<Self> {
        if !token.starts_with("rdr_") {
            bail!("that is not a Rudder API token (they start with rdr_)");
        }
        Ok(Self {
            http: build_http_client(Duration::from_secs(900))?,
            base: config::normalize_url(base)?,
            token: token.to_owned(),
            use_mcp_prefix: std::sync::atomic::AtomicBool::new(false),
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

    fn effective_path(&self, path: &str) -> String {
        if self.use_mcp_prefix.load(Ordering::Relaxed) && path.starts_with("/api/v1") {
            format!("/mcp{path}")
        } else {
            path.to_owned()
        }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<url::Url> {
        let eff = self.effective_path(path);
        let mut url = url::Url::parse(&format!("{}{eff}", self.base))?;
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
        for pass in 0..2 {
            let url = self.url(path, query)?;
            let mut request = apply_cf_access_headers(
                self.http
                    .request(method.clone(), url)
                    .bearer_auth(&self.token),
            );
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .with_context(|| format!("{method} {}{path}", self.base))?;
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response.bytes().await?;

            if pass == 0
                && !self.use_mcp_prefix.load(Ordering::Relaxed)
                && path.starts_with("/api/v1")
                && is_cloudflare_access_interception(status, &headers, &bytes)
            {
                self.use_mcp_prefix.store(true, Ordering::Relaxed);
                continue;
            }
            check_cloudflare_access_interception(&self.base, path, status, &headers, &bytes)?;

            if status.is_redirection() {
                let loc = headers
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("?");
                bail!(
                    "unexpected HTTP redirect ({status} -> {loc}) from {}{path}; check the panel URL",
                    self.base
                );
            }

            let value: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) if bytes.is_empty() => Value::Null,
                Err(_) if status.is_success() => {
                    let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
                    if self.use_mcp_prefix.load(Ordering::Relaxed)
                        && preview.contains("<!doctype html")
                    {
                        bail!(
                            "Cloudflare Access intercepted `{}{path}`, and the panel at {} is running an older Rudder server version that does not yet route CLI calls through `/mcp/api/v1`.\n\
                             Update the Rudder panel server (System -> Updates), or set `CF_ACCESS_CLIENT_ID` and `CF_ACCESS_CLIENT_SECRET`.",
                            self.base,
                            self.base
                        );
                    }
                    bail!(
                        "expected JSON from {}{path} ({status}), but received non-JSON response: {}",
                        self.base,
                        preview.trim()
                    );
                }
                Err(_) => Value::Null,
            };
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
        unreachable!()
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
        self.call(reqwest::Method::GET, path, query, None).await
    }

    async fn delete(&self, path: &str, query: &[(&str, String)]) -> anyhow::Result<Value> {
        self.call(reqwest::Method::DELETE, path, query, None).await
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
        out: &mut (dyn Write + Send),
    ) -> anyhow::Result<Value> {
        let total_bytes = std::fs::metadata(file_path)
            .with_context(|| format!("stat {}", file_path.display()))?
            .len();
        let max_attempts = 3u32;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                let backoff = Duration::from_millis(500 * u64::from(attempt - 1));
                writeln!(
                    out,
                    "==> Retrying upload (attempt {attempt}/{max_attempts}) after {:.1}s...",
                    backoff.as_secs_f64()
                )?;
                out.flush()?;
                tokio::time::sleep(backoff).await;
            }

            let url = self.url(path, query)?;
            let file = tokio::fs::File::open(file_path)
                .await
                .with_context(|| format!("open {}", file_path.display()))?;
            let uploaded = Arc::new(AtomicU64::new(0));
            let reader = ProgressFile {
                inner: file,
                uploaded: Arc::clone(&uploaded),
            };
            let stream = ReaderStream::new(reader);
            let body = reqwest::Body::wrap_stream(stream);
            let request = apply_cf_access_headers(
                self.http
                    .post(url)
                    .bearer_auth(&self.token)
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .header(reqwest::header::CONTENT_LENGTH, total_bytes),
            )
            .body(body);

            let send_res = request.send().await;
            let sent_bytes = uploaded.load(Ordering::Relaxed);
            if total_bytes > 0 && sent_bytes > 0 {
                let pct = ((sent_bytes as f64 / total_bytes as f64) * 100.0).min(100.0);
                writeln!(
                    out,
                    "==> Uploaded {pct:.0}% ({} / {})",
                    image::format_bytes(sent_bytes),
                    image::format_bytes(total_bytes)
                )?;
                out.flush()?;
            }

            let response = match send_res {
                Ok(resp) => resp,
                Err(err)
                    if attempt < max_attempts
                        && (err.is_connect()
                            || err.is_timeout()
                            || err.is_request()
                            || err.is_body()) =>
                {
                    writeln!(out, "==> Upload interrupted ({err}); will retry...")?;
                    continue;
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("POST {}{path}", self.base));
                }
            };

            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response.bytes().await?;
            if !self.use_mcp_prefix.load(Ordering::Relaxed)
                && path.starts_with("/api/v1")
                && is_cloudflare_access_interception(status, &headers, &bytes)
            {
                self.use_mcp_prefix.store(true, Ordering::Relaxed);
                continue;
            }
            check_cloudflare_access_interception(&self.base, path, status, &headers, &bytes)?;

            if matches!(status.as_u16(), 502..=504) && attempt < max_attempts {
                writeln!(out, "==> Server returned transient {status}; will retry...")?;
                continue;
            }

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
        bail!("upload failed after {max_attempts} attempts")
    }
}

struct ProgressFile {
    inner: tokio::fs::File,
    uploaded: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for ProgressFile {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            let delta = buf.filled().len().saturating_sub(before);
            if delta > 0 {
                self.uploaded.fetch_add(delta as u64, Ordering::Relaxed);
            }
        }
        poll
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
            "--help" | "-h" => {
                writeln!(
                    out,
                    "\
Usage: rudder login --url <panel URL> [--browser | --device] [--name <client-name>]

Sign in to a Rudder panel and save the API token to {}.

Modes:
  (default)     Read an API token (`rdr_...`) from RUDDER_TOKEN or stdin
  --browser     Open your browser and receive the token via a localhost callback
  --device      Print an 8-character device code to approve in the panel

Options:
  --url <URL>   Panel base URL (e.g. https://rudder.example.com)
  --name <NAME> Client label shown under Rudder Image > Signed-in clients",
                    ctx.config_path.display()
                )?;
                return Ok(());
            }
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
    let email = me["user"]["email"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "panel at {} did not return a user profile from /api/v1/me; check the panel URL and Cloudflare Access settings",
                api.base
            )
        })?;
    let role = me["user"]["role"].as_str().unwrap_or("member");
    config::save(
        &ctx.config_path,
        &config::Saved {
            url: api.base.clone(),
            token: api.token.clone(),
        },
    )?;
    writeln!(
        out,
        "Logged in to {} as {email} ({role}). Saved to {}",
        api.base,
        ctx.config_path.display()
    )?;
    Ok(())
}

/// Start a CLI auth session (`device_code` or `browser`), automatically falling back to
/// `/mcp/api/v1/cli/auth/start` when Cloudflare Access intercepts `/api/v1/cli/auth/start`.
async fn start_cli_auth_request(
    http: &reqwest::Client,
    base: &str,
    payload: Value,
    label: &str,
) -> anyhow::Result<(Value, bool)> {
    for use_mcp in [false, true] {
        let path = if use_mcp {
            "/mcp/api/v1/cli/auth/start"
        } else {
            "/api/v1/cli/auth/start"
        };
        let resp = apply_cf_access_headers(http.post(format!("{base}{path}")).json(&payload))
            .send()
            .await
            .with_context(|| format!("start {label} login"))?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await?;
        if !use_mcp && is_cloudflare_access_interception(status, &headers, &bytes) {
            continue;
        }
        check_cloudflare_access_interception(base, path, status, &headers, &bytes)?;
        if !status.is_success() {
            bail!("could not start {label} login ({status})");
        }
        let started: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(err) => {
                let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
                if use_mcp && preview.contains("<!doctype html") {
                    bail!(
                        "Cloudflare Access intercepted `/api/v1/cli/auth/start` on {base}, and the panel server is running an older version that does not yet route CLI login through `/mcp/api/v1`.\n\
                         Update the Rudder panel server (System -> Updates), or set `CF_ACCESS_CLIENT_ID` and `CF_ACCESS_CLIENT_SECRET`."
                    );
                }
                return Err(err).with_context(|| format!("parse {label} login start response"));
            }
        };
        return Ok((started, use_mcp));
    }
    unreachable!()
}

async fn login_via_device(
    base: &str,
    client_name: Option<&str>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<String> {
    let http = build_http_client(Duration::from_secs(30))?;
    let (started, use_mcp) = start_cli_auth_request(
        &http,
        base,
        json!({
            "method": "device_code",
            "client_name": client_name.unwrap_or("rudder-cli"),
        }),
        "device-code",
    )
    .await?;
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

    poll_for_cli_token(&http, base, &request_id, &poll_secret, use_mcp, ctx.poll).await
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

    let http = build_http_client(Duration::from_secs(30))?;
    let (started, use_mcp) = start_cli_auth_request(
        &http,
        base,
        json!({
            "method": "browser",
            "client_name": client_name.unwrap_or("rudder-cli"),
            "callback_url": callback_url,
        }),
        "browser",
    )
    .await?;
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
    let poll_fut = poll_for_cli_token(&http, base, &request_id, &poll_secret, use_mcp, ctx.poll);

    tokio::select! {
        res = callback_fut => {
            if let Ok(token) = res {
                return Ok(token);
            }
            poll_for_cli_token(&http, base, &request_id, &poll_secret, use_mcp, ctx.poll).await
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
    use_mcp: bool,
    interval: Duration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let poll_path = if use_mcp {
        "/mcp/api/v1/cli/auth/poll"
    } else {
        "/api/v1/cli/auth/poll"
    };
    loop {
        tokio::time::sleep(interval).await;
        let resp = apply_cf_access_headers(http.post(format!("{base}{poll_path}")).json(&json!({
            "request_id": request_id,
            "poll_secret": poll_secret,
        })))
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
            "--help" | "-h" => {
                writeln!(
                    out,
                    "Usage: rudder init [<service>] [--file rudder.toml] [--r2-keep N] [--local-keep N] [--force]"
                )?;
                return Ok(());
            }
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
        platform: None,
        checks: Vec::new(),
        build_args: BTreeMap::new(),
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

pub fn parse_build_arg_kv(raw: &str) -> anyhow::Result<(String, String)> {
    let Some((k, v)) = raw.split_once('=') else {
        bail!("--build-arg '{raw}' must be in KEY=VALUE format");
    };
    let key = k.trim();
    if key.is_empty() {
        bail!("--build-arg '{raw}' has an empty key");
    }
    Ok((key.to_owned(), v.to_owned()))
}

/// Best-effort lookup of the currently deployed image tag for `service_spec` so local pruning
/// never deletes the active image. Returns `None` when offline or not signed in.
async fn fetch_deployed_tag_best_effort(ctx: &Context, service_spec: &str) -> Option<String> {
    let api = Api::from_context(ctx).ok()?;
    let service = resolve(&api, service_spec).await.ok()?;
    let detail = api
        .get(&format!("/api/v1/services/{}", service.id), &[])
        .await
        .ok()?;
    let current = detail["current_image"]
        .as_str()
        .or_else(|| detail["spec"]["image"].as_str())?
        .trim();
    if current.is_empty() {
        return None;
    }
    Some(
        current
            .rsplit_once(':')
            .map(|(_, tag)| tag)
            .unwrap_or(current)
            .to_owned(),
    )
}

async fn build_cmd(
    args: Vec<String>,
    ctx: &Context,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let mut service = None;
    let mut tag = None;
    let mut dockerfile = None;
    let mut context_dir = None;
    let mut platform = None;
    let mut no_cache = false;
    let mut progress = None;
    let mut cli_build_args = BTreeMap::new();
    let mut checks = Vec::new();
    let mut skip_checks = false;
    let mut local_keep = None;
    let mut file_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_build_help(out)?;
                return Ok(());
            }
            "--service" | "-s" => service = Some(iter.next().context("--service needs a value")?),
            "--tag" | "-t" => tag = Some(iter.next().context("--tag needs a value")?),
            "--dockerfile" => dockerfile = Some(iter.next().context("--dockerfile needs a path")?),
            "--context" => context_dir = Some(iter.next().context("--context needs a path")?),
            "--platform" => platform = Some(iter.next().context("--platform needs a value")?),
            "--no-cache" => no_cache = true,
            "--progress" => progress = Some(iter.next().context("--progress needs a mode")?),
            "--build-arg" => {
                let raw = iter.next().context("--build-arg needs KEY=VALUE")?;
                let (k, v) = parse_build_arg_kv(&raw)?;
                cli_build_args.insert(k, v);
            }
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
            default_flag if default_flag.starts_with("--progress=") => {
                progress = Some(default_flag.trim_start_matches("--progress=").to_owned());
            }
            default_flag if default_flag.starts_with("--platform=") => {
                platform = Some(default_flag.trim_start_matches("--platform=").to_owned());
            }
            default_flag if default_flag.starts_with("--build-arg=") => {
                let raw = default_flag.trim_start_matches("--build-arg=");
                let (k, v) = parse_build_arg_kv(raw)?;
                cli_build_args.insert(k, v);
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
    let effective_platform = platform.or(cfg.platform);
    let mut merged_build_args = cfg.build_args;
    for (k, v) in cli_build_args {
        merged_build_args.insert(k, v);
    }
    let effective_checks = if checks.is_empty() {
        cfg.checks
    } else {
        checks
    };
    let effective_keep = local_keep.or(cfg.local_keep_count);
    let git_sha = image::detect_git_sha(&context_path);
    let tag = tag.unwrap_or_else(|| image::default_tag(git_sha.as_deref()));
    let protected_tag = if effective_keep.is_some() {
        fetch_deployed_tag_best_effort(ctx, &service).await
    } else {
        None
    };

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
            platform: effective_platform.as_deref(),
            no_cache,
            progress: progress.as_deref(),
            build_args: &merged_build_args,
            protected_tag: protected_tag.as_deref(),
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
    let started = Instant::now();
    let mut tag = None;
    let mut service_spec = None;
    let mut deploy_now = None;
    let mut r2_keep = None;
    let mut file_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_push_help(out)?;
                return Ok(());
            }
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

    // Check if the exact tag + SHA-256 is already present in R2 to skip redundant uploads.
    let existing_in_r2 = api
        .get(&format!("/api/v1/services/{}/images", service.id), &[])
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .find(|row| {
            row["tag"].as_str() == Some(record.tag.as_str())
                && row["sha256"].as_str() == Some(record.sha256.as_str())
                && row["r2_present"].as_bool() == Some(true)
        });

    if existing_in_r2.is_some() {
        writeln!(
            out,
            "==> Archive {} (tag {}, sha256 {}) is already present in Cloudflare R2; skipping upload.",
            record.image_ref,
            record.tag,
            &record.sha256[..12]
        )?;
        writeln!(
            out,
            "Pushed {} (tag {}) to Cloudflare R2 [sha256: {}] (cached)",
            service.path(),
            record.tag,
            &record.sha256[..12]
        )?;
        if deploy_now == Some(true) {
            writeln!(
                out,
                "Deploying {} from Cloudflare R2 ({})",
                service.path(),
                record.tag
            )?;
            let done = api
                .post(
                    &format!("/api/v1/services/{}/images/deploy", service.id),
                    json!({ "tag": record.tag }),
                )
                .await?;
            return report(&done, &mut 0, out);
        }
        writeln!(
            out,
            "Image {} is stored in R2 (auto-deploy is off; run `rudder deploy {}` when ready).",
            record.tag,
            service.path()
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "==> Pushing {} (tag {}, {} / {} bytes) to Cloudflare R2 via {}...",
        record.image_ref,
        record.tag,
        image::format_bytes(record.size_bytes),
        record.size_bytes,
        api.base
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
            out,
        )
        .await?;

    let elapsed = started.elapsed();
    writeln!(
        out,
        "Pushed {} (tag {}) to Cloudflare R2 [sha256: {}]",
        service.path(),
        record.tag,
        &record.sha256[..12]
    )?;
    writeln!(out, "  Service:     {}", service.path())?;
    writeln!(out, "  Image:       {}", record.image_ref)?;
    writeln!(out, "  Tag:         {}", record.tag)?;
    writeln!(
        out,
        "  Size:        {} ({} bytes)",
        image::format_bytes(record.size_bytes),
        record.size_bytes
    )?;
    writeln!(
        out,
        "  Digest:      sha256:{} (verified by panel)",
        record.sha256
    )?;
    writeln!(out, "  Duration:    {:.1}s", elapsed.as_secs_f64())?;
    writeln!(out, "  Dashboard:   {}/services/{}", api.base, service.id)?;

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
    let mut r2_only = false;
    let mut file_path = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "list" | "prune" | "rollback" | "remove" => subcmd = arg,
            "--r2-only" => r2_only = true,
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
                if (subcmd == "rollback" || subcmd == "remove") && target_tag.is_none() {
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
            let protected_tag = fetch_deployed_tag_best_effort(ctx, &service_spec).await;
            let mut removed = image::prune_local_store(
                &images_root,
                &service_spec,
                keep,
                protected_tag.as_deref(),
            )?;
            if removed.is_empty() {
                removed = image::prune_local_store(
                    &images_root,
                    &service.path(),
                    keep,
                    protected_tag.as_deref(),
                )?;
            }
            writeln!(
                out,
                "Pruned {} local image archive(s) for {}",
                removed.len(),
                service.path()
            )?;
            Ok(())
        }
        "remove" => {
            let tag =
                target_tag.context("usage: rudder images <service> remove <tag> [--r2-only]")?;
            let remote = api
                .get(&format!("/api/v1/services/{}/images", service.id), &[])
                .await?;
            let id = remote
                .as_array()
                .and_then(|list| {
                    list.iter()
                        .find(|r| r["tag"].as_str() == Some(tag.as_str()))
                })
                .and_then(|r| r["id"].as_str())
                .map(str::to_owned)
                .with_context(|| format!("no version '{tag}' on record for {}", service.path()))?;
            let query = if r2_only {
                vec![("r2_only", "true".to_owned())]
            } else {
                Vec::new()
            };
            api.delete(
                &format!("/api/v1/services/{}/images/{id}", service.id),
                &query,
            )
            .await?;
            if r2_only {
                writeln!(
                    out,
                    "Freed {tag} from Cloudflare R2 (history kept; local copy untouched)"
                )?;
            } else {
                writeln!(
                    out,
                    "Removed {tag} from Cloudflare R2 and the version history"
                )?;
            }
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

async fn whoami(api: &Api, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let me = api.get("/api/v1/me", &[]).await?;
    writeln!(out, "Panel:   {}", api.base)?;
    writeln!(
        out,
        "Account: {} ({})",
        me["user"]["email"].as_str().unwrap_or("?"),
        me["user"]["role"].as_str().unwrap_or("?")
    )?;
    Ok(())
}

fn human_bytes(bytes: i64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes.max(0))
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

/// `rudder status [--check]`: the same facts as the panel's Rudder Image page.
async fn status(api: &Api, args: Vec<String>, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            other => bail!("unknown option {other}; usage: rudder status [--check]"),
        }
    }
    let overview = api.get("/api/v1/image-store/overview", &[]).await?;
    writeln!(out, "Rudder Image on {}", api.base)?;
    match overview["destination"]["name"].as_str() {
        Some(name) => writeln!(
            out,
            "  Bucket:      {name} ({})",
            overview["destination"]["bucket"].as_str().unwrap_or("?")
        )?,
        None => writeln!(
            out,
            "  Bucket:      not set (panel: S3 > Rudder Image > Setup)"
        )?,
    }
    if check {
        if overview["destination"].is_null() {
            writeln!(out, "  Connection:  skipped, no bucket")?;
        } else {
            let probe = api.post("/api/v1/image-store/test", json!({})).await?;
            if probe["ok"].as_bool() == Some(true) {
                writeln!(out, "  Connection:  ok (write, read, delete)")?;
            } else {
                writeln!(
                    out,
                    "  Connection:  FAILING: {}",
                    probe["message"].as_str().unwrap_or("unknown error")
                )?;
            }
        }
    }
    let n = |v: &Value| v.as_i64().unwrap_or(0);
    let totals = &overview["totals"];
    writeln!(
        out,
        "  Versions:    {} on record, {} on R2 ({}), {} pruned",
        n(&totals["images"]),
        n(&totals["r2_images"]),
        human_bytes(n(&totals["r2_bytes"])),
        n(&totals["pruned_images"])
    )?;
    writeln!(
        out,
        "  Pushes:      {} in 24h, {} in 7d, {} in 30d",
        n(&totals["pushes_24h"]),
        n(&totals["pushes_7d"]),
        n(&totals["pushes_30d"])
    )?;
    let builds = &overview["builds"];
    writeln!(
        out,
        "  Builds 30d:  {} ({} ok, {} failed); {} running, {} queued; {} image deploys",
        n(&builds["builds_30d"]),
        n(&builds["succeeded_30d"]),
        n(&builds["failed_30d"]),
        n(&builds["running"]),
        n(&builds["queued"]),
        n(&builds["image_deploys_30d"])
    )?;
    writeln!(out, "\nSetup:")?;
    for step in overview["setup"].as_array().into_iter().flatten() {
        let mark = match step["state"].as_str() {
            Some("ok") => "[x]",
            Some("optional") => "[-]",
            _ => "[ ]",
        };
        writeln!(
            out,
            "  {mark} {:<12} {}",
            step["key"].as_str().unwrap_or("?"),
            step["detail"].as_str().unwrap_or("")
        )?;
    }
    let services = overview["services"].as_array().cloned().unwrap_or_default();
    if !services.is_empty() {
        writeln!(out, "\nServices:")?;
        for svc in services {
            writeln!(
                out,
                "  {:<40} {:>3} versions  {:>10}  latest {}  {}",
                svc["path"].as_str().unwrap_or("?"),
                n(&svc["images"]),
                human_bytes(n(&svc["r2_bytes"])),
                svc["last_tag"].as_str().unwrap_or("-"),
                if svc["auto_deploy"].as_bool() == Some(true) {
                    "deploys on push"
                } else {
                    "notify only"
                }
            )?;
        }
    }
    let clients = overview["clients"].as_array().map_or(0, Vec::len);
    writeln!(out, "\nSigned-in clients: {clients}")?;
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
    let mut platform = None;
    let mut no_cache = false;
    let mut progress = None;
    let mut cli_build_args = BTreeMap::new();
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
            "--platform" => platform = Some(args.next().context("--platform needs a value")?),
            "--no-cache" => no_cache = true,
            "--progress" => progress = Some(args.next().context("--progress needs a mode")?),
            "--build-arg" => {
                let raw = args.next().context("--build-arg needs KEY=VALUE")?;
                let (k, v) = parse_build_arg_kv(&raw)?;
                cli_build_args.insert(k, v);
            }
            default_flag if default_flag.starts_with("--progress=") => {
                progress = Some(default_flag.trim_start_matches("--progress=").to_owned());
            }
            default_flag if default_flag.starts_with("--platform=") => {
                platform = Some(default_flag.trim_start_matches("--platform=").to_owned());
            }
            default_flag if default_flag.starts_with("--build-arg=") => {
                let raw = default_flag.trim_start_matches("--build-arg=");
                let (k, v) = parse_build_arg_kv(raw)?;
                cli_build_args.insert(k, v);
            }
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
        let effective_platform = platform.or(cfg.platform);
        let mut merged_build_args = cfg.build_args;
        for (k, v) in cli_build_args {
            merged_build_args.insert(k, v);
        }
        let protected_tag = fetch_deployed_tag_best_effort(ctx, &spec).await;
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
                platform: effective_platform.as_deref(),
                no_cache,
                progress: progress.as_deref(),
                build_args: &merged_build_args,
                protected_tag: protected_tag.as_deref(),
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

    #[tokio::test]
    async fn subcommand_help_flags_succeed_without_login() {
        let ctx = Context {
            config_path: std::env::temp_dir().join("nonexistent-rudder-config.json"),
            url: None,
            token: None,
            read_token: Box::new(|| Ok(String::new())),
            poll: Duration::from_millis(10),
        };
        for cmd in [
            "login", "init", "build", "push", "deploy", "images", "status", "logs", "update",
        ] {
            let mut out = Vec::new();
            run(vec![cmd.into(), "--help".into()], &ctx, &mut out)
                .await
                .unwrap_or_else(|e| panic!("`rudder {cmd} --help` failed: {e}"));
            let text = String::from_utf8(out).unwrap();
            assert!(
                text.contains("Usage:"),
                "`rudder {cmd} --help` missing Usage: {text}"
            );
        }
    }

    #[test]
    fn detects_cloudflare_access_redirect_and_gives_actionable_error() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::LOCATION,
            "https://knotree.cloudflareaccess.com/cdn-cgi/access/login/rudder.dymini.com?kid=abc"
                .parse()
                .unwrap(),
        );
        headers.insert(
            reqwest::header::WWW_AUTHENTICATE,
            "Cloudflare-Access".parse().unwrap(),
        );
        let err = check_cloudflare_access_interception(
            "https://rudder.dymini.com",
            "/api/v1/services",
            reqwest::StatusCode::FOUND,
            &headers,
            b"",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Cloudflare Access intercepted"), "{err}");
        assert!(err.contains("CF_ACCESS_CLIENT_ID"), "{err}");
    }

    #[tokio::test]
    async fn cli_login_and_api_automatically_traverse_mcp_bypass_behind_cloudflare_access() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or_default();
                    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
                    let reply = if path.starts_with("/api/v1/") {
                        "HTTP/1.1 302 Found\r\nLocation: https://knotree.cloudflareaccess.com/cdn-cgi/access/login/rudder.dymini.com?kid=1\r\nWww-Authenticate: Cloudflare-Access\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                    } else if path == "/mcp/api/v1/cli/auth/start" {
                        let body = r#"{"request_id":"req-1","poll_secret":"sec-1","device_code":"ABCD1234","verification_path":"/tokens?device=1"}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else if path == "/mcp/api/v1/cli/auth/poll" {
                        let body = r#"{"status":"approved","token":"rdr_testtoken123456"}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else if path == "/mcp/api/v1/me" {
                        let body = r#"{"user":{"email":"admin@example.com","role":"owner"}}"#;
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_owned()
                    };
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "rudder-cf-bypass-test-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Context {
            config_path: dir.join("config.json"),
            url: None,
            token: None,
            read_token: Box::new(|| Ok(String::new())),
            poll: Duration::from_millis(10),
        };
        let mut out = Vec::new();
        run(
            vec![
                "login".into(),
                "--url".into(),
                base.clone(),
                "--device".into(),
            ],
            &ctx,
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ABCD1234"), "{text}");
        assert!(text.contains("admin@example.com (owner)"), "{text}");

        let mut whoami_out = Vec::new();
        run(vec!["whoami".into()], &ctx, &mut whoami_out)
            .await
            .unwrap();
        let whoami_text = String::from_utf8(whoami_out).unwrap();
        assert!(
            whoami_text.contains("admin@example.com (owner)"),
            "{whoami_text}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
