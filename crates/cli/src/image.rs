//! Local Rudder Image store on the developer's machine (`~/.config/rudder/images/<service>/`),
//! local CI checks runner, and Docker build/save compressor.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{bail, Context as _};
use chrono::{DateTime, SecondsFormat, Utc};
use flate2::write::GzEncoder;
use flate2::Compression;
use rudder_core::{select_images_to_prune_protecting, validate_image_tag, ImageVersionEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Threshold (200 MiB) above which `rudder build` warns about a large Docker build context.
pub const CONTEXT_WARN_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalImageRecord {
    pub tag: String,
    pub service: String,
    pub image_ref: String,
    pub git_sha: Option<String>,
    #[serde(default)]
    pub image_id: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
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
    pub platform: Option<&'a str>,
    pub no_cache: bool,
    pub progress: Option<&'a str>,
    pub build_args: &'a BTreeMap<String, String>,
    pub protected_tag: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct ContextSummary {
    pub total_bytes: u64,
    pub file_count: u64,
    pub ignore_file: Option<PathBuf>,
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut val = bytes as f64;
    let mut idx = 0usize;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.1} {}", UNITS[idx])
    }
}

/// Resolve the Dockerfile path against `context_dir` if the direct path does not exist.
pub fn resolve_dockerfile_path(context_dir: &Path, dockerfile: &str) -> PathBuf {
    let direct = PathBuf::from(dockerfile);
    if direct.exists() {
        return direct;
    }
    let in_context = context_dir.join(dockerfile);
    if in_context.exists() {
        return in_context;
    }
    direct
}

/// Estimate the Docker build context size while respecting `<dockerfile>.dockerignore`
/// (BuildKit precedence) or `<context_dir>/.dockerignore`.
pub fn measure_build_context(context_dir: &Path, dockerfile: &Path) -> ContextSummary {
    let df_ignore = PathBuf::from(format!("{}.dockerignore", dockerfile.display()));
    let ctx_ignore = context_dir.join(".dockerignore");
    let ignore_file = if df_ignore.is_file() {
        Some(df_ignore)
    } else if ctx_ignore.is_file() {
        Some(ctx_ignore)
    } else {
        None
    };

    let rules = ignore_file
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| parse_dockerignore(&text))
        .unwrap_or_default();

    let mut total_bytes = 0u64;
    let mut file_count = 0u64;
    walk_context_dir(context_dir, "", &rules, &mut total_bytes, &mut file_count);

    ContextSummary {
        total_bytes,
        file_count,
        ignore_file,
    }
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    negated: bool,
    pattern: String,
}

fn parse_dockerignore(content: &str) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    for raw in content.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (negated, rest) = match trimmed.strip_prefix('!') {
            Some(r) => (true, r.trim()),
            None => (false, trimmed),
        };
        let normalized = rest
            .replace('\\', "/")
            .trim_start_matches("./")
            .trim_matches('/')
            .to_owned();
        if !normalized.is_empty() {
            rules.push(IgnoreRule {
                negated,
                pattern: normalized,
            });
        }
    }
    rules
}

fn is_path_ignored(rel_path: &str, rules: &[IgnoreRule]) -> bool {
    let mut ignored = false;
    for rule in rules {
        if dockerignore_matches(&rule.pattern, rel_path) {
            ignored = !rule.negated;
        }
    }
    ignored
}

fn has_negated_child_rule(rel_dir: &str, rules: &[IgnoreRule]) -> bool {
    let prefix = format!("{rel_dir}/");
    rules
        .iter()
        .any(|r| r.negated && (r.pattern.starts_with(&prefix) || r.pattern.starts_with("**")))
}

fn dockerignore_matches(pattern: &str, rel_path: &str) -> bool {
    if pattern == "*" || pattern == "**" || pattern == "**/*" {
        return true;
    }
    if rel_path == pattern || rel_path.starts_with(&format!("{pattern}/")) {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("**/") {
        if rel_path == suffix
            || rel_path.ends_with(&format!("/{suffix}"))
            || rel_path.contains(&format!("/{suffix}/"))
            || wildcard_match(suffix, rel_path.rsplit('/').next().unwrap_or(rel_path))
        {
            return true;
        }
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        if rel_path == prefix || rel_path.starts_with(&format!("{prefix}/")) {
            return true;
        }
    }
    wildcard_match(pattern, rel_path)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn walk_context_dir(
    root: &Path,
    rel_prefix: &str,
    rules: &[IgnoreRule],
    total_bytes: &mut u64,
    file_count: &mut u64,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let rel = if rel_prefix.is_empty() {
            name_str.into_owned()
        } else {
            format!("{rel_prefix}/{name_str}")
        };
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let ignored = is_path_ignored(&rel, rules);
        if ft.is_dir() {
            if ignored && !has_negated_child_rule(&rel, rules) {
                continue;
            }
            walk_context_dir(&entry.path(), &rel, rules, total_bytes, file_count);
        } else if ft.is_file() && !ignored {
            if let Ok(meta) = entry.metadata() {
                *total_bytes = total_bytes.saturating_add(meta.len());
                *file_count += 1;
            }
        }
    }
}

/// Construct the effective `--build-arg KEY=VAL` map, injecting `GIT_SHA` and `BUILD_TIME`
/// automatically unless explicitly overridden by the caller.
pub fn effective_build_args(
    git_sha: Option<&str>,
    build_time_rfc3339: &str,
    custom: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = BTreeMap::new();
    merged.insert("GIT_SHA".to_owned(), git_sha.unwrap_or("nogit").to_owned());
    merged.insert("BUILD_TIME".to_owned(), build_time_rfc3339.to_owned());
    for (k, v) in custom {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

pub fn build_and_store_local(
    params: &BuildParams<'_>,
    out: &mut (dyn Write + Send),
) -> anyhow::Result<(LocalImageRecord, Vec<String>)> {
    let started = Instant::now();
    let tag = validate_image_tag(params.tag)?;
    if !params.skip_checks && !params.checks.is_empty() {
        run_local_checks(params.checks, params.context_dir, out)?;
    }

    let git_sha = detect_git_sha(params.context_dir);
    if git_sha.is_none() {
        writeln!(
            out,
            "==> Note: '{}' is not a Git repository (or has no commits); falling back to GIT_SHA=nogit and tag {tag}",
            params.context_dir.display()
        )?;
    }

    let created_at = Utc::now();
    let build_time = created_at.to_rfc3339_opts(SecondsFormat::Secs, true);
    let build_args = effective_build_args(git_sha.as_deref(), &build_time, params.build_args);

    let short_service = params.service.rsplit('/').next().unwrap_or(params.service);
    let image_ref = format!("rudder-image/{short_service}:{tag}");

    let dir = service_dir(params.images_root, params.service);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let archive_file = format!("{tag}.tar.gz");
    let dest_path = dir.join(&archive_file);

    let resolved_dockerfile = resolve_dockerfile_path(params.context_dir, params.dockerfile);
    if params.context_dir.is_dir() {
        let ctx_summary = measure_build_context(params.context_dir, &resolved_dockerfile);
        let ignore_note = ctx_summary
            .ignore_file
            .as_ref()
            .map(|p| format!(", ignore: {}", p.display()))
            .unwrap_or_else(|| ", no .dockerignore found".to_owned());
        writeln!(
            out,
            "==> Build context: {} ({} files{ignore_note})",
            format_bytes(ctx_summary.total_bytes),
            ctx_summary.file_count
        )?;
        if ctx_summary.total_bytes > CONTEXT_WARN_BYTES {
            writeln!(
                out,
                "==> WARNING: build context is {} (> {}); add large directories (e.g. target/, node_modules/) to .dockerignore",
                format_bytes(ctx_summary.total_bytes),
                format_bytes(CONTEXT_WARN_BYTES)
            )?;
        }
    }

    if let Some(platform) = params.platform {
        writeln!(
            out,
            "==> Building Docker image {image_ref} (platform: {platform}, BuildKit enabled)"
        )?;
    } else {
        writeln!(
            out,
            "==> Building Docker image {image_ref} (BuildKit enabled)"
        )?;
    }

    let (size_bytes, sha256, image_id, detected_platform, uncompressed_bytes) = if let Ok(
        fake_payload,
    ) =
        std::env::var("RUDDER_FAKE_DOCKER_ARCHIVE")
    {
        let (sz, sha) = write_gzip_and_hash(fake_payload.as_bytes(), &dest_path)?;
        (sz, sha, None, params.platform.map(str::to_owned), None)
    } else {
        if !params.context_dir.exists() {
            bail!(
                "build context directory '{}' does not exist",
                params.context_dir.display()
            );
        }
        if !resolved_dockerfile.is_file() {
            bail!(
                    "Dockerfile '{}' was not found (checked '{}' and '{}'); verify `dockerfile` in rudder.toml or pass --dockerfile <path>",
                    params.dockerfile,
                    Path::new(params.dockerfile).display(),
                    params.context_dir.join(params.dockerfile).display()
                );
        }

        let mut cmd = Command::new("docker");
        cmd.env("DOCKER_BUILDKIT", "1");
        cmd.arg("build")
            .arg("-f")
            .arg(&resolved_dockerfile)
            .arg("-t")
            .arg(&image_ref);
        if let Some(platform) = params.platform.filter(|p| !p.trim().is_empty()) {
            cmd.args(["--platform", platform.trim()]);
        }
        if params.no_cache {
            cmd.arg("--no-cache");
        }
        if let Some(progress) = params.progress.filter(|p| !p.trim().is_empty()) {
            cmd.args(["--progress", progress.trim()]);
        }
        for (k, v) in &build_args {
            cmd.arg("--build-arg").arg(format!("{k}={v}"));
        }
        cmd.arg(params.context_dir);

        let build_status = cmd.status().map_err(|err| {
            anyhow::anyhow!(
                "could not start `docker build`: {err}. Is Docker installed and on your PATH?"
            )
        })?;
        if !build_status.success() {
            bail!(
                    "`docker build` failed with status {build_status}. Check the build output above, and confirm the Docker daemon (e.g. Docker Desktop) is running."
                );
        }

        let (image_id, detected_platform, uncompressed_bytes) = inspect_built_image(&image_ref);

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
        let (sz, sha) = hash_file(&dest_path)?;
        (
            sz,
            sha,
            image_id,
            detected_platform.or_else(|| params.platform.map(str::to_owned)),
            uncompressed_bytes,
        )
    };

    let record = LocalImageRecord {
        tag: tag.clone(),
        service: params.service.to_owned(),
        image_ref,
        git_sha: git_sha.clone(),
        image_id: image_id.clone(),
        platform: detected_platform.clone(),
        archive_file,
        compression: "gzip".into(),
        size_bytes,
        sha256,
        created_at,
    };

    let mut images = load_local_images(params.images_root, params.service)?;
    images.retain(|item| item.tag != record.tag);
    images.insert(0, record.clone());

    let pruned = prune_records(
        &dir,
        &mut images,
        params.local_keep_count,
        params.protected_tag,
    );
    save_local_images(params.images_root, params.service, images)?;

    let elapsed = started.elapsed();
    writeln!(
        out,
        "Built {} ({} bytes, sha256 {}) -> {}",
        record.image_ref,
        record.size_bytes,
        &record.sha256[..12],
        dest_path.display()
    )?;
    writeln!(out, "  Service:     {}", record.service)?;
    writeln!(out, "  Tag:         {}", record.tag)?;
    if let Some(id) = &image_id {
        writeln!(out, "  Image ID:    {id}")?;
    }
    if let Some(platform) = &detected_platform {
        writeln!(out, "  Platform:    {platform}")?;
    }
    writeln!(
        out,
        "  Git SHA:     {} (BUILD_TIME={build_time})",
        build_args
            .get("GIT_SHA")
            .map(String::as_str)
            .unwrap_or("nogit")
    )?;
    if let Some(uncompressed) = uncompressed_bytes {
        writeln!(
            out,
            "  Size:        {} compressed ({} uncompressed)",
            format_bytes(record.size_bytes),
            format_bytes(uncompressed)
        )?;
    } else {
        writeln!(
            out,
            "  Size:        {} ({} bytes)",
            format_bytes(record.size_bytes),
            record.size_bytes
        )?;
    }
    writeln!(out, "  Digest:      sha256:{}", record.sha256)?;
    writeln!(out, "  Duration:    {:.1}s", elapsed.as_secs_f64())?;
    writeln!(
        out,
        "  Next step:   `rudder push` (or `rudder push --deploy` to deploy immediately)"
    )?;
    if !pruned.is_empty() {
        writeln!(out, "Pruned local archives: {}", pruned.join(", "))?;
    }

    Ok((record, pruned))
}

fn inspect_built_image(image_ref: &str) -> (Option<String>, Option<String>, Option<u64>) {
    let Ok(output) = Command::new("docker")
        .args([
            "image",
            "inspect",
            image_ref,
            "--format",
            "{{.Id}}|{{.Os}}/{{.Architecture}}|{{.Size}}",
        ])
        .output()
    else {
        return (None, None, None);
    };
    if !output.status.success() {
        return (None, None, None);
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let mut parts = line.split('|');
    let id = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let platform = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "/")
        .map(str::to_owned);
    let size = parts.next().and_then(|s| s.trim().parse::<u64>().ok());
    (id, platform, size)
}

pub fn prune_local_store(
    images_root: &Path,
    service: &str,
    keep_count: u32,
    protected_tag: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let dir = service_dir(images_root, service);
    let mut images = load_local_images(images_root, service)?;
    let pruned = prune_records(&dir, &mut images, Some(keep_count.max(1)), protected_tag);
    save_local_images(images_root, service, images)?;
    Ok(pruned)
}

fn prune_records(
    dir: &Path,
    images: &mut Vec<LocalImageRecord>,
    keep_count: Option<u32>,
    protected_tag: Option<&str>,
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
    let to_prune =
        select_images_to_prune_protecting(&entries, Some(keep), None, protected_tag, Utc::now());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_build_args_injects_git_sha_and_build_time_and_allows_override() {
        let custom = BTreeMap::from([("APP_ENV".to_owned(), "staging".to_owned())]);
        let args = effective_build_args(Some("9e5686f"), "2026-11-07T03:00:00Z", &custom);
        assert_eq!(args.get("GIT_SHA").unwrap(), "9e5686f");
        assert_eq!(args.get("BUILD_TIME").unwrap(), "2026-11-07T03:00:00Z");
        assert_eq!(args.get("APP_ENV").unwrap(), "staging");

        let fallback = effective_build_args(None, "2026-11-07T03:00:00Z", &BTreeMap::new());
        assert_eq!(fallback.get("GIT_SHA").unwrap(), "nogit");
    }

    #[test]
    fn dockerignore_respects_dockerfile_specific_ignore_and_negations() {
        let tmp = std::env::temp_dir().join(format!(
            "rudder-ctx-test-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let rust_dir = tmp.join("rust");
        let nm_dir = tmp.join("node_modules");
        std::fs::create_dir_all(&rust_dir).unwrap();
        std::fs::create_dir_all(&nm_dir).unwrap();
        let dockerfile = rust_dir.join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").unwrap();
        std::fs::write(rust_dir.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        std::fs::write(nm_dir.join("big.js"), vec![b'x'; 4096]).unwrap();
        std::fs::write(
            rust_dir.join("Dockerfile.dockerignore"),
            "*\n!rust/Cargo.toml\n!rust/Dockerfile\n",
        )
        .unwrap();

        let summary = measure_build_context(&tmp, &dockerfile);
        assert_eq!(summary.file_count, 2);
        assert!(summary.total_bytes < 200);
        assert!(summary
            .ignore_file
            .as_ref()
            .unwrap()
            .ends_with("Dockerfile.dockerignore"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
