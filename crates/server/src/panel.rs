//! Panel disaster recovery: settings export, panel backups, and offline restore.
//!
//! A panel backup is a tar of `manifest.json`, `master.key`, `database.pgdump`
//! (`pg_dump --format=custom`), and `settings.json`. It carries the master key,
//! so it is always encrypted with a passphrase before it leaves the panel.
//! [`restore`] runs before the panel starts, from `rudder-server restore`.

use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, TryStreamExt};
use rudder_core::{ByteStream, Compression, Error};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::io::{ReaderStream, StreamReader, SyncIoBridge};
use uuid::Uuid;

use crate::backup::pipeline;
use crate::backup::tarfilter::{tokio_stream_from, ChannelWriter};
use crate::config::Config;
use crate::error::ApiError;

pub const FORMAT: &str = "rudder-panel-backup";
pub const FORMAT_VERSION: u32 = 1;
pub const MIN_PASSPHRASE: usize = 12;

const MANIFEST: &str = "manifest.json";
const MASTER_KEY: &str = "master.key";
const DUMP: &str = "database.pgdump";
const SETTINGS: &str = "settings.json";
const ENTRIES: [&str; 4] = [MANIFEST, MASTER_KEY, DUMP, SETTINGS];

/// Tables whose rows are history rather than configuration. Skipped unless asked for.
const HISTORY_TABLES: [&str; 4] = [
    "public.log_entries*",
    "public.metrics_samples*",
    "public.metrics_rollups*",
    "public.sessions",
];

/// Describes a panel backup. Stored as `manifest.json` inside the archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub rudder_version: String,
    pub created_at: DateTime<Utc>,
    /// Highest applied schema migration when the dump was taken.
    pub migration: i64,
    pub includes_history: bool,
}

pub fn validate_passphrase(passphrase: &str) -> Result<(), Error> {
    if passphrase.chars().count() < MIN_PASSPHRASE {
        return Err(Error::Validation(format!(
            "the passphrase must be at least {MIN_PASSPHRASE} characters"
        )));
    }
    Ok(())
}

/// Configuration of the panel as JSON, without secrets.
///
/// Columns ending in `_enc` or `_hash`, and session material, are removed at any
/// depth, so new encrypted columns stay out of the export without changes here.
pub async fn settings_export(pool: &PgPool) -> Result<Value, ApiError> {
    let mut export = sqlx::query_scalar!(
        r#"
        SELECT jsonb_build_object(
            'panel_settings', (SELECT to_jsonb(p) FROM panel_settings p),
            'users', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM users t),
            'api_tokens', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM api_tokens t),
            'servers', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM servers t),
            'projects', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM projects t),
            'environments', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM environments t),
            'services', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM services t),
            'env_vars', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM env_vars t),
            'domains', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM domains t),
            'certificates', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM certificates t),
            'registries', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM registries t),
            'git_providers', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM git_providers t),
            'deploy_keys', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM deploy_keys t),
            's3_destinations', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM s3_destinations t),
            'backup_jobs', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM backup_jobs t),
            'cache_policies', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM cache_policies t),
            'cache_rules', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM cache_rules t),
            'alerts', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM alerts t),
            'notification_channels', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM notification_channels t),
            'retention_settings', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM retention_settings t),
            'cleanup_schedules', (SELECT COALESCE(jsonb_agg(to_jsonb(t)), '[]'::jsonb) FROM cleanup_schedules t)
        ) AS "export!"
        "#
    )
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)?;
    strip_secrets(&mut export);
    let mut document = serde_json::Map::new();
    document.insert("format".into(), Value::from("rudder-settings-export"));
    document.insert("version".into(), Value::from(1));
    document.insert("rudder_version".into(), Value::from(rudder_core::VERSION));
    document.insert("exported_at".into(), Value::from(Utc::now().to_rfc3339()));
    if let Value::Object(tables) = export {
        document.extend(tables);
    }
    Ok(Value::Object(document))
}

fn is_secret_key(key: &str) -> bool {
    key.ends_with("_enc")
        || key.ends_with("_hash")
        || matches!(key, "totp_secret" | "csrf_token" | "password" | "secret")
}

pub fn strip_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_secret_key(key));
            map.values_mut().for_each(strip_secrets);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_secrets),
        _ => {}
    }
}

/// libpq environment for a `postgres://` URL, so passwords never reach argv.
pub fn pg_env(database_url: &str) -> Result<Vec<(&'static str, String)>, Error> {
    let url = url::Url::parse(database_url)
        .map_err(|error| Error::Validation(format!("DATABASE_URL: {error}")))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(Error::Validation(
            "DATABASE_URL must be a postgres:// URL".into(),
        ));
    }
    let decode = |text: &str| {
        percent_encoding::percent_decode_str(text)
            .decode_utf8_lossy()
            .into_owned()
    };
    let mut env: Vec<(&'static str, String)> = Vec::new();
    let mut set = |name: &'static str, value: String| {
        env.retain(|(key, _)| *key != name);
        if !value.is_empty() {
            env.push((name, value));
        }
    };
    if let Some(host) = url.host_str() {
        set("PGHOST", host.trim_matches(['[', ']']).to_owned());
    }
    if let Some(port) = url.port() {
        set("PGPORT", port.to_string());
    }
    set("PGUSER", decode(url.username()));
    if let Some(password) = url.password() {
        set("PGPASSWORD", decode(password));
    }
    set("PGDATABASE", decode(url.path().trim_start_matches('/')));
    for (key, value) in url.query_pairs() {
        let name = match key.as_ref() {
            "host" => "PGHOST",
            "port" => "PGPORT",
            "user" => "PGUSER",
            "password" => "PGPASSWORD",
            "dbname" => "PGDATABASE",
            "sslmode" => "PGSSLMODE",
            "sslrootcert" => "PGSSLROOTCERT",
            "sslcert" => "PGSSLCERT",
            "sslkey" => "PGSSLKEY",
            _ => continue,
        };
        set(name, value.into_owned());
    }
    Ok(env)
}

async fn run_pg_tool(
    program: &Path,
    args: &[String],
    env: &[(&'static str, String)],
) -> Result<(), Error> {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string());
    let output = tokio::process::Command::new(program)
        .args(args)
        .env_remove("PGPASSFILE")
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Error::Node(format!(
                "{name} is not installed; install the PostgreSQL client tools or set its path"
            )),
            _ => Error::Node(format!("could not start {name}: {error}")),
        })?;
    if output.status.success() {
        return Ok(());
    }
    let secrets: Vec<&str> = env
        .iter()
        .filter(|(key, _)| *key == "PGPASSWORD")
        .map(|(_, value)| value.as_str())
        .collect();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = rudder_core::redact(stderr.trim(), &secrets);
    let tail: String = stderr
        .chars()
        .rev()
        .take(800)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Err(Error::Node(format!(
        "{name} failed ({}): {tail}",
        output.status
    )))
}

/// Private scratch directory, removed on drop.
struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn create(parent: &Path, prefix: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        let path = parent.join(format!("{prefix}-{}", Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        Ok(Self { path })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

fn io_error(context: &str) -> impl Fn(std::io::Error) -> Error + '_ {
    move |error| Error::Node(format!("{context}: {error}"))
}

pub(crate) async fn applied_migration(pool: &PgPool) -> Result<i64, Error> {
    sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(version), 0) AS "version!" FROM _sqlx_migrations WHERE success"#
    )
    .fetch_one(pool)
    .await
    .map_err(|error| Error::Node(format!("read schema version: {error}")))
}

/// Dump the panel and stream the uncompressed, unencrypted tar.
///
/// The dump is written to a private directory under the data directory first,
/// because tar needs each entry's size up front. The directory is removed once
/// the stream ends or is dropped.
pub async fn archive(
    pool: &PgPool,
    config: &Config,
    include_history: bool,
) -> Result<ByteStream, Error> {
    let work = WorkDir::create(&config.data_dir.join("tmp"), "panel-backup")
        .map_err(io_error("create a scratch directory"))?;
    let env = pg_env(&config.database_url)?;
    let mut args = vec![
        "--format=custom".to_owned(),
        "--no-owner".to_owned(),
        "--no-acl".to_owned(),
        format!("--file={}", work.file(DUMP).display()),
    ];
    if !include_history {
        args.extend(
            HISTORY_TABLES
                .iter()
                .map(|table| format!("--exclude-table-data={table}")),
        );
    }
    run_pg_tool(&config.pg_dump, &args, &env).await?;
    #[cfg(unix)]
    std::fs::set_permissions(work.file(DUMP), std::fs::Permissions::from_mode(0o600))
        .map_err(io_error("protect the dump"))?;
    let manifest = Manifest {
        format: FORMAT.into(),
        version: FORMAT_VERSION,
        rudder_version: rudder_core::VERSION.into(),
        created_at: Utc::now(),
        migration: applied_migration(pool).await?,
        includes_history: include_history,
    };
    let settings = settings_export(pool)
        .await
        .map_err(|error| Error::Node(error.public_message()))?;
    write_private(&work.file(MANIFEST), &to_json(&manifest)?)
        .map_err(io_error("write the manifest"))?;
    write_private(&work.file(SETTINGS), &to_json(&settings)?)
        .map_err(io_error("write the settings export"))?;
    let mut key = hex::encode(config.master_key());
    key.push('\n');
    write_private(&work.file(MASTER_KEY), key.as_bytes())
        .map_err(io_error("write the master key"))?;
    Ok(tar_stream(work))
}

fn to_json(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    serde_json::to_vec_pretty(value).map_err(|error| Error::Node(format!("serialize: {error}")))
}

fn tar_stream(work: WorkDir) -> ByteStream {
    let (tx, rx) = mpsc::channel(8);
    let error_tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let result = (|| -> std::io::Result<()> {
            let mut builder = tar::Builder::new(ChannelWriter::new(tx));
            builder.mode(tar::HeaderMode::Deterministic);
            for name in ENTRIES {
                builder.append_path_with_name(work.file(name), name)?;
            }
            builder.into_inner()?.flush()
        })();
        if let Err(error) = result {
            let _ = error_tx.blocking_send(Err(Error::Node(format!("panel archive: {error}"))));
        }
        drop(work);
    });
    Box::pin(tokio_stream_from(rx))
}

/// Compress and encrypt a panel archive for download or upload.
pub fn seal(archive: ByteStream, passphrase: &str) -> Result<ByteStream, Error> {
    validate_passphrase(passphrase)?;
    pipeline::encrypt(
        pipeline::compress(archive, Compression::Zstd, 3),
        passphrase,
    )
}

pub fn file_name(at: DateTime<Utc>) -> String {
    format!("rudder-panel-{}.tar.zst.enc", at.format("%Y%m%d-%H%M%S"))
}

/// Inputs of an offline restore.
pub struct RestoreOptions {
    pub archive: PathBuf,
    /// Needed when the archive is encrypted, which panel backups always are.
    pub passphrase: Option<String>,
    pub database_url: String,
    /// Where the master key from the backup is written, mode 0600.
    pub master_key_out: PathBuf,
    /// Replace a database that already has tables, and overwrite a different key file.
    pub force: bool,
    pub pg_restore: PathBuf,
    /// Highest migration this binary knows. Newer backups are refused.
    pub known_migration: i64,
}

impl std::fmt::Debug for RestoreOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestoreOptions")
            .field("archive", &self.archive)
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "[redacted]"),
            )
            .field("master_key_out", &self.master_key_out)
            .field("force", &self.force)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct RestoreReport {
    pub manifest: Manifest,
    pub replaced_existing: bool,
}

async fn open_plain(path: &Path, passphrase: Option<&str>) -> anyhow::Result<ByteStream> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let mut magic = [0u8; 8];
    let read = tokio::io::AsyncReadExt::read(&mut file, &mut magic).await?;
    tokio::io::AsyncSeekExt::rewind(&mut file).await?;
    let mut stream: ByteStream = Box::pin(
        ReaderStream::new(file).map_err(|error| Error::Node(format!("read archive: {error}"))),
    );
    if read == magic.len() && &magic == rudder_core::ENVELOPE_MAGIC {
        let passphrase = passphrase
            .filter(|value| !value.is_empty())
            .context("the backup is encrypted; provide its passphrase")?;
        stream = pipeline::decrypt(stream, passphrase);
    }
    let first = match stream.next().await {
        Some(Ok(chunk)) => chunk,
        Some(Err(error)) => return Err(map_stream_error(error)),
        None => bail!("the backup file is empty"),
    };
    let compression = if first.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Compression::Zstd
    } else if first.starts_with(&[0x1f, 0x8b]) {
        Compression::Gzip
    } else {
        Compression::None
    };
    let rest = futures_util::stream::once(async move { Ok(first) }).chain(stream);
    Ok(pipeline::decompress(Box::pin(rest), compression))
}

fn map_stream_error(error: Error) -> anyhow::Error {
    match error {
        Error::Crypto => anyhow::anyhow!("the passphrase is wrong or the backup is damaged"),
        other => anyhow::anyhow!(other.to_string()),
    }
}

/// Unpack the four known entries. Anything else is refused.
async fn extract(stream: ByteStream, work: &WorkDir) -> anyhow::Result<()> {
    let reader = SyncIoBridge::new(StreamReader::new(
        stream.map_err(|error| std::io::Error::other(error.to_string())),
    ));
    let dir = work.path.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut archive = tar::Archive::new(reader);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_string_lossy().into_owned();
            let name = path.trim_start_matches("./");
            if !ENTRIES.contains(&name) || !entry.header().entry_type().is_file() {
                bail!("unexpected entry '{path}' in the backup");
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let mut file = opts
                .open(dir.join(name))
                .with_context(|| format!("unpack {name}"))?;
            std::io::copy(&mut entry, &mut file).with_context(|| format!("unpack {name}"))?;
        }
        Ok(())
    })
    .await
    .context("unpack task")?
    .map_err(|error| {
        let text = format!("{error:#}");
        if text.contains("crypto") {
            anyhow::anyhow!("the passphrase is wrong or the backup is damaged")
        } else {
            error
        }
    })
}

fn parse_key(text: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(text.trim()).context("master.key is not hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("master.key is not 32 bytes"))
}

fn write_key(path: &Path, key: &[u8; 32]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
    let mut text = hex::encode(key);
    text.push('\n');
    write_private(&temp, text.as_bytes()).with_context(|| format!("write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn existing_key(path: &Path) -> anyhow::Result<Option<[u8; 32]>> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => Ok(Some(bytes.try_into().expect("32 bytes"))),
        Ok(bytes) => parse_key(&String::from_utf8_lossy(&bytes)).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Restore a panel backup into an empty database and write its master key.
///
/// The database is restored in one transaction, so a failure leaves it as it was.
/// Start the panel afterwards; it applies newer migrations and republishes every
/// server's proxy configuration.
pub async fn restore(options: RestoreOptions) -> anyhow::Result<RestoreReport> {
    let work = WorkDir::create(&std::env::temp_dir(), "rudder-restore")
        .context("create a scratch directory")?;
    let stream = open_plain(&options.archive, options.passphrase.as_deref()).await?;
    extract(stream, &work).await?;
    for name in [MANIFEST, MASTER_KEY, DUMP] {
        if !work.file(name).exists() {
            bail!("the backup has no {name}; is it a panel backup?");
        }
    }
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(work.file(MANIFEST))?)
        .context("manifest.json is not valid")?;
    if manifest.format != FORMAT || manifest.version > FORMAT_VERSION {
        bail!(
            "unsupported backup format {} v{}",
            manifest.format,
            manifest.version
        );
    }
    if manifest.migration > options.known_migration {
        bail!(
            "the backup was taken by Rudder {} (schema {}), newer than this binary (schema {}); install that version and restore again",
            manifest.rudder_version,
            manifest.migration,
            options.known_migration
        );
    }
    let key = parse_key(&std::fs::read_to_string(work.file(MASTER_KEY))?)?;
    let current_key = existing_key(&options.master_key_out)?;
    if let Some(current) = current_key {
        if current != key && !options.force {
            bail!(
                "{} holds a different master key; pass --force to replace it",
                options.master_key_out.display()
            );
        }
    }

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&options.database_url)
        .await
        .context("connect to the database")?;
    let has_tables: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public')")
            .fetch_one(&pool)
            .await
            .context("inspect the database")?;
    if has_tables {
        if !options.force {
            bail!("the database already has tables; pass --force to replace them");
        }
        sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
            .execute(&pool)
            .await
            .context("clear the database")?;
    }
    let server_major = server_major(&pool).await?;
    pool.close().await;

    let env = pg_env(&options.database_url)?;
    let client_major = client_major(&options.pg_restore).await;
    // pg_restore 17 and newer emits `SET transaction_timeout = 0`, which
    // PostgreSQL 16 and older reject. Load the script with psql there and
    // drop that statement. The same client against PostgreSQL 17 keeps using
    // pg_restore directly, so the timeout stays disabled for a long restore.
    if server_major < 17 && client_major.is_some_and(|major| major >= 17) {
        load_dump_with_psql(&options.pg_restore, &work.file(DUMP), &env).await
    } else {
        let args = vec![
            "--no-owner".to_owned(),
            "--no-acl".to_owned(),
            "--exit-on-error".to_owned(),
            "--single-transaction".to_owned(),
            format!("--dbname={}", database_name(&env)),
            work.file(DUMP).display().to_string(),
        ];
        run_pg_tool(&options.pg_restore, &args, &env).await
    }
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // The key file may be a read-only mount; leave it alone when it already matches.
    if current_key != Some(key) {
        write_key(&options.master_key_out, &key)?;
    }
    Ok(RestoreReport {
        manifest,
        replaced_existing: has_tables,
    })
}

async fn server_major(pool: &PgPool) -> anyhow::Result<i64> {
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(pool)
        .await
        .context("read the PostgreSQL version")?;
    let numeric: i64 = version
        .trim()
        .parse()
        .context("PostgreSQL version is not a number")?;
    Ok(numeric / 10_000)
}

async fn client_major(program: &Path) -> Option<i64> {
    let output = tokio::process::Command::new(program)
        .arg("--version")
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .find_map(|word| word.split('.').next().and_then(|major| major.parse().ok()))
}

/// `psql` installed next to `pg_restore`, or `psql` on `PATH`.
fn psql_for(pg_restore: &Path) -> PathBuf {
    let sibling = match pg_restore.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("psql"),
        _ => PathBuf::from("psql"),
    };
    if sibling.exists() {
        sibling
    } else {
        PathBuf::from("psql")
    }
}

/// A restore script line that PostgreSQL 16 cannot execute.
///
/// The match is the whole statement, not a tab-separated `COPY` row, so a
/// stored value that mentions the setting is still restored.
fn skip_restore_line(line: &[u8]) -> bool {
    let trimmed = trim_ascii(line);
    if trimmed.contains(&b'\t') {
        return false;
    }
    if let Some(rest) = strip_prefix_ignore_ascii(trimmed, b"set transaction_timeout") {
        let rest = trim_ascii(rest);
        return rest.first() == Some(&b'=') || strip_prefix_ignore_ascii(rest, b"to").is_some();
    }
    strip_prefix_ignore_ascii(trimmed, b"\\restrict").is_some()
        || strip_prefix_ignore_ascii(trimmed, b"\\unrestrict").is_some()
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

fn strip_prefix_ignore_ascii<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if bytes.len() < prefix.len() {
        return None;
    }
    if !bytes[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    Some(&bytes[prefix.len()..])
}

async fn load_dump_with_psql(
    pg_restore: &Path,
    dump: &Path,
    env: &[(&'static str, String)],
) -> Result<(), Error> {
    let psql = psql_for(pg_restore);
    let mut restore = spawn_pg(
        pg_restore,
        &[
            "--no-owner".into(),
            "--no-acl".into(),
            "--file=-".into(),
            dump.display().to_string(),
        ],
        env,
        true,
    )?;
    let mut load = spawn_pg(
        &psql,
        &[
            "--set=ON_ERROR_STOP=1".into(),
            "--single-transaction".into(),
        ],
        env,
        false,
    )?;
    let script = restore.stdout.take().expect("pg_restore stdout");
    let sql = load.stdin.take().expect("psql stdin");
    let restore_err = restore.stderr.take().expect("pg_restore stderr");
    let load_err = load.stderr.take().expect("psql stderr");
    let (copied, restore_stderr, load_stderr) = tokio::join!(
        filter_restore_script(script, sql),
        read_to_end(restore_err),
        read_to_end(load_err),
    );
    let restore_status = restore
        .wait()
        .await
        .map_err(|error| Error::Node(format!("pg_restore failed to exit: {error}")))?;
    let load_status = load
        .wait()
        .await
        .map_err(|error| Error::Node(format!("psql failed to exit: {error}")))?;
    if let Err(error) = copied {
        return Err(Error::Node(format!(
            "could not filter the restore script: {error}"
        )));
    }
    if !restore_status.success() {
        return Err(tool_failure(
            "pg_restore",
            restore_status,
            &restore_stderr,
            env,
        ));
    }
    if !load_status.success() {
        return Err(tool_failure("psql", load_status, &load_stderr, env));
    }
    Ok(())
}

fn spawn_pg(
    program: &Path,
    args: &[String],
    env: &[(&'static str, String)],
    capture_stdout: bool,
) -> Result<tokio::process::Child, Error> {
    let name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string());
    tokio::process::Command::new(program)
        .args(args)
        .env_remove("PGPASSFILE")
        .envs(env.iter().map(|(key, value)| (*key, value.as_str())))
        .stdin(if capture_stdout {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Error::Node(format!(
                "{name} is not installed; install the PostgreSQL client tools or set its path"
            )),
            _ => Error::Node(format!("could not start {name}: {error}")),
        })
}

async fn filter_restore_script(
    script: impl tokio::io::AsyncRead + Unpin,
    mut sql: impl tokio::io::AsyncWrite + Unpin,
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut reader = tokio::io::BufReader::new(script);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            break;
        }
        if skip_restore_line(&line) {
            continue;
        }
        sql.write_all(&line).await?;
    }
    sql.shutdown().await
}

async fn read_to_end(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = tokio::io::copy(&mut pipe, &mut buf).await;
    buf
}

fn tool_failure(
    name: &str,
    status: std::process::ExitStatus,
    stderr: &[u8],
    env: &[(&'static str, String)],
) -> Error {
    let secrets: Vec<&str> = env
        .iter()
        .filter(|(key, _)| *key == "PGPASSWORD")
        .map(|(_, value)| value.as_str())
        .collect();
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = rudder_core::redact(stderr.trim(), &secrets);
    let tail: String = stderr
        .chars()
        .rev()
        .take(800)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Error::Node(format!("{name} failed ({status}): {tail}"))
}

fn database_name(env: &[(&'static str, String)]) -> String {
    env.iter()
        .find(|(key, _)| *key == "PGDATABASE")
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| "postgres".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_secret_columns_at_any_depth() {
        let mut value = json!({
            "users": [{ "email": "a@b.c", "password_hash": "x", "totp_secret": "y" }],
            "env_vars": [{ "key": "TOKEN", "value_enc": "z" }],
            "services": [{ "spec": { "registry": { "password_enc": "q", "user": "u" } } }],
            "sessions": { "csrf_token": "t" }
        });
        strip_secrets(&mut value);
        assert_eq!(
            value,
            json!({
                "users": [{ "email": "a@b.c" }],
                "env_vars": [{ "key": "TOKEN" }],
                "services": [{ "spec": { "registry": { "user": "u" } } }],
                "sessions": {}
            })
        );
    }

    #[test]
    fn database_urls_become_libpq_environment() {
        let env =
            pg_env("postgres://rud%40der:p%2Fss@db.internal:6543/rudder?sslmode=require").unwrap();
        assert_eq!(
            env,
            vec![
                ("PGHOST", "db.internal".to_owned()),
                ("PGPORT", "6543".to_owned()),
                ("PGUSER", "rud@der".to_owned()),
                ("PGPASSWORD", "p/ss".to_owned()),
                ("PGDATABASE", "rudder".to_owned()),
                ("PGSSLMODE", "require".to_owned()),
            ]
        );
        let socket = pg_env("postgresql:///rudder?host=/run/postgresql&user=me").unwrap();
        assert!(socket.contains(&("PGHOST", "/run/postgresql".to_owned())));
        assert!(socket.contains(&("PGUSER", "me".to_owned())));
        assert!(pg_env("mysql://x@y/z").is_err());
    }

    #[test]
    fn passphrases_need_twelve_characters() {
        assert!(validate_passphrase("short").is_err());
        assert!(validate_passphrase("twelve chars").is_ok());
    }

    #[test]
    fn restore_script_drops_only_the_incompatible_statements() {
        assert!(skip_restore_line(b"SET transaction_timeout = 0;\n"));
        assert!(skip_restore_line(b"  set transaction_timeout TO 0;\r\n"));
        assert!(skip_restore_line(b"\\restrict abc123\n"));
        assert!(skip_restore_line(b"\\unrestrict abc123\n"));
        assert!(!skip_restore_line(b"SET statement_timeout = 0;\n"));
        assert!(!skip_restore_line(b"SET transaction_timeout = 0;\tkept\n"));
        assert!(!skip_restore_line(
            b"INSERT INTO notes VALUES ('SET transaction_timeout = 0;');\n"
        ));
    }

    #[tokio::test]
    async fn old_server_restore_skips_transaction_timeout() {
        let Ok(admin_url) = std::env::var("DATABASE_URL") else {
            return;
        };
        if admin_url.is_empty() {
            return;
        }
        let name = format!("rudder_to_{}", Uuid::new_v4().simple());
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&admin_url)
            .await
            .expect("connect");
        sqlx::raw_sql(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create database");
        let _cleanup = TempDb {
            admin: admin_url.clone(),
            name: name.clone(),
        };
        let mut url = url::Url::parse(&admin_url).unwrap();
        url.set_path(&name);
        let script = std::env::temp_dir().join(format!(
            "rudder-fake-pg-restore-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \\\n  'SET transaction_timeout = 0;' \\\n  '\\restrict tok' \\\n  'CREATE TABLE probe (n int);' \\\n  'INSERT INTO probe VALUES (7);' \\\n  '\\unrestrict tok'\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = pg_env(url.as_str()).unwrap();
        load_dump_with_psql(&script, &script, &env)
            .await
            .expect("filtered restore");
        let restored = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(url.as_str())
            .await
            .unwrap();
        let value: i32 = sqlx::query_scalar("SELECT n FROM probe")
            .fetch_one(&restored)
            .await
            .unwrap();
        assert_eq!(value, 7);
        let _ = std::fs::remove_file(&script);
    }

    struct TempDb {
        admin: String,
        name: String,
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::process::Command::new("psql")
                .arg(&self.admin)
                .args([
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-c",
                    &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name),
                ])
                .output();
        }
    }
}
