//! Project configuration file (`rudder.toml`) for local CI checks, Docker builds,
//! and dual retention settings (Cloudflare R2 vs local developer storage).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

pub const DEFAULT_FILE: &str = "rudder.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub service: String,
    pub dockerfile: String,
    pub context: String,
    pub checks: Vec<String>,
    pub r2_keep_count: Option<u32>,
    pub local_keep_count: Option<u32>,
    pub deploy_build: bool,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub build_args: BTreeMap<String, String>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            service: String::new(),
            dockerfile: "Dockerfile".into(),
            context: ".".into(),
            checks: Vec::new(),
            r2_keep_count: None,
            local_keep_count: None,
            deploy_build: false,
            platform: None,
            build_args: BTreeMap::new(),
        }
    }
}

impl ProjectConfig {
    pub fn render_toml(&self) -> String {
        let mut out = String::new();
        out.push_str("# Rudder project configuration (https://github.com/vantanminh/rudder)\n");
        out.push_str(&format!("service = {}\n", quote_str(&self.service)));
        out.push_str(&format!("dockerfile = {}\n", quote_str(&self.dockerfile)));
        out.push_str(&format!("context = {}\n", quote_str(&self.context)));
        if let Some(platform) = &self.platform {
            out.push_str(&format!("platform = {}\n", quote_str(platform)));
        } else {
            out.push_str("# platform = \"linux/amd64\" # Optional: target platform for docker build\n");
        }
        let checks_rendered: Vec<String> = self.checks.iter().map(|c| quote_str(c)).collect();
        out.push_str(&format!("checks = [{}]\n", checks_rendered.join(", ")));
        out.push_str(&format!(
            "deploy_build = {}\n",
            if self.deploy_build { "true" } else { "false" }
        ));
        if let Some(r2) = self.r2_keep_count {
            out.push_str(&format!("r2_keep_count = {r2}\n"));
        } else {
            out.push_str("# r2_keep_count = 10   # Optional: keep only the newest N images in Cloudflare R2\n");
        }
        if let Some(local) = self.local_keep_count {
            out.push_str(&format!("local_keep_count = {local}\n"));
        } else {
            out.push_str(
                "# local_keep_count = 20 # Optional: keep N image archives on this machine\n",
            );
        }
        if !self.build_args.is_empty() {
            let pairs: Vec<String> = self
                .build_args
                .iter()
                .map(|(k, v)| format!("{k} = {}", quote_str(v)))
                .collect();
            out.push_str(&format!("build_args = {{ {} }}\n", pairs.join(", ")));
        }
        out
    }

    pub fn parse_toml(input: &str) -> anyhow::Result<Self> {
        let mut cfg = Self::default();
        let mut section = String::new();
        for (idx, raw_line) in input.lines().enumerate() {
            let line_no = idx + 1;
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_owned();
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                bail!("rudder.toml line {line_no}: expected key = value");
            };
            let key = key.trim();
            let val = val.trim();
            if section == "build_args" {
                let k = parse_table_key(key, line_no)?;
                cfg.build_args.insert(k, parse_str(val, line_no)?);
                continue;
            }
            match key {
                "service" => cfg.service = parse_str(val, line_no)?,
                "dockerfile" => cfg.dockerfile = parse_str(val, line_no)?,
                "context" => cfg.context = parse_str(val, line_no)?,
                "platform" => {
                    let p = parse_str(val, line_no)?;
                    cfg.platform = (!p.trim().is_empty()).then(|| p.trim().to_owned());
                }
                "checks" => cfg.checks = parse_str_array(val, line_no)?,
                "build_args" => {
                    for (k, v) in parse_inline_map(val, line_no)? {
                        cfg.build_args.insert(k, v);
                    }
                }
                "r2_keep_count" => {
                    let n: u32 = val.parse().with_context(|| {
                        format!("rudder.toml line {line_no}: invalid r2_keep_count")
                    })?;
                    cfg.r2_keep_count = (n >= 1).then_some(n);
                }
                "local_keep_count" => {
                    let n: u32 = val.parse().with_context(|| {
                        format!("rudder.toml line {line_no}: invalid local_keep_count")
                    })?;
                    cfg.local_keep_count = (n >= 1).then_some(n);
                }
                "deploy_build" => {
                    cfg.deploy_build = match val {
                        "true" => true,
                        "false" => false,
                        _ => {
                            bail!("rudder.toml line {line_no}: deploy_build must be true or false")
                        }
                    };
                }
                _ => {}
            }
        }
        Ok(cfg)
    }
}

pub fn load_optional(path: Option<&Path>) -> anyhow::Result<Option<(PathBuf, ProjectConfig)>> {
    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FILE));
    match std::fs::read_to_string(&target) {
        Ok(text) => {
            let parsed = ProjectConfig::parse_toml(&text)
                .with_context(|| format!("parse {}", target.display()))?;
            Ok(Some((target, parsed)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && path.is_none() => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", target.display())),
    }
}

fn quote_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quotes {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if ch == '#' && !in_quotes {
            return &line[..i];
        }
    }
    line
}

fn parse_str(val: &str, line_no: usize) -> anyhow::Result<String> {
    let trimmed = val.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        Ok(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
    } else if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        Ok(trimmed[1..trimmed.len() - 1].to_owned())
    } else {
        bail!("rudder.toml line {line_no}: expected quoted string, got {trimmed}");
    }
}

fn parse_str_array(val: &str, line_no: usize) -> anyhow::Result<Vec<String>> {
    let trimmed = val.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        bail!("rudder.toml line {line_no}: expected array [...]");
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quotes {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            let piece = current.trim();
            if !piece.is_empty() {
                items.push(parse_str(piece, line_no)?);
            }
            current.clear();
            continue;
        }
        current.push(ch);
    }
    let tail = current.trim();
    if !tail.is_empty() {
        items.push(parse_str(tail, line_no)?);
    }
    Ok(items)
}

fn parse_table_key(raw: &str, line_no: usize) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return parse_str(trimmed, line_no);
    }
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!("rudder.toml line {line_no}: invalid build_args key '{trimmed}'");
    }
    Ok(trimmed.to_owned())
}

fn parse_inline_map(val: &str, line_no: usize) -> anyhow::Result<BTreeMap<String, String>> {
    let trimmed = val.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        bail!("rudder.toml line {line_no}: expected inline table {{ KEY = \"VALUE\" }}");
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    let mut map = BTreeMap::new();
    if inner.is_empty() {
        return Ok(map);
    }
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && in_quotes {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            current.push(ch);
            continue;
        }
        if ch == ',' && !in_quotes {
            let piece = current.trim();
            if !piece.is_empty() {
                entries.push(piece.to_owned());
            }
            current.clear();
            continue;
        }
        current.push(ch);
    }
    let tail = current.trim();
    if !tail.is_empty() {
        entries.push(tail.to_owned());
    }
    for entry in entries {
        let Some((k, v)) = entry.split_once('=') else {
            bail!("rudder.toml line {line_no}: expected KEY = \"VALUE\" inside build_args");
        };
        map.insert(parse_table_key(k, line_no)?, parse_str(v, line_no)?);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_rudder_toml() {
        let mut build_args = BTreeMap::new();
        build_args.insert("RUST_VERSION".into(), "1.97".into());
        let cfg = ProjectConfig {
            service: "shop/prod/web".into(),
            dockerfile: "Dockerfile".into(),
            context: ".".into(),
            checks: vec!["cargo test".into(), "pnpm build".into()],
            r2_keep_count: Some(10),
            local_keep_count: Some(25),
            deploy_build: true,
            platform: Some("linux/amd64".into()),
            build_args,
        };
        let text = cfg.render_toml();
        let parsed = ProjectConfig::parse_toml(&text).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn parses_crlf_and_build_args_section() {
        let raw = "service = \"knotree/staging/knotree-api\"\r\ndockerfile = \"rust/Dockerfile\"\r\ncontext = \".\"\r\nplatform = \"linux/amd64\"\r\nbuild_args = { A = \"1\", B = \"two\" }\r\n[build_args]\r\nC = \"three\"\r\n";
        let parsed = ProjectConfig::parse_toml(raw).unwrap();
        assert_eq!(parsed.service, "knotree/staging/knotree-api");
        assert_eq!(parsed.platform.as_deref(), Some("linux/amd64"));
        assert_eq!(parsed.build_args.get("A").map(String::as_str), Some("1"));
        assert_eq!(parsed.build_args.get("B").map(String::as_str), Some("two"));
        assert_eq!(parsed.build_args.get("C").map(String::as_str), Some("three"));
    }
}
