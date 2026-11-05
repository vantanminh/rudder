//! Local Rudder Image store on the developer's machine (`~/.config/rudder/images/<service>/`),
//! local CI checks runner, and Docker build/save compressor.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context as _};
use chrono::{DateTime, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use rudder_core::{select_images_to_prune, validate_image_tag, ImageVersionEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalImageRecord {
    pub tag: String,
    pub service: String,
    pub image_ref: String,
    pub git_sha: Option<String>,
    pub archive_file: String,
    pub compression: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalIndex {
    images: Vec<LocalImageRecord>,
}

pub fn sanitize_service_dir(service: &str) -> String {
    service
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn service_dir(images_root: &Path, service: &str) -> PathBuf {
    images_root.join(sanitize_service_dir(service))
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

pub fn load_local_images(
    images_root: &Path,
    service: &str,
) -> anyhow::Result<Vec<LocalImageRecord>> {
    let path = index_path(&service_dir(images_root, service));
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mut index: LocalIndex = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            index
                .images
                .sort_by_key(|b| std::cmp::Reverse(b.created_at));
            Ok(index.images)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn save_local_images(
    images_root: &Path,
    service: &str,
    mut images: Vec<LocalImageRecord>,
) -> anyhow::Result<()> {
    let dir = service_dir(images_root, service);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    images.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    let path = index_path(&dir);
    let payload = serde_json::to_vec_pretty(&LocalIndex { images })?;
    std::fs::write(&path, payload).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn archive_path(images_root: &Path, service: &str, record: &LocalImageRecord) -> PathBuf {
    service_dir(images_root, service).join(&record.archive_file)
}

pub fn default_tag(git_sha: Option<&str>) -> String {
    let ts = Utc::now().format("%Y%m%d-%H%M%S");
    match git_sha.and_then(|s| s.get(..7)) {
        Some(short) => format!("{ts}-{short}"),
        None => ts.to_string(),
    }
}

pub fn detect_git_sha(context_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(context_dir)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!sha.is_empty()).then_some(sha)
}

pub fn run_local_checks(
    checks: &[String],
    workdir: &Path,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    for check in checks {
        let trimmed = check.trim();
        if trimmed.is_empty() {
            continue;
        }
        writeln!(out, "==> Running check: {trimmed}")?;
        let status = run_shell_command(trimmed, workdir)?;
        if !status.success() {
            bail!("local CI check failed: `{trimmed}` (exit {})", status);
        }
    }
    Ok(())
}

fn run_shell_command(cmd: &str, workdir: &Path) -> anyhow::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    let mut child = Command::new("cmd")
        .args(["/C", cmd])
        .current_dir(workdir)
        .status()
        .with_context(|| format!("execute `{cmd}`"))?;
    #[cfg(not(windows))]
    let mut child = Command::new("sh")
        .args(["-c", cmd])
        .current_dir(workdir)
        .status()
        .with_context(|| format!("execute `{cmd}`"))?;
    let _ = &mut child;
    Ok(child)
}

pub struct BuildParams<'a> {
    pub images_root: &'a Path,
    pub service: &'a str,
    pub tag: &'a str,
    pub dockerfile: &'a str,
    pub context_dir: &'a Path,
    pub checks: &'a [String],
    pub skip_checks: bool,
    pub local_keep_count: Option<u32>,
}

pub fn build_and_store_local(
    params: &BuildParams<'_>,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<(LocalImageRecord, Vec<String>)> {
    let tag = validate_image_tag(params.tag)?;
    if !params.skip_checks && !params.checks.is_empty() {
        run_local_checks(params.checks, params.context_dir, out)?;
    }

    let git_sha = detect_git_sha(params.context_dir);
    let short_service = params.service.rsplit('/').next().unwrap_or(params.service);
    let image_ref = format!("rudder-image/{short_service}:{tag}");

    let dir = service_dir(params.images_root, params.service);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let archive_file = format!("{tag}.tar.gz");
    let dest_path = dir.join(&archive_file);

    writeln!(out, "==> Building Docker image {image_ref}")?;

    let (size_bytes, sha256) = if let Ok(fake_payload) = std::env::var("RUDDER_FAKE_DOCKER_ARCHIVE")
    {
        write_gzip_and_hash(fake_payload.as_bytes(), &dest_path)?
    } else {
        let build_status = Command::new("docker")
            .args([
                "build",
                "-f",
                params.dockerfile,
                "-t",
                &image_ref,
                &params.context_dir.display().to_string(),
            ])
            .status()
            .context("run `docker build` (is Docker installed and running?)")?;
        if !build_status.success() {
            bail!("`docker build` failed with status {build_status}");
        }

        writeln!(out, "==> Exporting and compressing {image_ref}")?;
        let mut save_child = Command::new("docker")
            .args(["save", &image_ref])
            .stdout(Stdio::piped())
            .spawn()
            .context("run `docker save`")?;
        let mut stdout = save_child
            .stdout
            .take()
            .context("capture `docker save` stdout")?;
        let file = std::fs::File::create(&dest_path)
            .with_context(|| format!("create {}", dest_path.display()))?;
        let mut gz = GzEncoder::new(file, Compression::default());
        std::io::copy(&mut stdout, &mut gz).context("compress `docker save` output")?;
        let file = gz.finish().context("finish gzip archive")?;
        file.sync_all()?;
        let save_status = save_child.wait()?;
        if !save_status.success() {
            let _ = std::fs::remove_file(&dest_path);
            bail!("`docker save` failed with status {save_status}");
        }
        hash_file(&dest_path)?
    };

    let record = LocalImageRecord {
        tag: tag.clone(),
        service: params.service.to_owned(),
        image_ref,
        git_sha,
        archive_file,
        compression: "gzip".into(),
        size_bytes,
        sha256,
        created_at: Utc::now(),
    };

    let mut images = load_local_images(params.images_root, params.service)?;
    images.retain(|item| item.tag != record.tag);
    images.insert(0, record.clone());

    let pruned = prune_records(&dir, &mut images, params.local_keep_count);
    save_local_images(params.images_root, params.service, images)?;

    writeln!(
        out,
        "Built {} ({} bytes, sha256 {}) -> {}",
        record.image_ref,
        record.size_bytes,
        &record.sha256[..12],
        dest_path.display()
    )?;
    if !pruned.is_empty() {
        writeln!(out, "Pruned local archives: {}", pruned.join(", "))?;
    }

    Ok((record, pruned))
}

pub fn prune_local_store(
    images_root: &Path,
    service: &str,
    keep_count: u32,
) -> anyhow::Result<Vec<String>> {
    let dir = service_dir(images_root, service);
    let mut images = load_local_images(images_root, service)?;
    let pruned = prune_records(&dir, &mut images, Some(keep_count.max(1)));
    save_local_images(images_root, service, images)?;
    Ok(pruned)
}

fn prune_records(
    dir: &Path,
    images: &mut Vec<LocalImageRecord>,
    keep_count: Option<u32>,
) -> Vec<String> {
    let Some(keep) = keep_count.filter(|v| *v >= 1) else {
        return Vec::new();
    };
    let entries: Vec<ImageVersionEntry> = images
        .iter()
        .map(|item| ImageVersionEntry {
            id: item.tag.clone(),
            tag: item.tag.clone(),
            created_at: item.created_at,
        })
        .collect();
    let to_prune = select_images_to_prune(&entries, Some(keep), None, Utc::now());
    if to_prune.is_empty() {
        return Vec::new();
    }
    let mut removed = Vec::new();
    images.retain(|item| {
        if to_prune.contains(&item.tag) {
            let _ = std::fs::remove_file(dir.join(&item.archive_file));
            removed.push(item.tag.clone());
            false
        } else {
            true
        }
    });
    removed
}

fn write_gzip_and_hash(raw: &[u8], dest: &Path) -> anyhow::Result<(u64, String)> {
    let file = std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut gz = GzEncoder::new(file, Compression::default());
    gz.write_all(raw)?;
    let file = gz.finish()?;
    file.sync_all()?;
    hash_file(dest)
}

fn hash_file(path: &Path) -> anyhow::Result<(u64, String)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}
