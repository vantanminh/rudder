//! GitHub App helpers: the RS256 JWT used to mint an installation token, the
//! manifest that lets GitHub create an app in one click, and repository names.
//!
//! The private key stays with the caller. `github_app_jwt` only returns the JWT.

use base64::Engine;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::Pkcs1v15Sign;
use rsa::pkcs8::DecodePrivateKey;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::Error;

/// Longest app name GitHub accepts.
const APP_NAME_MAX: usize = 34;

/// What the manifest needs to know about this panel.
pub struct AppManifest<'a> {
    /// Display name of the app. GitHub requires it to be unique.
    pub name: &'a str,
    /// Base URL of the panel as the browser reaches it, without a trailing slash.
    pub base_url: &'a str,
    /// Where GitHub delivers push events. `None` leaves webhooks off.
    pub webhook_url: Option<&'a str>,
}

/// Manifest for `POST https://github.com/settings/apps/new`.
///
/// The app only gets read access to repository contents, which is enough to
/// clone, and a push subscription when a webhook URL is given. GitHub sends the
/// browser back to `{base_url}/git/github` after the app is created and again
/// after it is installed.
pub fn github_app_manifest(input: &AppManifest<'_>) -> Value {
    let base = input.base_url.trim_end_matches('/');
    let landing = format!("{base}/git/github");
    let mut manifest = json!({
        "name": input.name,
        "url": base,
        "description": "Clones repositories for Rudder deployments.",
        "redirect_url": landing,
        "setup_url": landing,
        "setup_on_update": true,
        "public": false,
        "default_permissions": { "contents": "read", "metadata": "read" },
    });
    if let Some(url) = input.webhook_url {
        manifest["hook_attributes"] = json!({ "url": url, "active": true });
        manifest["default_events"] = json!(["push", "pull_request"]);
    }
    manifest
}

/// GitHub app names are shown to people, so spaces are fine, but not much else.
pub fn validate_app_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.len() <= APP_NAME_MAX
        && !name.starts_with([' ', '-'])
        && !name.ends_with([' ', '-'])
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(Error::Validation(
            "app name must be 1 to 34 letters, digits, spaces, dashes or underscores".into(),
        ))
    }
}

/// A GitHub user or organization login.
pub fn validate_github_login(login: &str) -> Result<(), Error> {
    let ok = !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && login
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if ok {
        Ok(())
    } else {
        Err(Error::Validation("invalid GitHub organization".into()))
    }
}

/// True when GitHub cannot reach the host, so a webhook there would never fire.
/// `host` has no port; an IPv6 literal may be in brackets.
pub fn is_private_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
            }
        };
    }
    !host.contains('.')
}

/// `owner/name`, lowercased, of a github.com repository URL in HTTPS or SSH form.
pub fn github_repo_full_name(url: &str) -> Option<String> {
    let url = url.trim();
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("ssh://git@"))?;
        let (host, path) = rest.split_once('/')?;
        if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com")
        {
            return None;
        }
        path
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    (valid(owner) && valid(name)).then(|| format!("{owner}/{name}").to_ascii_lowercase())
}

pub fn github_app_jwt(app_id: &str, private_key_pem: &str, now_unix: i64) -> Result<String, Error> {
    if app_id.is_empty() || app_id.len() > 20 || !app_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Validation("github app id must be numeric".into()));
    }
    let iat = now_unix.saturating_sub(60);
    let exp = now_unix.saturating_add(9 * 60);
    let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = format!(r#"{{"iat":{iat},"exp":{exp},"iss":"{app_id}"}}"#);
    let payload_b64 = b64url(payload.as_bytes());
    let signing_input = format!("{header}.{payload_b64}");
    let key = RsaPrivateKey::from_pkcs8_pem(private_key_pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(private_key_pem))
        .map_err(|_| Error::Validation("github app private key could not be parsed".into()))?;
    let digest = Sha256::digest(signing_input.as_bytes());
    let signature = key
        .sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
        .map_err(|_| Error::Validation("github app private key could not sign".into()))?;
    Ok(format!("{signing_input}.{}", b64url(&signature)))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn manifest_points_github_back_at_the_panel() {
        let manifest = github_app_manifest(&AppManifest {
            name: "Rudder-1a2b",
            base_url: "https://panel.example/",
            webhook_url: Some("https://panel.example/api/v1/git-providers/x/webhook"),
        });
        assert_eq!(manifest["redirect_url"], "https://panel.example/git/github");
        assert_eq!(manifest["setup_url"], "https://panel.example/git/github");
        assert_eq!(manifest["url"], "https://panel.example");
        assert_eq!(manifest["public"], false);
        assert_eq!(manifest["default_permissions"]["contents"], "read");
        assert_eq!(manifest["hook_attributes"]["active"], true);
        assert_eq!(manifest["default_events"], json!(["push", "pull_request"]));
    }

    #[test]
    fn manifest_without_a_webhook_subscribes_to_nothing() {
        let manifest = github_app_manifest(&AppManifest {
            name: "Rudder",
            base_url: "http://localhost:3000",
            webhook_url: None,
        });
        assert!(manifest.get("hook_attributes").is_none());
        assert!(manifest.get("default_events").is_none());
    }

    #[test]
    fn app_names_and_logins_are_checked() {
        assert!(validate_app_name("Rudder-1a2b").is_ok());
        assert!(validate_app_name("My Deploy_App").is_ok());
        for bad in ["", " lead", "trail-", "a/b", "quote\"d", &"x".repeat(35)] {
            assert!(validate_app_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_github_login("acme-co").is_ok());
        for bad in ["", "-acme", "acme-", "ac me", "ac/me", &"x".repeat(40)] {
            assert!(validate_github_login(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn private_hosts_cannot_receive_webhooks() {
        for private in [
            "localhost",
            "panel.localhost",
            "nas.local",
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.9",
            "172.20.0.1",
            "169.254.1.1",
            "[::1]",
            "fd00::1",
            "panel",
        ] {
            assert!(is_private_host(private), "{private}");
        }
        for public in [
            "panel.example.com",
            "8.8.8.8",
            "172.32.0.1",
            "[2001:db8::1]",
        ] {
            assert!(!is_private_host(public), "{public}");
        }
    }

    #[test]
    fn repository_names_come_from_https_and_ssh_urls() {
        let want = Some("acme/web-app".to_owned());
        for url in [
            "https://github.com/acme/web-app",
            "https://github.com/Acme/Web-App.git",
            "https://github.com/acme/web-app/",
            "git@github.com:acme/web-app.git",
            "ssh://git@github.com/acme/web-app.git",
            "https://www.github.com/acme/web-app.git",
        ] {
            assert_eq!(github_repo_full_name(url), want, "{url}");
        }
        for url in [
            "https://gitlab.com/acme/web-app.git",
            "https://github.com/acme",
            "https://github.com/acme/web app",
            "git@github.com:acme",
            "file:///srv/acme/web-app",
            "",
        ] {
            assert_eq!(github_repo_full_name(url), None, "{url}");
        }
    }

    #[test]
    fn jwt_signature_matches_the_app_key() {
        let pem = TEST_APP_KEY;
        let jwt = github_app_jwt("12345", pem, 1_700_000_000).unwrap();
        let mut parts = jwt.split('.');
        let header = parts.next().unwrap();
        let payload = parts.next().unwrap();
        let signature = parts.next().unwrap();
        assert!(parts.next().is_none());
        let signing_input = format!("{header}.{payload}");
        let signature_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .unwrap();
        let private = RsaPrivateKey::from_pkcs8_pem(pem).unwrap();
        private
            .to_public_key()
            .verify(
                Pkcs1v15Sign::new::<Sha256>(),
                &Sha256::digest(signing_input.as_bytes()),
                &signature_bytes,
            )
            .unwrap();
        let payload_json = String::from_utf8(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload)
                .unwrap(),
        )
        .unwrap();
        assert!(payload_json.contains("\"iss\":\"12345\""));
        assert!(github_app_jwt("app", pem, 1_700_000_000).is_err());
    }

    pub(crate) const TEST_APP_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDMm3BWuPw2Hlpa
/TRK53FVFVMQAu4YEIgfe6UxBgWcDEIQK98obYe7y3D7+pwutdqcjjNpZiNX6mRx
VZqqMFwVfTDIKGIslUEIuJrwhGrVhXBiJFH32Lhi/N0a9elm4qKyvknri1ZnpORe
AneRnumaE4wX+euulDL+asQ0k6f/G9XVgMCHfS09dZisehNngSFFlyBoyG56gall
DvGQHqOUO1ewI5OS05G/7WmId5rbHAZ0k07jPlSHHf4OaaSWjlg3q4z1TiVAUuCc
/JW429HIqqiJTqu3oUQksf2G4jR7WsfLuEfkfqHM064WdjCVsaOjXHCDEa5fLtBC
MqAf+L73AgMBAAECggEACVnXQ1dhS3XiqkLlMhaBbaKJBnPOfty5OTsfkcMqhsxh
STeVaNXB/M6vmLcw48pINfHNHrWIyvqkNtvIiEZShww5f678zE3RSD1A84S8aeAX
LUe8qUecfFr9yJQm48la0dwe07wtVD+gd5LESoGelSiXNVH6N/h/m3S0sonHBUJ4
Tw14zOayx8ce1yxrHBob/MUt6GRS0YXjYxF8JvmntwwmzhHfWRbPGDDJgAns4v0P
XYdJWZ/f8QjXAYl6UY4aKzaG8kGznX/oOfNwBmLBHWrpMHoAgUBSxTzmfSee7zKw
6U5RwNLpEDz1JSSmcGO0JznQb8xo+naq32sPsWcC5QKBgQDtln3wmUVJwzNLe7x3
TGPgYrY0ZAVah7rdDjbg8d8aLFCmfjFavtUOiwtq0t6EFB1MA7qE335N8SX4ayrm
tRFt+H/GKF1CR8B2pBovaiYupshD8M9dFHm/v1giTJbcxzwNtAHPOjivRSzWhcr3
AdssvMNnH034fkK8RvuJi6vcuwKBgQDcdqSUJ4bZKoXif1ZvsA6sSngwwnzqrc/v
4njNhtweH1lY14RT1xm4J+piIIdU5MWhwrcRIlgAd9hAd22mMDMFOonwErOAOHID
7pIzjYHBmjJmISnSz5XjDMKJ+5eIS4y7qOwvUZjoGp4Ma5sthc7s1A2pid6CrZHy
1veESNOA9QKBgEMgBaz4YM8aS1/EMdcyyfUXbosWxzxwpKbX4UFklLf8VTz04lTV
HbTiK/OeaCfA1xYm54eq7TBwNV4eSRytg6MiVldSCCc1xna+nndO8kKz9FjdrHiL
1GoMUYY/RVBTKfBlLy2xMVqznJtvaYyoBZh6LGryOUE/yvMOyvptb70JAoGAVi2C
c+9nhOhcYYdJMGtvsOWDSfwcfFfpGmo6soxu6s9yS4ojTeqdQSFJO4ZfoE20+2Bz
0xzLrZMrUzfcAsSAjSCu3stQo2RtHSBawhAkkptF7Co2gRNoEdLhELLqLvrpTknf
2TdaQBB3i39cr1TI6EaT+JvOAOL8VCK3qlwVYSkCgYAFsXiAxrbo46C8cUFNqM/U
VX+MdJnueahwi5LKxcSztUwYLVSxrWk0e5/8wAhME36acfUl/0Xgf6SFs8iVbOJ6
RDhdlwzmNJrNLaFcSKAPVltO1B8fQcBxIVbQCOwyWawFdcRbwOUcHgmjY1Y+CRjY
ZSY+TIIzNCTcxml9MFZ09A==
-----END PRIVATE KEY-----
";
}
