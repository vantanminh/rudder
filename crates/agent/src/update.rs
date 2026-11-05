//! Agent self-update: install a binary streamed by the panel, restart, and roll
//! back if the new binary cannot reach the panel.
//!
//! The panel verified the binary against the signed release before sending it;
//! the agent checks the SHA-256 again and that the binary runs and reports the
//! expected version. The old binary is kept as `<name>.prev` and an
//! `update-pending` marker is written. After the restart, [`watchdog`] removes
//! the marker once a session is welcomed, or restores `.prev` if none is within
//! the grace period.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use rudder_core::{ByteStream, Error};
use sha2::Digest;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

pub const MARKER: &str = "update-pending";
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// Where the running binary lives and how to restart it.
#[derive(Clone)]
pub struct SelfUpdate {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
    /// Exit so the service manager starts the new binary.
    pub restart: Arc<dyn Fn() + Send + Sync>,
    pub grace: Duration,
}

impl std::fmt::Debug for SelfUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelfUpdate")
            .field("binary", &self.binary)
            .field("data_dir", &self.data_dir)
            .field("grace", &self.grace)
            .finish_non_exhaustive()
    }
}

/// Target triple of the release build that matches this machine.
pub fn target() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

fn sibling(binary: &Path, suffix: &str) -> PathBuf {
    let name = binary
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rudder-agent".into());
    binary.with_file_name(format!("{name}.{suffix}"))
}

pub fn previous_path(binary: &Path) -> PathBuf {
    sibling(binary, "prev")
}

/// Write, check, and swap in a new binary. The caller restarts afterwards.
pub async fn install(
    update: &SelfUpdate,
    version: &str,
    sha256: &str,
    mut input: ByteStream,
) -> Result<(), Error> {
    rudder_core::parse_version(version)?;
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Validation("invalid agent checksum".into()));
    }
    let staged = sibling(&update.binary, "new");
    let io =
        |what: &'static str| move |error: std::io::Error| Error::Node(format!("{what}: {error}"));
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o755);
    let mut file = opts
        .open(&staged)
        .await
        .map_err(io("stage the new agent"))?;
    let mut hasher = sha2::Sha256::new();
    let mut written = 0_u64;
    let outcome = async {
        while let Some(chunk) = input.next().await {
            let chunk = chunk?;
            written += chunk.len() as u64;
            if written > MAX_BINARY_BYTES {
                return Err(Error::Validation("agent binary is too large".into()));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(io("write the new agent"))?;
        }
        file.sync_all().await.map_err(io("write the new agent"))?;
        Ok(())
    }
    .await;
    // Tokio may still hold a cloned writable descriptor on its blocking pool.
    // Wait for it before executing the staged binary (Linux returns ETXTBSY
    // while any writable descriptor is open).
    drop(file.into_std().await);
    let cleanup = |error: Error| {
        let _ = std::fs::remove_file(&staged);
        error
    };
    outcome.map_err(cleanup)?;
    if !hex::encode(hasher.finalize()).eq_ignore_ascii_case(sha256) {
        return Err(cleanup(Error::Validation(
            "agent binary does not match its checksum".into(),
        )));
    }
    #[cfg(unix)]
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(io("mark the new agent executable"))
        .map_err(cleanup)?;
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(&staged)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| cleanup(Error::Node("the new agent did not answer --version".into())))?
    .map_err(|error| cleanup(Error::Node(format!("the new agent does not run: {error}"))))?;
    let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let expected = format!("rudder-agent {version}");
    if !output.status.success() || reported != expected {
        return Err(cleanup(Error::Node(format!(
            "the new agent reports '{reported}', expected '{expected}'"
        ))));
    }
    let previous = previous_path(&update.binary);
    std::fs::rename(&update.binary, &previous)
        .map_err(io("keep the current agent"))
        .map_err(cleanup)?;
    if let Err(error) = std::fs::rename(&staged, &update.binary) {
        let _ = std::fs::rename(&previous, &update.binary);
        return Err(cleanup(Error::Node(format!(
            "install the new agent: {error}"
        ))));
    }
    write_marker(&update.data_dir, version).map_err(io("record the pending update"))?;
    tracing::info!(%version, binary = %update.binary.display(), "agent binary replaced; restarting");
    Ok(())
}

fn write_marker(data_dir: &Path, version: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(data_dir.join(MARKER), format!("{version}\n"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogOutcome {
    NothingPending,
    Confirmed,
    RolledBack,
    /// The grace period passed but there was no previous binary to restore.
    NoPrevious,
}

/// After an update, wait for a welcomed session; otherwise restore the old binary.
pub async fn watchdog(update: SelfUpdate, mut connected: watch::Receiver<bool>) -> WatchdogOutcome {
    let marker = update.data_dir.join(MARKER);
    if !marker.exists() {
        return WatchdogOutcome::NothingPending;
    }
    let welcomed = tokio::time::timeout(update.grace, connected.wait_for(|up| *up)).await;
    if matches!(welcomed, Ok(Ok(_))) {
        let _ = std::fs::remove_file(&marker);
        tracing::info!(
            version = rudder_core::VERSION,
            "agent update confirmed by the panel"
        );
        return WatchdogOutcome::Confirmed;
    }
    let previous = previous_path(&update.binary);
    let _ = std::fs::remove_file(&marker);
    if !previous.exists() {
        tracing::error!("the updated agent cannot reach the panel and no previous binary exists");
        return WatchdogOutcome::NoPrevious;
    }
    match std::fs::rename(&previous, &update.binary) {
        Ok(()) => {
            tracing::error!(
                grace_secs = update.grace.as_secs(),
                "the updated agent did not reach the panel; restored the previous binary"
            );
            (update.restart)();
            WatchdogOutcome::RolledBack
        }
        Err(error) => {
            tracing::error!(%error, "cannot restore the previous agent binary");
            WatchdogOutcome::NoPrevious
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho rudder-agent {version}\n").into_bytes()
    }

    fn stream(bytes: Vec<u8>) -> ByteStream {
        Box::pin(futures_util::stream::once(
            async move { Ok(Bytes::from(bytes)) },
        ))
    }

    fn setup(grace: Duration) -> (tempfile::TempDir, SelfUpdate, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("rudder-agent");
        std::fs::write(&binary, script("0.1.0")).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let restarts = Arc::new(AtomicUsize::new(0));
        let counter = restarts.clone();
        let update = SelfUpdate {
            binary,
            data_dir: dir.path().join("data"),
            restart: Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
            grace,
        };
        (dir, update, restarts)
    }

    #[tokio::test]
    async fn install_checks_hash_and_version_then_swaps() {
        let (_dir, update, _) = setup(Duration::from_secs(1));
        let new = script("0.2.0");
        let hash = rudder_core::sha256_hex(&new);

        let wrong = install(&update, "0.2.0", &"0".repeat(64), stream(new.clone())).await;
        assert!(wrong.unwrap_err().to_string().contains("checksum"));
        let mismatch = install(&update, "0.3.0", &hash, stream(new.clone())).await;
        assert!(mismatch
            .unwrap_err()
            .to_string()
            .contains("expected 'rudder-agent 0.3.0'"));
        assert!(!sibling(&update.binary, "new").exists());
        assert_eq!(std::fs::read(&update.binary).unwrap(), script("0.1.0"));

        install(&update, "0.2.0", &hash, stream(new.clone()))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&update.binary).unwrap(), new);
        assert_eq!(
            std::fs::read(previous_path(&update.binary)).unwrap(),
            script("0.1.0")
        );
        assert!(update.data_dir.join(MARKER).exists());
    }

    #[tokio::test]
    async fn watchdog_confirms_on_welcome_and_rolls_back_otherwise() {
        let (_dir, update, restarts) = setup(Duration::from_millis(300));
        let (_tx, rx) = watch::channel(false);
        assert_eq!(
            watchdog(update.clone(), rx).await,
            WatchdogOutcome::NothingPending
        );

        let new = script("0.2.0");
        install(
            &update,
            "0.2.0",
            &rudder_core::sha256_hex(&new),
            stream(new.clone()),
        )
        .await
        .unwrap();
        let (tx, rx) = watch::channel(false);
        tx.send(true).unwrap();
        assert_eq!(
            watchdog(update.clone(), rx).await,
            WatchdogOutcome::Confirmed
        );
        assert!(!update.data_dir.join(MARKER).exists());
        assert_eq!(restarts.load(Ordering::SeqCst), 0);

        write_marker(&update.data_dir, "0.2.0").unwrap();
        let (_tx, rx) = watch::channel(false);
        assert_eq!(
            watchdog(update.clone(), rx).await,
            WatchdogOutcome::RolledBack
        );
        assert_eq!(std::fs::read(&update.binary).unwrap(), script("0.1.0"));
        assert!(!previous_path(&update.binary).exists());
        assert_eq!(restarts.load(Ordering::SeqCst), 1);
    }
}
