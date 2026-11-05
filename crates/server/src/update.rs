//! Self-update from signed GitHub releases.
//!
//! * [`check`] reads the repository's releases, picks the newest one for the
//!   configured channel, and verifies its minisign-signed `SHA256SUMS` and the
//!   `release.json` it lists. Only a verified manifest can be applied.
//! * [`apply_panel`] takes an encrypted local backup, pulls the panel image by
//!   digest, and starts a one-off updater container from it. The updater
//!   ([`run_updater`]) swaps the `rudder` container, waits for the new panel to
//!   report the new version, and otherwise restores the database from the
//!   backup and restarts the previous container.
//! * [`update_components`] brings every server's agent and cache to the panel's
//!   version: agents get their binary over the session, caches are recreated
//!   from the release's pinned image with a health check and rollback.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use futures_util::StreamExt;
use rudder_core::{
    in_maintenance_window, newer_releases, parse_version, verify_release, ByteStream, Error,
    GithubRelease, NodeDriver, ReadyProbe, ReleaseManifest, ReplaceContainer, UpdateChannel,
    CACHE_CONTAINER, RELEASE_MANIFEST, RELEASE_SIGNATURE, RELEASE_SUMS,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::driver::remote::RemoteHandle;
use crate::events::{self, Event};
use crate::state::AppState;
use crate::{panel, Config};

const MAX_ASSET_BYTES: usize = 4 * 1024 * 1024;
/// Release notes kept per release, and how many unreleased-here releases are listed.
const MAX_NOTES_CHARS: usize = 8_000;
const MAX_PENDING_RELEASES: usize = 10;
const UPDATER_CONTAINER: &str = "rudder-updater";
const KEEP_PRE_UPDATE_BACKUPS: usize = 3;
const PANEL_READY_TIMEOUT: Duration = Duration::from_secs(180);
const AGENT_RETURN_TIMEOUT: Duration = Duration::from_secs(150);
const CACHE_ADMIN_PORT: u16 = 9090;

/// Serializes panel updates and component rollouts within one process.
#[derive(Default)]
pub struct Updates {
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Updates {
    fn try_start(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, ApplyError> {
        self.lock
            .clone()
            .try_lock_owned()
            .map_err(|_| ApplyError::Conflict("an update is already running".into()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleaseInfo {
    pub version: String,
    pub name: Option<String>,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    pub html_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateCheck {
    pub checked_at: DateTime<Utc>,
    pub current: String,
    pub channel: String,
    /// Newest release above the current version, if any.
    pub latest: Option<ReleaseInfo>,
    /// Every release between the installed version and `latest`, oldest first,
    /// `latest` included: what an update installs, with each release's notes.
    #[serde(default)]
    pub pending: Vec<ReleaseInfo>,
    /// The latest release's signature and checksums were verified.
    pub verified: bool,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(ignore)]
    pub manifest: Option<ReleaseManifest>,
}

impl UpdateCheck {
    fn public(mut self) -> Self {
        self.manifest = None;
        self
    }
}

fn http_client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .user_agent(format!("rudder/{}", rudder_core::VERSION))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| Error::Node(format!("http client: {error}")))
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, Error> {
    let response = client
        .get(url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|error| Error::Node(format!("GET {url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Node(format!("GET {url}: {status}")));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ASSET_BYTES as u64)
    {
        return Err(Error::Node(format!("{url} is too large")));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| Error::Node(format!("GET {url}: {error}")))?;
        if body.len() + chunk.len() > MAX_ASSET_BYTES {
            return Err(Error::Node(format!("{url} is too large")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |part: Option<&str>| {
        part.is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// Look for a newer release and verify it. The result is stored either way.
pub async fn check(state: &AppState) -> Result<UpdateCheck, Error> {
    let settings = crate::db::load_panel_settings(&state.pool)
        .await
        .map_err(|error| Error::Node(format!("{error:#}")))?;
    let channel = UpdateChannel::parse(&settings.update_channel)?;
    let outcome = discover(&state.config, &settings.github_repo, channel).await;
    let check = match outcome {
        Ok(check) => check,
        Err(error) => UpdateCheck {
            checked_at: Utc::now(),
            current: rudder_core::VERSION.into(),
            channel: channel.as_str().into(),
            latest: None,
            pending: Vec::new(),
            verified: false,
            error: Some(error.to_string()),
            manifest: None,
        },
    };
    let stored = serde_json::to_value(&check).unwrap_or_default();
    sqlx::query!(
        r#"
        UPDATE panel_settings
        SET update_checked_at = $1, update_last_check = $2, updated_at = now()
        WHERE id = TRUE
        "#,
        check.checked_at,
        stored,
    )
    .execute(&state.pool)
    .await
    .map_err(|error| Error::Node(format!("store the update check: {error}")))?;
    Ok(check)
}

async fn discover(
    config: &Config,
    repo: &str,
    channel: UpdateChannel,
) -> Result<UpdateCheck, Error> {
    if !valid_repo(repo) {
        return Err(Error::Validation(format!(
            "update repository '{repo}' is not owner/name"
        )));
    }
    let client = http_client()?;
    let url = format!(
        "{}/repos/{repo}/releases?per_page=50",
        config.update_api_url
    );
    let releases: Vec<GithubRelease> = serde_json::from_slice(&fetch(&client, &url).await?)
        .map_err(|error| Error::Node(format!("GitHub releases: {error}")))?;
    let current = parse_version(rudder_core::VERSION)?;
    let mut check = UpdateCheck {
        checked_at: Utc::now(),
        current: rudder_core::VERSION.into(),
        channel: channel.as_str().into(),
        latest: None,
        pending: Vec::new(),
        verified: false,
        error: None,
        manifest: None,
    };
    let newer = newer_releases(&releases, &current, channel);
    let Some((release, version)) = newer.last() else {
        return Ok(check);
    };
    let info = |release: &GithubRelease, version: &semver::Version| ReleaseInfo {
        version: version.to_string(),
        name: release.name.clone(),
        notes: release
            .body
            .as_ref()
            .map(|body| body.chars().take(MAX_NOTES_CHARS).collect()),
        published_at: release.published_at.clone(),
        html_url: release.html_url.clone(),
    };
    check.latest = Some(info(release, version));
    check.pending = newer
        .iter()
        .rev()
        .take(MAX_PENDING_RELEASES)
        .rev()
        .map(|(release, version)| info(release, version))
        .collect();
    let Some(key) = config.update_public_key.as_deref() else {
        check.error = Some(
            "this build has no release signing key (RUDDER_UPDATE_PUBKEY), so releases cannot be verified or applied"
                .into(),
        );
        return Ok(check);
    };
    match verify_assets(&client, release, key, version).await {
        Ok(manifest) => {
            check.verified = true;
            check.manifest = Some(manifest);
        }
        Err(error) => check.error = Some(error.to_string()),
    }
    Ok(check)
}

async fn verify_assets(
    client: &reqwest::Client,
    release: &GithubRelease,
    key: &str,
    version: &semver::Version,
) -> Result<ReleaseManifest, Error> {
    let asset = |name: &str| {
        release
            .asset(name)
            .ok_or_else(|| Error::Validation(format!("release {} has no {name}", release.tag_name)))
    };
    let sums = fetch(client, &asset(RELEASE_SUMS)?.browser_download_url).await?;
    let signature = fetch(client, &asset(RELEASE_SIGNATURE)?.browser_download_url).await?;
    let manifest = fetch(client, &asset(RELEASE_MANIFEST)?.browser_download_url).await?;
    let signature = String::from_utf8(signature)
        .map_err(|_| Error::Validation("release signature is not text".into()))?;
    let (manifest, _) = verify_release(key, &sums, &signature, &manifest, version)?;
    Ok(manifest)
}

/// The stored result of the last check.
pub async fn last_check(pool: &PgPool) -> Result<Option<UpdateCheck>, sqlx::Error> {
    let value =
        sqlx::query_scalar!(r#"SELECT update_last_check FROM panel_settings WHERE id = TRUE"#)
            .fetch_one(pool)
            .await?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

pub async fn last_public_check(pool: &PgPool) -> Result<Option<UpdateCheck>, sqlx::Error> {
    Ok(last_check(pool).await?.map(UpdateCheck::public))
}

/// Why the panel cannot replace itself, if it cannot.
pub async fn apply_blocker(state: &AppState) -> Option<String> {
    if state.config.update_public_key.is_none() {
        return Some("this build has no release signing key (RUDDER_UPDATE_PUBKEY)".into());
    }
    match state
        .local_driver
        .container_image(&state.config.self_container)
        .await
    {
        Ok(Some(_)) => None,
        Ok(None) => Some(format!(
            "the panel is not running in a Docker container named '{}'; update it by running install.sh with --version",
            state.config.self_container
        )),
        Err(error) => Some(format!("Docker is not reachable: {error}")),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Failed(#[from] Error),
}

/// Start a panel update to the latest verified release. Returns its history id.
pub async fn apply_panel(state: &AppState, actor: Option<Uuid>) -> Result<Uuid, ApplyError> {
    let guard = state.updates.try_start()?;
    if let Some(reason) = apply_blocker(state).await {
        return Err(ApplyError::Conflict(reason));
    }
    if running_update(&state.pool).await? {
        return Err(ApplyError::Conflict("an update is already running".into()));
    }
    let check = check(state).await?;
    let (Some(latest), Some(manifest)) = (check.latest.clone(), check.manifest.clone()) else {
        return Err(ApplyError::Conflict(match check.error {
            Some(error) => format!("no verified release to apply: {error}"),
            None => "the panel is already on the newest release".into(),
        }));
    };
    let manifest_json = serde_json::to_value(&manifest).unwrap_or_default();
    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO update_history (component, from_version, to_version, status, logs, manifest, created_by)
        VALUES ('panel', $1, $2, 'running', '', $3, $4)
        RETURNING id
        "#,
        rudder_core::VERSION,
        latest.version,
        manifest_json,
        actor,
    )
    .fetch_one(&state.pool)
    .await
    .map_err(db_error)?;
    let state = state.clone();
    tokio::spawn(async move {
        let _held = guard;
        if let Err(error) = prepare_panel(&state, id, &manifest).await {
            finish(&state.pool, id, "failed", &format!("{error}\n")).await;
            events::record(
                &state.pool,
                Event::critical("update.failed", "Panel update failed").message(error.to_string()),
            )
            .await;
        }
    });
    Ok(id)
}

async fn running_update(pool: &PgPool) -> Result<bool, Error> {
    sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM update_history
            WHERE status = 'running' AND started_at > now() - INTERVAL '1 hour'
        ) AS "running!"
        "#
    )
    .fetch_one(pool)
    .await
    .map_err(db_error)
}

fn db_error(error: sqlx::Error) -> Error {
    Error::Node(format!("database: {error}"))
}

async fn append_log(pool: &PgPool, id: Uuid, line: &str) {
    let mut line = line.trim_end().to_owned();
    line.push('\n');
    let _ = sqlx::query!(
        r#"UPDATE update_history SET logs = right(logs || $2, 200000) WHERE id = $1"#,
        id,
        line,
    )
    .execute(pool)
    .await;
}

async fn finish(pool: &PgPool, id: Uuid, status: &str, line: &str) {
    if !line.is_empty() {
        append_log(pool, id, line).await;
    }
    let _ = sqlx::query!(
        r#"UPDATE update_history SET status = $2, finished_at = now() WHERE id = $1"#,
        id,
        status,
    )
    .execute(pool)
    .await;
}

/// Passphrase of the local pre-update backups: the master key, which already
/// sits next to them on this host.
fn local_backup_passphrase(config: &Config) -> String {
    hex::encode(config.master_key())
}

fn updates_dir(config: &Config) -> PathBuf {
    config.data_dir.join("updates")
}

/// Backup, pull, and hand over to the updater container.
async fn prepare_panel(
    state: &AppState,
    id: Uuid,
    manifest: &ReleaseManifest,
) -> Result<(), Error> {
    let pool = &state.pool;
    append_log(
        pool,
        id,
        &format!(
            "updating the panel from {} to {}",
            rudder_core::VERSION,
            manifest.version
        ),
    )
    .await;
    let migration = panel::applied_migration(pool).await?;
    let backup = write_pre_update_backup(state).await?;
    append_log(
        pool,
        id,
        &format!(
            "backup written to {} (schema {migration})",
            backup.display()
        ),
    )
    .await;
    append_log(pool, id, &format!("pulling {}", manifest.images.panel)).await;
    state
        .local_driver
        .pull_image(&manifest.images.panel, None)
        .await?;
    let command = vec![
        "self-update".to_owned(),
        "--container".to_owned(),
        state.config.self_container.clone(),
        "--image".to_owned(),
        manifest.images.panel.clone(),
        "--history".to_owned(),
        id.to_string(),
        "--backup".to_owned(),
        backup.display().to_string(),
        "--from-version".to_owned(),
        rudder_core::VERSION.to_owned(),
        "--to-version".to_owned(),
        manifest.version.clone(),
        "--migration".to_owned(),
        migration.to_string(),
    ];
    state
        .local_driver
        .run_clone(
            &state.config.self_container,
            UPDATER_CONTAINER,
            &manifest.images.panel,
            command,
        )
        .await?;
    append_log(pool, id, "started the updater; the panel restarts shortly").await;
    Ok(())
}

async fn write_pre_update_backup(state: &AppState) -> Result<PathBuf, Error> {
    let dir = updates_dir(&state.config);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| Error::Node(format!("create {}: {error}", dir.display())))?;
    #[cfg(unix)]
    std::fs::set_permissions(
        &dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .map_err(|error| Error::Node(format!("protect {}: {error}", dir.display())))?;
    let path = dir.join(format!(
        "pre-update-{}-{}.tar.zst.enc",
        rudder_core::VERSION,
        Utc::now().format("%Y%m%d-%H%M%S")
    ));
    let archive = panel::archive(&state.pool, &state.config, false).await?;
    let mut stream = panel::seal(archive, &local_backup_passphrase(&state.config))?;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts
        .open(&path)
        .await
        .map_err(|error| Error::Node(format!("create {}: {error}", path.display())))?;
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?)
            .await
            .map_err(|error| Error::Node(format!("write the backup: {error}")))?;
    }
    file.sync_all()
        .await
        .map_err(|error| Error::Node(format!("write the backup: {error}")))?;
    prune_backups(&dir);
    Ok(path)
}

fn prune_backups(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pre-update-") && name.ends_with(".enc"))
        })
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(KEEP_PRE_UPDATE_BACKUPS);
    for path in backups.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Arguments of `rudder-server self-update`, which runs in the updater container.
#[derive(Debug, Clone)]
pub struct UpdaterArgs {
    pub container: String,
    pub image: String,
    pub history: Uuid,
    pub backup: PathBuf,
    pub from_version: String,
    pub to_version: String,
    pub migration: i64,
}

/// Swap the panel container, check the new version answers, and roll back if not.
pub async fn run_updater(config: Config, args: UpdaterArgs) -> anyhow::Result<bool> {
    let pool = crate::db::connect(&config).await?;
    let driver =
        rudder_node::LocalDriver::connect(&config.data_dir, config.docker_socket.as_deref());
    let id = args.history;
    // Give the old panel a moment to answer the request that started the update.
    tokio::time::sleep(Duration::from_secs(2)).await;
    append_log(
        &pool,
        id,
        &format!("updater {} started", rudder_core::VERSION),
    )
    .await;
    let swap = match driver
        .swap_control_plane(&args.container, &args.image)
        .await
    {
        Ok(swap) => swap,
        Err(error) => {
            finish(
                &pool,
                id,
                "failed",
                &format!("could not start the new panel: {error}; the previous container was kept"),
            )
            .await;
            return Ok(false);
        }
    };
    append_log(
        &pool,
        id,
        &format!("started {} on {}", swap.name, swap.image),
    )
    .await;
    let probe = ReadyProbe::Exec {
        command: vec![
            "wget".into(),
            "-qO-".into(),
            format!("http://127.0.0.1:{}/api/health", config.bind.port()),
        ],
        expect: Some(format!("\"version\":\"{}\"", args.to_version)),
    };
    match driver
        .wait_ready(&swap.name, &probe, PANEL_READY_TIMEOUT)
        .await
    {
        Ok(()) => {
            if let Err(error) = driver.finish_swap(&swap).await {
                append_log(
                    &pool,
                    id,
                    &format!("could not remove {}: {error}", swap.previous),
                )
                .await;
            }
            finish(
                &pool,
                id,
                "succeeded",
                &format!("panel {} is healthy", args.to_version),
            )
            .await;
            events::record(
                &pool,
                Event::info(
                    "update.succeeded",
                    format!("Panel updated to {}", args.to_version),
                ),
            )
            .await;
            Ok(true)
        }
        Err(reason) => {
            append_log(&pool, id, &format!("{reason}; rolling back")).await;
            let output = driver
                .container_logs(&swap.name, 60)
                .await
                .unwrap_or_default();
            let _ = driver.stop_container(&swap.name, 30).await;
            pool.close().await;
            let restored = restore_if_migrated(&config, &args).await;
            let reverted = driver.revert_swap(&swap).await;
            let pool = crate::db::connect(&config).await?;
            let mut log = format!("--- last output of {} ---\n{output}\n", args.to_version);
            match &restored {
                Ok(Some(_)) => log.push_str("restored the database from the pre-update backup\n"),
                Ok(None) => log.push_str("the schema was unchanged; no database restore needed\n"),
                Err(error) => log.push_str(&format!("database restore failed: {error:#}\n")),
            }
            let status = match reverted {
                Ok(_) => {
                    let back = ReadyProbe::Exec {
                        command: vec![
                            "wget".into(),
                            "-qO-".into(),
                            format!("http://127.0.0.1:{}/api/health", config.bind.port()),
                        ],
                        expect: Some(format!("\"version\":\"{}\"", args.from_version)),
                    };
                    match driver
                        .wait_ready(&args.container, &back, PANEL_READY_TIMEOUT)
                        .await
                    {
                        Ok(()) => {
                            log.push_str(&format!("panel {} is running again", args.from_version));
                            if restored.is_ok() {
                                "rolled_back"
                            } else {
                                "failed"
                            }
                        }
                        Err(error) => {
                            log.push_str(&format!("the previous panel did not come back: {error}"));
                            "failed"
                        }
                    }
                }
                Err(error) => {
                    log.push_str(&format!(
                        "could not restore the previous container: {error}"
                    ));
                    "failed"
                }
            };
            finish(&pool, id, status, &log).await;
            events::record(
                &pool,
                Event::critical(
                    "update.rolled_back",
                    format!("Panel update to {} rolled back", args.to_version),
                )
                .message(reason),
            )
            .await;
            Ok(false)
        }
    }
}

async fn restore_if_migrated(config: &Config, args: &UpdaterArgs) -> anyhow::Result<Option<()>> {
    let pool = crate::db::connect(config).await?;
    let applied = panel::applied_migration(&pool).await?;
    pool.close().await;
    if applied <= args.migration {
        return Ok(None);
    }
    let key_out = match &config.master_key_file {
        Some(path) => path.clone(),
        None => config.data_dir.join("tmp").join("restore-master.key"),
    };
    panel::restore(panel::RestoreOptions {
        archive: args.backup.clone(),
        passphrase: Some(local_backup_passphrase(config)),
        database_url: config.database_url.clone(),
        master_key_out: key_out.clone(),
        force: true,
        pg_restore: config.pg_restore.clone(),
        known_migration: crate::db::known_migration(),
    })
    .await?;
    if config.master_key_file.is_none() {
        let _ = std::fs::remove_file(key_out);
    }
    Ok(Some(()))
}

/// Cache image the running panel version ships with, from its verified release.
pub async fn target_cache_image(pool: &PgPool) -> Result<Option<String>, sqlx::Error> {
    let applied = sqlx::query_scalar!(
        r#"
        SELECT manifest FROM update_history
        WHERE component = 'panel' AND status = 'succeeded' AND to_version = $1 AND manifest IS NOT NULL
        ORDER BY started_at DESC LIMIT 1
        "#,
        rudder_core::VERSION,
    )
    .fetch_optional(pool)
    .await?
    .flatten()
    .and_then(|value| serde_json::from_value::<ReleaseManifest>(value).ok());
    if let Some(manifest) = applied {
        return Ok(Some(manifest.images.cache));
    }
    Ok(last_check(pool)
        .await?
        .and_then(|check| check.manifest)
        .filter(|manifest| manifest.version == rudder_core::VERSION)
        .map(|manifest| manifest.images.cache)
        .or_else(|| {
            std::env::var("RUDDER_CACHE_IMAGE")
                .ok()
                .filter(|v| !v.is_empty())
        }))
}

/// Agent and cache versions per server.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ComponentStatus {
    pub server_id: Uuid,
    pub server_name: String,
    pub kind: String,
    pub online: bool,
    pub agent_version: Option<String>,
    pub agent_outdated: bool,
    pub cache_image: Option<String>,
    pub cache_outdated: bool,
}

pub async fn components(state: &AppState) -> Result<Vec<ComponentStatus>, Error> {
    let target_cache = target_cache_image(&state.pool).await.map_err(db_error)?;
    let servers = sqlx::query!(
        r#"SELECT id, name, kind, status, agent_version FROM servers ORDER BY kind, name"#
    )
    .fetch_all(&state.pool)
    .await
    .map_err(db_error)?;
    let mut out = Vec::with_capacity(servers.len());
    for server in servers {
        let remote = server.kind == "remote";
        let driver: Option<Arc<dyn NodeDriver>> = if remote {
            state.agents.driver(server.id)
        } else {
            Some(state.local_driver.clone())
        };
        let online = !remote || driver.is_some();
        let cache_image = match &driver {
            Some(driver) => tokio::time::timeout(
                Duration::from_secs(5),
                driver.container_image(CACHE_CONTAINER),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten(),
            None => None,
        };
        let agent_outdated = remote
            && server
                .agent_version
                .as_deref()
                .is_some_and(|version| version != rudder_core::VERSION);
        let cache_outdated = matches!((&cache_image, &target_cache), (Some(current), Some(target)) if current != target);
        out.push(ComponentStatus {
            server_id: server.id,
            server_name: server.name,
            kind: server.kind,
            online,
            agent_version: if remote { server.agent_version } else { None },
            agent_outdated,
            cache_image,
            cache_outdated,
        });
    }
    Ok(out)
}

/// Update outdated agents first, then caches, one server at a time, in the background.
/// Returns how many components will be updated.
pub async fn update_components(state: &AppState, actor: Option<Uuid>) -> Result<usize, ApplyError> {
    let guard = state.updates.try_start()?;
    let list = components(state).await?;
    let target_cache = target_cache_image(&state.pool).await.map_err(db_error)?;
    let work: Vec<ComponentStatus> = list
        .into_iter()
        .filter(|c| c.online && (c.agent_outdated || c.cache_outdated))
        .collect();
    let count = work
        .iter()
        .map(|c| usize::from(c.agent_outdated) + usize::from(c.cache_outdated))
        .sum();
    if count == 0 {
        return Ok(0);
    }
    let state = state.clone();
    tokio::spawn(async move {
        let _held = guard;
        for component in work {
            let mut agent_ok = true;
            if component.agent_outdated {
                agent_ok = update_agent(&state, &component, actor).await;
            }
            if component.cache_outdated && agent_ok {
                if let Some(target) = &target_cache {
                    update_cache(&state, &component, target, actor).await;
                }
            }
        }
    });
    Ok(count)
}

async fn start_row(
    pool: &PgPool,
    component: &str,
    server_id: Uuid,
    from: Option<&str>,
    to: &str,
    actor: Option<Uuid>,
) -> Option<Uuid> {
    sqlx::query_scalar!(
        r#"
        INSERT INTO update_history (component, server_id, from_version, to_version, status, created_by)
        VALUES ($1, $2, $3, $4, 'running', $5)
        RETURNING id
        "#,
        component,
        server_id,
        from,
        to,
        actor,
    )
    .fetch_one(pool)
    .await
    .ok()
}

async fn update_agent(state: &AppState, component: &ComponentStatus, actor: Option<Uuid>) -> bool {
    let pool = &state.pool;
    let Some(id) = start_row(
        pool,
        "agent",
        component.server_id,
        component.agent_version.as_deref(),
        rudder_core::VERSION,
        actor,
    )
    .await
    else {
        return false;
    };
    match push_agent(state, component, id).await {
        Ok(()) => {
            finish(
                pool,
                id,
                "succeeded",
                &format!(
                    "agent on {} reconnected on {}",
                    component.server_name,
                    rudder_core::VERSION
                ),
            )
            .await;
            true
        }
        Err(error) => {
            finish(pool, id, "failed", &error.to_string()).await;
            events::record(
                pool,
                Event::warning(
                    "update.failed",
                    format!("Agent update on {} failed", component.server_name),
                )
                .server(component.server_id)
                .message(error.to_string()),
            )
            .await;
            false
        }
    }
}

async fn push_agent(state: &AppState, component: &ComponentStatus, id: Uuid) -> Result<(), Error> {
    let session = state
        .agents
        .session(component.server_id)
        .ok_or_else(|| Error::Node("the agent is not connected".into()))?;
    let handle = RemoteHandle(session.clone());
    let info = handle.agent_info().await.map_err(|error| {
        Error::Node(format!("the agent cannot report its target, so it predates self-update; reinstall it with install-agent.sh ({error})"))
    })?;
    if !rudder_core::AGENT_TARGETS.contains(&info.target.as_str()) {
        return Err(Error::Validation(format!(
            "no agent build for {}",
            info.target
        )));
    }
    let path = state
        .config
        .dist_dir
        .join(format!("rudder-agent-{}", info.target));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| Error::Node(format!("read {}: {error}", path.display())))?;
    let sha256 = rudder_core::sha256_hex(&bytes);
    let published = tokio::fs::read_to_string(path.with_extension("sha256"))
        .await
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_ascii_lowercase));
    if published.as_deref().is_some_and(|hash| hash != sha256) {
        return Err(Error::Validation(format!(
            "{} does not match its .sha256 file",
            path.display()
        )));
    }
    if let Some(manifest) = last_check(&state.pool)
        .await
        .ok()
        .flatten()
        .and_then(|c| c.manifest)
    {
        if manifest.version == rudder_core::VERSION {
            if let Some(expected) = manifest.agents.get(&info.target) {
                if *expected != sha256 {
                    return Err(Error::Validation(
                        "the bundled agent does not match the signed release".into(),
                    ));
                }
            }
        }
    }
    append_log(
        &state.pool,
        id,
        &format!(
            "sending {} ({} bytes, sha256 {sha256}) to {} running {}",
            path.display(),
            bytes.len(),
            component.server_name,
            info.version
        ),
    )
    .await;
    let stream: ByteStream = Box::pin(futures_util::stream::iter(
        bytes
            .chunks(256 * 1024)
            .map(|chunk| Ok(bytes::Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>(),
    ));
    handle
        .update_agent(rudder_core::VERSION, &sha256, stream)
        .await?;
    append_log(
        &state.pool,
        id,
        "the agent installed the binary and is restarting",
    )
    .await;
    let deadline = tokio::time::Instant::now() + AGENT_RETURN_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let current = state.agents.session(component.server_id);
        let reconnected = current
            .as_ref()
            .is_some_and(|s| s.session_id != session.session_id);
        if reconnected {
            let version = sqlx::query_scalar!(
                r#"SELECT agent_version FROM servers WHERE id = $1"#,
                component.server_id
            )
            .fetch_one(&state.pool)
            .await
            .map_err(db_error)?;
            if version.as_deref() == Some(rudder_core::VERSION) {
                return Ok(());
            }
            return Err(Error::Node(format!(
                "the agent reconnected on {}; it restored its previous binary",
                version.unwrap_or_default()
            )));
        }
        if tokio::time::Instant::now() > deadline {
            return Err(Error::Node(
                "the agent did not reconnect; it restores its previous binary after its grace period".into(),
            ));
        }
    }
}

async fn update_cache(
    state: &AppState,
    component: &ComponentStatus,
    target: &str,
    actor: Option<Uuid>,
) {
    let pool = &state.pool;
    let Some(id) = start_row(
        pool,
        "cache",
        component.server_id,
        component.cache_image.as_deref(),
        target,
        actor,
    )
    .await
    else {
        return;
    };
    let driver: Option<Arc<dyn NodeDriver>> = if component.kind == "remote" {
        state.agents.driver(component.server_id)
    } else {
        Some(state.local_driver.clone())
    };
    let Some(driver) = driver else {
        finish(pool, id, "failed", "the agent is not connected").await;
        return;
    };
    append_log(
        pool,
        id,
        &format!("pulling {target} on {}", component.server_name),
    )
    .await;
    if let Err(error) = driver.pull_image(target, None).await {
        finish(pool, id, "failed", &format!("pull failed: {error}")).await;
        return;
    }
    let request = ReplaceContainer {
        name: CACHE_CONTAINER.into(),
        image: target.into(),
        probe: ReadyProbe::Http {
            port: CACHE_ADMIN_PORT,
            path: "/health".into(),
        },
        timeout_secs: 60,
    };
    match driver.replace_container(&request).await {
        Ok(report) => {
            let status = if report.rolled_back {
                "rolled_back"
            } else {
                "succeeded"
            };
            finish(pool, id, status, &report.log).await;
            if report.rolled_back {
                events::record(
                    pool,
                    Event::warning(
                        "update.rolled_back",
                        format!("Cache update on {} rolled back", component.server_name),
                    )
                    .server(component.server_id),
                )
                .await;
            }
        }
        Err(error) => finish(pool, id, "failed", &error.to_string()).await,
    }
}

/// Periodic release check; applies updates when auto-update is on and the
/// maintenance window allows it.
pub fn spawn_background(state: AppState) {
    let every = state.config.update_check_secs;
    if every == 0 {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            auto_update_tick(&state).await;
            tokio::time::sleep(Duration::from_secs(every)).await;
        }
    });
}

async fn auto_update_tick(state: &AppState) {
    let _ = sqlx::query!(
        r#"
        UPDATE update_history SET status = 'failed', finished_at = now(),
            logs = logs || 'abandoned: no progress for an hour' || chr(10)
        WHERE status = 'running' AND started_at < now() - INTERVAL '1 hour'
        "#
    )
    .execute(&state.pool)
    .await;
    let check = match check(state).await {
        Ok(check) => check,
        Err(error) => {
            tracing::warn!(%error, "release check failed");
            return;
        }
    };
    let Ok(settings) = sqlx::query!(
        r#"SELECT auto_update, maintenance_window FROM panel_settings WHERE id = TRUE"#
    )
    .fetch_one(&state.pool)
    .await
    else {
        return;
    };
    if !settings.auto_update {
        return;
    }
    let now = Utc::now();
    let minute = u16::try_from(now.hour() * 60 + now.minute()).unwrap_or(0);
    if !in_maintenance_window(settings.maintenance_window.as_deref(), minute).unwrap_or(false) {
        return;
    }
    if check.verified && check.latest.is_some() {
        match apply_panel(state, None).await {
            Ok(id) => tracing::info!(%id, "automatic panel update started"),
            Err(error) => tracing::warn!(%error, "automatic panel update did not start"),
        }
        return;
    }
    if let Err(error) = update_components(state, None).await {
        tracing::warn!(%error, "automatic component update did not start");
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UpdateHistoryEntry {
    pub id: Uuid,
    pub component: String,
    pub server_id: Option<Uuid>,
    pub server_name: Option<String>,
    pub from_version: Option<String>,
    pub to_version: String,
    pub status: String,
    pub logs: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub async fn history(pool: &PgPool, limit: i64) -> Result<Vec<UpdateHistoryEntry>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT h.id, h.component, h.server_id, s.name AS "server_name?", h.from_version,
               h.to_version, h.status, h.logs, h.started_at, h.finished_at
        FROM update_history h
        LEFT JOIN servers s ON s.id = h.server_id
        ORDER BY h.started_at DESC
        LIMIT $1
        "#,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| UpdateHistoryEntry {
            id: row.id,
            component: row.component,
            server_id: row.server_id,
            server_name: row.server_name,
            from_version: row.from_version,
            to_version: row.to_version,
            status: row.status,
            logs: row.logs,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_must_be_owner_and_name() {
        assert!(valid_repo("vantanminh/rudder"));
        assert!(valid_repo("a-b/c.d_e"));
        assert!(!valid_repo("vantanminh"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("a/../b"));
        assert!(!valid_repo("a/b?x=1"));
    }

    #[test]
    fn only_three_pre_update_backups_are_kept() {
        let dir = std::env::temp_dir().join(format!("rudder-updates-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for day in 1..=5 {
            std::fs::write(
                dir.join(format!("pre-update-0.1.0-2026010{day}-000000.tar.zst.enc")),
                b"x",
            )
            .unwrap();
        }
        std::fs::write(dir.join("unrelated.txt"), b"x").unwrap();
        prune_backups(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), 4);
        assert!(left[0].contains("20260103"));
        assert_eq!(left[3], "unrelated.txt");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
