//! Copy a Git work tree onto a server without its `.git` directory.
//!
//! The panel clones the repository. A remote server receives only the tracked
//! files, as a tar extracted under the node data directory. A manifest beside
//! the checkout lists those files so the next copy can remove ones Git deleted.
//! Files that were never in the manifest, such as a bind-mounted `./data`, stay.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Cursor, Write};
use std::path::{Component, Path};
use std::process::Command;

use rudder_core::Error;

/// Largest checkout copied to a remote server.
pub const MAX_TREE_BYTES: u64 = 512 * 1024 * 1024;

/// Manifest stored next to `stacks/<project>/src`, not inside the work tree.
pub fn manifest_relative(tree: &str) -> String {
    format!("{tree}.manifest")
}

/// Paths Git tracks in `checkout`, relative and sorted. Untracked files are omitted.
pub fn git_tracked(checkout: &Path) -> Result<Vec<String>, Error> {
    if !checkout.join(".git").exists() {
        return Err(Error::Validation("the checkout has no git metadata".into()));
    }
    let output = Command::new("git")
        .args(["ls-files", "-z", "--"])
        .current_dir(checkout)
        .output()
        .map_err(|error| Error::Node(format!("git ls-files: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Node(format!(
            "git ls-files failed: {}",
            detail.trim()
        )));
    }
    let mut files = Vec::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(raw)
            .map_err(|_| Error::Validation("git listed a file path that is not UTF-8".into()))?;
        check_relative(Path::new(text))?;
        files.push(text.to_owned());
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Tar of `files`, which are paths relative to `root`. Symlinks are stored as
/// the file they point at, and only when that file stays inside `root`.
pub fn pack_files(root: &Path, files: &[String]) -> Result<Vec<u8>, Error> {
    let root = root
        .canonicalize()
        .map_err(|error| Error::Node(format!("checkout: {error}")))?;
    let mut builder = tar::Builder::new(Vec::new());
    builder.mode(tar::HeaderMode::Deterministic);
    let mut total = 0u64;
    for rel in files {
        check_relative(Path::new(rel))?;
        let full = root.join(rel);
        let canonical = full
            .canonicalize()
            .map_err(|_| Error::Validation(format!("{rel} is not a file in the checkout")))?;
        if !canonical.starts_with(&root) {
            return Err(Error::Validation(format!(
                "{rel} points outside the checkout"
            )));
        }
        if !canonical.is_file() {
            return Err(Error::Validation(format!(
                "{rel} is not a file, so it was not copied"
            )));
        }
        let mut file =
            File::open(&canonical).map_err(|error| Error::Node(format!("read {rel}: {error}")))?;
        let len = file
            .metadata()
            .map_err(|error| Error::Node(error.to_string()))?
            .len();
        total = total.saturating_add(len);
        if total > MAX_TREE_BYTES {
            return Err(Error::Validation(
                "the checkout is larger than 512 MiB, so it was not copied".into(),
            ));
        }
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(len);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, rel, &mut file)
            .map_err(|error| Error::Node(format!("pack {rel}: {error}")))?;
    }
    builder
        .finish()
        .map_err(|error| Error::Node(format!("pack checkout: {error}")))?;
    builder
        .into_inner()
        .map_err(|error| Error::Node(format!("pack checkout: {error}")))
}

/// Extract `archive` into `dest`. Files listed in the previous manifest and
/// absent from this archive are removed. Anything else under `dest` is kept.
pub fn apply_tree(dest: &Path, manifest: &Path, archive: &[u8]) -> Result<(), Error> {
    if archive.len() as u64 > MAX_TREE_BYTES {
        return Err(Error::Validation(
            "the checkout is larger than 512 MiB, so it was not copied".into(),
        ));
    }
    fs::create_dir_all(dest).map_err(|error| Error::Node(error.to_string()))?;
    if let Some(parent) = manifest.parent() {
        fs::create_dir_all(parent).map_err(|error| Error::Node(error.to_string()))?;
    }
    let mut unpacked = tar::Archive::new(Cursor::new(archive));
    let mut written = BTreeSet::new();
    let mut total = 0u64;
    let entries = unpacked
        .entries()
        .map_err(|error| Error::Node(format!("read checkout archive: {error}")))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|error| Error::Node(format!("read checkout archive: {error}")))?;
        let rel = entry
            .path()
            .map_err(|error| Error::Node(format!("checkout path: {error}")))?
            .into_owned();
        check_relative(&rel)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(dest.join(&rel)).map_err(|error| Error::Node(error.to_string()))?;
            continue;
        }
        if !kind.is_file() {
            return Err(Error::Validation(format!(
                "the checkout archive contains an unsupported entry {}",
                rel.display()
            )));
        }
        let size = entry.header().size().unwrap_or(0);
        total = total.saturating_add(size);
        if total > MAX_TREE_BYTES {
            return Err(Error::Validation(
                "the checkout is larger than 512 MiB, so it was not copied".into(),
            ));
        }
        let target = dest.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| Error::Node(error.to_string()))?;
        }
        let mut file = File::create(&target)
            .map_err(|error| Error::Node(format!("write {}: {error}", rel.display())))?;
        let copied = std::io::copy(&mut entry, &mut file)
            .map_err(|error| Error::Node(format!("write {}: {error}", rel.display())))?;
        if copied > size {
            return Err(Error::Validation(
                "the checkout archive is larger than it claims".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
                .map_err(|error| Error::Node(error.to_string()))?;
        }
        written.insert(normalize(&rel));
    }
    for stale in read_manifest(manifest) {
        if !written.contains(&stale) && check_relative(Path::new(&stale)).is_ok() {
            let _ = fs::remove_file(dest.join(&stale));
        }
    }
    write_manifest(manifest, &written)
}

/// Reject absolute paths, `..`, and any `.git` component.
pub fn check_relative(path: &Path) -> Result<(), Error> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(Error::Validation(
            "checkout path must stay inside the repository".into(),
        ));
    }
    let mut parts = 0;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_string_lossy();
                if text == ".git" || text.contains('\0') {
                    return Err(Error::Validation(
                        "checkout path must stay inside the repository".into(),
                    ));
                }
                parts += 1;
            }
            Component::CurDir => {}
            _ => {
                return Err(Error::Validation(
                    "checkout path must stay inside the repository".into(),
                ));
            }
        }
    }
    if parts == 0 {
        return Err(Error::Validation(
            "checkout path must stay inside the repository".into(),
        ));
    }
    Ok(())
}

fn normalize(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn read_manifest(path: &Path) -> BTreeSet<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn write_manifest(path: &Path, files: &BTreeSet<String>) -> Result<(), Error> {
    let mut body = files.iter().cloned().collect::<Vec<_>>().join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let mut file =
        File::create(path).map_err(|error| Error::Node(format!("write manifest: {error}")))?;
    file.write_all(body.as_bytes())
        .map_err(|error| Error::Node(format!("write manifest: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| Error::Node(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let path =
                std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> TempDir {
        TempDir::new(name)
    }

    #[test]
    fn copy_keeps_untracked_files_and_drops_deleted_tracked_ones() {
        let dir = scratch("rudder-tree");
        let src = dir.0.join("src");
        fs::create_dir_all(src.join("app")).unwrap();
        fs::write(src.join("app/main.txt"), "one").unwrap();
        fs::write(src.join("README"), "hi").unwrap();
        let packed = pack_files(&src, &["README".into(), "app/main.txt".into()]).unwrap();

        let dest = dir.0.join("remote/src");
        let manifest = dir.0.join("remote/src.manifest");
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(dest.join("data")).unwrap();
        fs::write(dest.join("data/db"), "keep-me").unwrap();
        fs::write(dest.join("old.txt"), "gone").unwrap();
        fs::write(&manifest, "old.txt\nREADME\n").unwrap();

        apply_tree(&dest, &manifest, &packed).unwrap();
        assert_eq!(fs::read_to_string(dest.join("README")).unwrap(), "hi");
        assert_eq!(
            fs::read_to_string(dest.join("app/main.txt")).unwrap(),
            "one"
        );
        assert_eq!(fs::read_to_string(dest.join("data/db")).unwrap(), "keep-me");
        assert!(!dest.join("old.txt").exists());
        let listed = fs::read_to_string(&manifest).unwrap();
        assert!(listed.contains("app/main.txt"));
        assert!(listed.contains("README"));
        assert!(!listed.contains("old.txt"));
        assert!(!listed.contains("data/db"));
    }

    #[test]
    fn archive_cannot_escape_the_checkout() {
        let dir = scratch("rudder-tree-escape");
        let bytes = hostile_ustar("../secret", b"nope");
        let dest = dir.0.join("src");
        let err = apply_tree(&dest, &dir.0.join("src.manifest"), &bytes).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("inside the repository") || message.contains(".."),
            "{message}"
        );
        assert!(!dir.0.join("secret").exists());
        assert!(!dest.join("secret").exists());
    }

    /// A ustar member whose name the `tar` builder refuses to write.
    fn hostile_ustar(name: &str, body: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", body.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..262].copy_from_slice(b"ustar");
        header[263..265].copy_from_slice(b"00");
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        let mut out = header.to_vec();
        out.extend_from_slice(body);
        out.extend(std::iter::repeat_n(0, 512 - body.len()));
        out.extend(std::iter::repeat_n(0, 1024));
        out
    }

    #[test]
    #[cfg(unix)]
    fn symlink_outside_the_checkout_is_not_packed() {
        let dir = scratch("rudder-tree-link");
        let src = dir.0.join("src");
        fs::create_dir_all(&src).unwrap();
        let outside = dir.0.join("secret");
        fs::write(&outside, "hidden").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("leak")).unwrap();
        let err = pack_files(&src, &["leak".into()]).unwrap_err();
        assert!(err.to_string().contains("outside"));
    }

    #[test]
    fn git_lists_tracked_files_and_skips_untracked_ones() {
        let dir = scratch("rudder-tree-git");
        let repo = &dir.0;
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        git(&["init"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        fs::create_dir_all(repo.join("web")).unwrap();
        fs::write(repo.join("web/index.html"), "<p>").unwrap();
        fs::write(repo.join("compose.yml"), "services: {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
        fs::write(repo.join("data.sqlite"), "local").unwrap();
        let files = git_tracked(repo).unwrap();
        assert_eq!(
            files,
            ["compose.yml".to_owned(), "web/index.html".to_owned()]
        );
    }
}
