//! Domain primitives for Rudder Image (local build, R2 retention, project config)
//! and CLI authentication (browser loopback callback and 8-character device codes).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Alphabet for 8-character device codes (uppercase letters and digits).
const LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
const DIGITS: &[u8] = b"23456789";
const ALPHANUM: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Generate an 8-character alphanumeric device code guaranteed to contain both
/// letters and digits (for example `A7K9M2X4`).
pub fn generate_device_code() -> String {
    let bytes = crate::random_bytes::<10>();
    let mut chars = [b'A'; 8];
    for i in 0..8 {
        chars[i] = ALPHANUM[(bytes[i] as usize) % ALPHANUM.len()];
    }
    let has_letter = chars.iter().any(|c| c.is_ascii_alphabetic());
    let has_digit = chars.iter().any(|c| c.is_ascii_digit());
    if !has_letter {
        let pos = (bytes[8] as usize) % 8;
        chars[pos] = LETTERS[(bytes[9] as usize) % LETTERS.len()];
    } else if !has_digit {
        let pos = (bytes[8] as usize) % 8;
        chars[pos] = DIGITS[(bytes[9] as usize) % DIGITS.len()];
    }
    String::from_utf8(chars.to_vec()).expect("ASCII device code")
}

/// Normalize and validate an 8-character alphanumeric device code.
/// Accepts optional hyphens or whitespace (e.g. `a7k9-m2x4` -> `A7K9M2X4`).
pub fn normalize_device_code(raw: &str) -> Result<String, Error> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.len() != 8 || !cleaned.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Error::Validation(
            "device code must be 8 alphanumeric characters".into(),
        ));
    }
    let has_letter = cleaned.bytes().any(|b| b.is_ascii_alphabetic());
    let has_digit = cleaned.bytes().any(|b| b.is_ascii_digit());
    if !has_letter || !has_digit {
        return Err(Error::Validation(
            "device code must include both letters and numbers".into(),
        ));
    }
    Ok(cleaned)
}

/// Validate a loopback callback URL used by `rudder login --browser`.
pub fn validate_cli_callback_url(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 256
        || trimmed.contains(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '@' | '#'))
    {
        return Err(Error::Validation(
            "callback_url must be a valid loopback URL".into(),
        ));
    }
    let Some(rest) = trimmed.strip_prefix("http://") else {
        return Err(Error::Validation(
            "CLI callback URL must use http on loopback".into(),
        ));
    };
    let (authority, _path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port_str) = authority.rsplit_once(':').ok_or_else(|| {
        Error::Validation("CLI callback URL must include an explicit loopback port".into())
    })?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(Error::Validation(
            "CLI callback URL must target 127.0.0.1 or localhost".into(),
        ));
    }
    let port: u16 = port_str.parse().map_err(|_| {
        Error::Validation("CLI callback URL must include a valid loopback port".into())
    })?;
    if port == 0 {
        return Err(Error::Validation(
            "CLI callback URL port must be non-zero".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Validate a Rudder Image version tag (e.g. `20261105-163000-a1b2c3d` or `v1.2.0`).
pub fn validate_image_tag(tag: &str) -> Result<String, Error> {
    let trimmed = tag.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return Err(Error::Validation(
            "image tag must be 1 to 128 characters".into(),
        ));
    }
    let valid = trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !trimmed.starts_with(['.', '-']);
    if !valid {
        return Err(Error::Validation(
            "image tag may only contain letters, digits, '.', '-', or '_'".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Build the object key in Cloudflare R2 / S3 for a pushed Rudder Image archive.
pub fn image_object_key(
    prefix: &str,
    service_id: &str,
    tag: &str,
    compression: &str,
) -> Result<String, Error> {
    let clean_prefix = prefix.trim().trim_matches('/');
    if !clean_prefix.is_empty() {
        crate::validate_prefix(clean_prefix)?;
    }
    let clean_tag = validate_image_tag(tag)?;
    let ext = match compression {
        "zstd" => "tar.zst",
        "gzip" => "tar.gz",
        "none" => "tar",
        _ => {
            return Err(Error::Validation(
                "compression must be zstd, gzip, or none".into(),
            ))
        }
    };
    if clean_prefix.is_empty() {
        Ok(format!("{service_id}/{clean_tag}.{ext}"))
    } else {
        Ok(format!("{clean_prefix}/{service_id}/{clean_tag}.{ext}"))
    }
}

/// Metadata of a stored Rudder Image version for retention evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageVersionEntry {
    pub id: String,
    pub tag: String,
    pub created_at: DateTime<Utc>,
}

/// Determine which image IDs should be pruned from R2 (or local storage)
/// given optional `keep_count` (keep N newest) and `keep_days` (delete older than D days).
///
/// - When both `keep_count` and `keep_days` are `None`, nothing is pruned (default: keep all).
/// - The newest image (index 0 when sorted newest-first) is always kept so the service
///   always has at least one deployable image on R2.
pub fn select_images_to_prune(
    entries: &[ImageVersionEntry],
    keep_count: Option<u32>,
    keep_days: Option<u32>,
    now: DateTime<Utc>,
) -> Vec<String> {
    select_images_to_prune_protecting(entries, keep_count, keep_days, None, now)
}

/// Same as [`select_images_to_prune`], additionally protecting `protected_tag` (the image tag
/// currently deployed on the service) so an active deployment's image is never deleted.
pub fn select_images_to_prune_protecting(
    entries: &[ImageVersionEntry],
    keep_count: Option<u32>,
    keep_days: Option<u32>,
    protected_tag: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<String> {
    if keep_count.is_none() && keep_days.is_none() {
        return Vec::new();
    }
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    let cutoff = keep_days.map(|days| now - Duration::days(i64::from(days.max(1))));
    let mut prune = Vec::new();
    for (idx, item) in sorted.iter().enumerate() {
        if idx == 0 {
            // Never prune the most recent image version.
            continue;
        }
        if protected_tag.is_some_and(|tag| item.tag == tag || item.id == tag) {
            // Never prune the image version currently deployed on the service.
            continue;
        }
        let over_count = keep_count.is_some_and(|max| idx >= (max.max(1) as usize));
        let over_age = cutoff.is_some_and(|limit| item.created_at < limit);
        if over_count || over_age {
            prune.push(item.id.clone());
        }
    }
    prune
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_code_is_eight_alphanumeric_with_letters_and_digits() {
        for _ in 0..50 {
            let code = generate_device_code();
            assert_eq!(code.len(), 8);
            assert_eq!(normalize_device_code(&code).unwrap(), code);
            let formatted = format!("{}-{}", &code[..4], &code[4..]);
            assert_eq!(
                normalize_device_code(&formatted.to_lowercase()).unwrap(),
                code
            );
        }
        assert!(normalize_device_code("ABCDEFGH").is_err());
        assert!(normalize_device_code("12345678").is_err());
        assert!(normalize_device_code("A1B2C3").is_err());
    }

    #[test]
    fn callback_urls_must_be_loopback_http() {
        assert!(validate_cli_callback_url("http://127.0.0.1:54321/callback").is_ok());
        assert!(validate_cli_callback_url("http://localhost:8080/cb").is_ok());
        assert!(validate_cli_callback_url("https://evil.example.com/cb").is_err());
        assert!(validate_cli_callback_url("http://127.0.0.1/cb").is_err());
    }

    #[test]
    fn retention_keeps_all_by_default_and_prunes_oldest_when_configured() {
        let now = Utc::now();
        let entries: Vec<ImageVersionEntry> = (0..5)
            .map(|i| ImageVersionEntry {
                id: format!("img-{i}"),
                tag: format!("v{i}"),
                created_at: now - Duration::days(i),
            })
            .collect();
        assert!(select_images_to_prune(&entries, None, None, now).is_empty());
        assert_eq!(
            select_images_to_prune(&entries, Some(2), None, now),
            vec!["img-2", "img-3", "img-4"]
        );
        assert_eq!(
            select_images_to_prune(&entries, None, Some(3), now),
            vec!["img-4"]
        );
        // Currently deployed tag is never pruned even when older than keep_count.
        assert_eq!(
            select_images_to_prune_protecting(&entries, Some(2), None, Some("v3"), now),
            vec!["img-2", "img-4"]
        );
    }
}
