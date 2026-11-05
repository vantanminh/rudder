//! Saved panel URL and API token (`~/.config/rudder/config.json`, mode 0600).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub url: String,
    pub token: String,
}

/// `$RUDDER_CONFIG`, else `$XDG_CONFIG_HOME/rudder/config.json`, else `~/.config/rudder/config.json`.
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("RUDDER_CONFIG").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("rudder").join("config.json")
}

/// Directory for local Rudder Image archives (`$RUDDER_IMAGE_DIR` or alongside `config.json`).
pub fn images_dir(config_path: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("RUDDER_IMAGE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("images")
}

pub fn load(path: &Path) -> anyhow::Result<Option<Saved>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("{} is not a Rudder config", path.display())
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn save(path: &Path, saved: &Saved) -> anyhow::Result<()> {
    use std::io::Write;

    let Some(dir) = path.parent() else {
        bail!("{} has no parent directory", path.display());
    };
    if !dir.as_os_str().is_empty() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(saved)?)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// A panel base URL without a trailing slash.
pub fn normalize_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let parsed = url::Url::parse(trimmed).with_context(|| format!("'{trimmed}' is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("the panel URL must be http:// or https://, got '{trimmed}'");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("the panel URL cannot have a query or fragment");
    }
    Ok(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_normalized_and_checked() {
        assert_eq!(
            normalize_url(" https://panel.example.com/ ").unwrap(),
            "https://panel.example.com"
        );
        assert!(normalize_url("ftp://x").is_err());
        assert!(normalize_url("panel.example.com").is_err());
        assert!(normalize_url("https://x/?a=1").is_err());
    }

    #[test]
    fn saved_config_is_private() {
        let dir = std::env::temp_dir().join(format!("rudder-cli-{}", std::process::id()));
        let path = dir.join("nested").join("config.json");
        let saved = Saved {
            url: "http://127.0.0.1:3000".into(),
            token: "rdr_secret".into(),
        };
        save(&path, &saved).unwrap();
        save(&path, &saved).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(load(&path).unwrap().unwrap().token, "rdr_secret");
        assert!(load(&dir.join("missing.json")).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
