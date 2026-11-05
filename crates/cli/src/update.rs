//! Native self-update (`rudder update` / `rudder self-update`) across macOS, Windows, and Linux.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use rudder_core::{parse_sha256sums, parse_version, verify_minisign, VERSION};

const DEFAULT_GITHUB_REPO: &str = "vantanminh/rudder";

fn is_newer(current: &str, latest: &str) -> Result<bool, rudder_core::Error> {
    Ok(parse_version(latest)? > parse_version(current)?)
}
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub fn current_target_asset() -> anyhow::Result<String> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn asset_name_for(os: &str, arch: &str) -> anyhow::Result<String> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("rudder-x86_64-unknown-linux-musl".into()),
        ("linux", "aarch64") => Ok("rudder-aarch64-unknown-linux-musl".into()),
        ("macos", "x86_64") => Ok("rudder-x86_64-apple-darwin".into()),
        ("macos", "aarch64") => Ok("rudder-aarch64-apple-darwin".into()),
        ("windows", "x86_64") => Ok("rudder-x86_64-pc-windows-msvc.exe".into()),
        ("windows", "aarch64") => Ok("rudder-aarch64-pc-windows-msvc.exe".into()),
        _ => bail!("unsupported OS/architecture for native update: {os}/{arch}"),
    }
}

pub async fn run(args: Vec<String>, out: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let mut check_only = false;
    let mut repo = std::env::var("RUDDER_GITHUB_REPO")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_owned());
    let mut api_url = std::env::var("RUDDER_GITHUB_API_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_owned());
    let mut pubkey = std::env::var("RUDDER_UPDATE_PUBKEY")
        .ok()
        .filter(|v| !v.is_empty());
    let mut target_path = std::env::var("RUDDER_UPDATE_TARGET_PATH")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let mut current_version = VERSION.to_owned();

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--check" => check_only = true,
            "--yes" | "-y" => {}
            "--repo" => repo = iter.next().context("--repo needs OWNER/REPO")?,
            "--api-url" => api_url = iter.next().context("--api-url needs a URL")?,
            "--pubkey" => pubkey = Some(iter.next().context("--pubkey needs a value")?),
            "--target-path" => {
                target_path = Some(PathBuf::from(
                    iter.next().context("--target-path needs a path")?,
                ))
            }
            "--current-version" => {
                current_version = iter.next().context("--current-version needs a version")?
            }
            other => {
                bail!("unknown option {other}; usage: rudder update [--check] [--repo OWNER/REPO]")
            }
        }
    }

    let http = reqwest::Client::builder()
        .user_agent(format!("rudder-cli/{VERSION}"))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?;

    let release_url = format!(
        "{}/repos/{}/releases/latest",
        api_url.trim_end_matches('/'),
        repo.trim_matches('/')
    );
    let response = http
        .get(&release_url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("fetch latest release from {release_url}"))?;
    if !response.status().is_success() {
        bail!(
            "GitHub release check failed with status {} at {release_url}",
            response.status()
        );
    }
    let release: GithubRelease = response.json().await.context("parse GitHub release JSON")?;
    let latest_version = release.tag_name.trim_start_matches('v');

    if !is_newer(&current_version, latest_version).map_err(|e| anyhow!(e.to_string()))? {
        writeln!(
            out,
            "rudder {current_version} is already up to date (latest: {latest_version})."
        )?;
        return Ok(());
    }

    if check_only {
        writeln!(
            out,
            "Update available: rudder {current_version} -> {latest_version} (run `rudder update` to install)"
        )?;
        return Ok(());
    }

    let asset_name = current_target_asset()?;
    let binary_asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| {
            anyhow!(
                "release {} does not contain native binary asset '{asset_name}'",
                release.tag_name
            )
        })?;
    let sums_asset = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .ok_or_else(|| anyhow!("release {} is missing SHA256SUMS", release.tag_name))?;

    writeln!(
        out,
        "Updating rudder {current_version} -> {latest_version} ({asset_name})..."
    )?;

    let sums_bytes = http
        .get(&sums_asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let sums_text = std::str::from_utf8(&sums_bytes).context("SHA256SUMS is not UTF-8")?;

    if let Some(pk) = pubkey.as_deref() {
        let sig_asset = release
            .assets
            .iter()
            .find(|a| a.name == "SHA256SUMS.minisig")
            .ok_or_else(|| anyhow!("release {} is missing SHA256SUMS.minisig", release.tag_name))?;
        let sig_bytes = http
            .get(&sig_asset.browser_download_url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let sig_text =
            std::str::from_utf8(&sig_bytes).context("SHA256SUMS.minisig is not UTF-8")?;
        verify_minisign(pk, &sums_bytes, sig_text)
            .map_err(|err| anyhow!("minisign verification failed: {err}"))?;
    }

    let checksums = parse_sha256sums(sums_text).map_err(|err| anyhow!(err.to_string()))?;
    let expected_sha = checksums
        .get(&asset_name)
        .ok_or_else(|| anyhow!("SHA256SUMS does not list a checksum for '{asset_name}'"))?;

    let bin_bytes = http
        .get(&binary_asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let actual_sha = hex::encode(Sha256::digest(&bin_bytes));
    if !actual_sha.eq_ignore_ascii_case(expected_sha) {
        bail!("SHA-256 mismatch for {asset_name}: expected {expected_sha}, got {actual_sha}");
    }

    let dest = match target_path {
        Some(path) => path,
        None => std::env::current_exe().context("locate current rudder executable")?,
    };
    replace_executable(&dest, &bin_bytes)?;

    writeln!(
        out,
        "Updated rudder to {latest_version} at {}",
        dest.display()
    )?;
    Ok(())
}

fn replace_executable(dest: &Path, new_bytes: &[u8]) -> anyhow::Result<()> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rudder".into());
    let staged = parent.join(format!("{file_name}.new"));
    let backup = parent.join(format!("{file_name}.old"));

    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_file(&backup);

    {
        use std::io::Write as _;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o755)
                .open(&staged)
                .with_context(|| format!("create {}", staged.display()))?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .with_context(|| format!("create {}", staged.display()))?;
        file.write_all(new_bytes)?;
        file.sync_all()?;
    }

    if dest.exists() {
        std::fs::rename(dest, &backup).with_context(|| {
            format!(
                "move current binary {} -> {}",
                dest.display(),
                backup.display()
            )
        })?;
    }
    if let Err(err) = std::fs::rename(&staged, dest) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, dest);
        }
        return Err(err).with_context(|| format!("install new binary at {}", dest.display()));
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_os_and_arch_to_release_asset_names() {
        assert_eq!(
            asset_name_for("linux", "x86_64").unwrap(),
            "rudder-x86_64-unknown-linux-musl"
        );
        assert_eq!(
            asset_name_for("macos", "aarch64").unwrap(),
            "rudder-aarch64-apple-darwin"
        );
        assert_eq!(
            asset_name_for("windows", "x86_64").unwrap(),
            "rudder-x86_64-pc-windows-msvc.exe"
        );
        assert!(asset_name_for("freebsd", "x86_64").is_err());
    }
}
