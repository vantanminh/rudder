//! Sessions, passwords, API tokens, and the request authenticator.
//!
//! Session cookies are random bearers stored only as SHA-256 hashes. CSRF is a
//! second random value checked on cookie-authenticated writes. Bearer tokens
//! skip CSRF because a browser will not attach them automatically.

use std::sync::OnceLock;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use cookie::{Cookie, SameSite};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "rudder_session";
const SESSION_DAYS: i64 = 7;

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub totp_enabled: bool,
    /// The account created at setup. Other administrators cannot remove it.
    pub is_owner: bool,
    pub csrf_token: Option<String>,
    pub scopes: Vec<String>,
    pub via_token: bool,
    /// Set when the bearer is an agent access token. Scope checks are finer
    /// than `read`/`write` and run before the handler.
    pub via_mcp: bool,
}

impl AuthUser {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    /// Cookie sessions of admins may write. Token callers need the `write` scope too.
    pub fn can_write(&self) -> bool {
        if !self.is_admin() {
            return false;
        }
        if self.via_mcp {
            return self.scopes.iter().any(|scope| scope.ends_with(":write"));
        }
        if self.via_token {
            self.scopes.iter().any(|scope| scope == "write")
        } else {
            true
        }
    }

    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "admin role required",
            ))
        }
    }

    pub fn require_write(&self) -> Result<(), ApiError> {
        if self.can_write() {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "write access required",
            ))
        }
    }
}

pub fn hash_password(password: &str) -> Result<String, ApiError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| {
            tracing::error!(%error, "password hash failed");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "password_hash",
                "could not hash password",
            )
        })
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(encoded) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password("not-a-real-user-password").expect("dummy password hash"))
}

pub fn dummy_verify(password: &str) {
    let _ = verify_password(password, dummy_hash());
}

pub fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

pub fn random_token(prefix: &str) -> String {
    let raw = URL_SAFE_NO_PAD.encode(rudder_core::random_bytes::<32>());
    if prefix.is_empty() {
        raw
    } else {
        format!("{prefix}_{raw}")
    }
}

pub fn session_expiry() -> DateTime<Utc> {
    Utc::now() + Duration::days(SESSION_DAYS)
}

pub fn tokens_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

pub struct OptionalPeer(pub Option<std::net::SocketAddr>);

impl FromRequestParts<AppState> for OptionalPeer {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|info| info.0);
        Ok(OptionalPeer(peer))
    }
}

/// True while Traefik serves the panel and apps through a Cloudflare tunnel.
static TUNNEL_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_tunnel_mode(on: bool) {
    TUNNEL_MODE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The visitor's address as Cloudflare saw it, when the request arrived through
/// the tunnel. `hop` is the address that connected to us (the first forwarded
/// address or the peer); the header is trusted only when that hop is on a
/// private network, which is where the tunnel's connector lives. A client that
/// reaches Traefik directly comes from a public address and cannot spoof it.
pub fn tunnel_client_ip(headers: &HeaderMap, hop: &str) -> Option<String> {
    if !TUNNEL_MODE.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let hop: std::net::IpAddr = hop.parse().ok()?;
    let private = match hop {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
    };
    if !private {
        return None;
    }
    let real: std::net::IpAddr = headers
        .get("cf-connecting-ip")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(real.to_string())
}

pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    trust_proxy: bool,
) -> String {
    let mut hop = peer.map(|addr| addr.ip().to_string());
    if trust_proxy {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
        {
            if let Some(first) = forwarded.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    hop = Some(ip.to_owned());
                }
            }
        }
    }
    if let Some(real) = hop
        .as_deref()
        .and_then(|hop| tunnel_client_ip(headers, hop))
    {
        return real;
    }
    hop.unwrap_or_else(|| "unknown".to_owned())
}

pub fn session_cookie(token: &str, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token.to_owned()))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(cookie::time::Duration::days(SESSION_DAYS))
        .build()
}

pub(crate) async fn session_user(state: &AppState, headers: &HeaderMap) -> Option<AuthUser> {
    let token = cookie_value(headers, SESSION_COOKIE)?;
    load_session(state, &token).await.ok()
}

pub fn clear_session_cookie(secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .max_age(cookie::time::Duration::seconds(0))
        .build()
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for item in Cookie::split_parse(header) {
        let Ok(cookie) = item else { continue };
        if cookie.name() == name {
            return Some(cookie.value().to_owned());
        }
    }
    None
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|info| info.0);
        let _ip = client_ip(&parts.headers, peer, state.config.trust_proxy);

        if let Some(token) = bearer(&parts.headers) {
            return load_token(state, &token).await;
        }
        let Some(token) = cookie_value(&parts.headers, SESSION_COOKIE) else {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "sign in required",
            ));
        };
        let user = load_session(state, &token).await?;
        if is_unsafe(&parts.method) {
            let provided = parts
                .headers
                .get("x-csrf-token")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let expected = user.csrf_token.as_deref().unwrap_or("");
            if !tokens_eq(provided, expected) {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "csrf",
                    "missing or invalid CSRF token",
                ));
            }
        }
        Ok(user)
    }
}

fn is_unsafe(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

async fn load_session(state: &AppState, token: &str) -> Result<AuthUser, ApiError> {
    let hash = sha256_hex(token);
    let row = sqlx::query!(
        r#"
        SELECT s.user_id, s.csrf_token, s.expires_at, u.email, u.display_name, u.role, u.totp_enabled, u.is_owner
        FROM sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1
        "#,
        hash,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "sign in required",
        ));
    };
    if row.expires_at <= Utc::now() {
        let _ = sqlx::query!("DELETE FROM sessions WHERE token_hash = $1", hash)
            .execute(&state.pool)
            .await;
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "session expired",
        ));
    }
    Ok(AuthUser {
        id: row.user_id,
        email: row.email,
        display_name: row.display_name,
        role: row.role,
        totp_enabled: row.totp_enabled,
        is_owner: row.is_owner,
        csrf_token: Some(row.csrf_token),
        scopes: vec!["read".into(), "write".into()],
        via_token: false,
        via_mcp: false,
    })
}

async fn load_token(state: &AppState, token: &str) -> Result<AuthUser, ApiError> {
    if token.starts_with(crate::mcp::ACCESS_PREFIX) {
        return crate::mcp::tokens::load_access(state, token).await;
    }
    let hash = sha256_hex(token);
    let row = sqlx::query!(
        r#"
        SELECT t.id, t.user_id, t.scopes, t.expires_at, u.email, u.display_name, u.role, u.totp_enabled, u.is_owner
        FROM api_tokens t
        JOIN users u ON u.id = t.user_id
        WHERE t.token_hash = $1
        "#,
        hash,
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let Some(row) = row else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid API token",
        ));
    };
    if row.expires_at.is_some_and(|when| when <= Utc::now()) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "API token expired",
        ));
    }
    let _ = sqlx::query!(
        "UPDATE api_tokens SET last_used_at = now() WHERE id = $1",
        row.id
    )
    .execute(&state.pool)
    .await;
    Ok(AuthUser {
        id: row.user_id,
        email: row.email,
        display_name: row.display_name,
        role: row.role,
        totp_enabled: row.totp_enabled,
        is_owner: row.is_owner,
        csrf_token: None,
        scopes: row.scopes,
        via_token: true,
        via_mcp: false,
    })
}

pub fn normalize_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim().to_lowercase();
    let valid = email.len() >= 3
        && email.len() <= 254
        && email.contains('@')
        && !email.contains(' ')
        && email
            .split('@')
            .nth(1)
            .is_some_and(|host| host.contains('.'));
    if valid {
        Ok(email)
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_email",
            "enter a valid email address",
        ))
    }
}

pub fn validate_password(password: &str) -> Result<(), ApiError> {
    if (12..=200).contains(&password.len()) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "weak_password",
            "password must be 12 to 200 characters",
        ))
    }
}

pub fn validate_scopes(scopes: &[String]) -> Result<Vec<String>, ApiError> {
    if scopes.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_scopes",
            "choose at least one scope",
        ));
    }
    let mut clean = Vec::new();
    for scope in scopes {
        if scope != "read" && scope != "write" {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_scopes",
                "scopes are read and write",
            ));
        }
        if !clean.iter().any(|existing: &String| existing == scope) {
            clean.push(scope.clone());
        }
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip_and_rejection() {
        let encoded = hash_password("correct-horse-battery").unwrap();
        assert!(verify_password("correct-horse-battery", &encoded));
        assert!(!verify_password("wrong-password-value", &encoded));
    }

    #[test]
    fn csrf_comparison_rejects_different_lengths() {
        assert!(tokens_eq("abc", "abc"));
        assert!(!tokens_eq("abc", "abd"));
        assert!(!tokens_eq("abc", "abcd"));
    }

    #[test]
    fn forwarded_for_is_honored_only_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.5, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&headers, None, true), "203.0.113.5");
        assert_eq!(client_ip(&headers, None, false), "unknown");
    }

    #[test]
    fn the_cloudflare_address_is_trusted_only_from_a_private_hop() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "198.51.100.7".parse().unwrap());
        headers.insert("x-forwarded-for", "172.18.0.5".parse().unwrap());
        // Off by default: the header is ignored.
        assert_eq!(client_ip(&headers, None, true), "172.18.0.5");
        set_tunnel_mode(true);
        assert_eq!(client_ip(&headers, None, true), "198.51.100.7");
        // A public hop reached Traefik directly and could have forged the header.
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&headers, None, true), "203.0.113.9");
        // A malformed value is ignored.
        headers.insert("x-forwarded-for", "172.18.0.5".parse().unwrap());
        headers.insert("cf-connecting-ip", "not-an-ip".parse().unwrap());
        assert_eq!(client_ip(&headers, None, true), "172.18.0.5");
        set_tunnel_mode(false);
    }
}
