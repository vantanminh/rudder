//! Cloudflare Zero Trust mode: tunnel containers, ingress, DNS planning,
//! Access token verification, and the script that makes `sshd` trust Cloudflare.
//!
//! Everything here is pure. The server owns the HTTP calls to Cloudflare.

use base64::Engine;
use rsa::pkcs1v15::Pkcs1v15Sign;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPublicKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{ContainerSpec, Error, PROXY_NETWORK, TRAEFIK_CONTAINER};

/// Prefix of the `cloudflared` containers Rudder runs. Replica `n` is `rudder-cloudflared-n`.
pub const CLOUDFLARED_CONTAINER_PREFIX: &str = "rudder-cloudflared";

/// Image run for each replica unless `RUDDER_CLOUDFLARED_IMAGE` says otherwise.
pub const DEFAULT_CLOUDFLARED_IMAGE: &str = "cloudflare/cloudflared:latest";

/// Most replicas Rudder starts on one server. Each opens four connections.
pub const MAX_TUNNEL_REPLICAS: u8 = 5;

/// Port `cloudflared` serves `/ready` and metrics on inside its container.
pub const CLOUDFLARED_METRICS_PORT: u16 = 2000;

/// Label that marks containers Rudder started for the tunnel.
pub const CLOUDFLARED_LABEL: (&str, &str) = ("rudder.managed", "cloudflared");

/// Name of replica `index` (counting from 1).
pub fn replica_container_name(index: u8) -> String {
    format!("{CLOUDFLARED_CONTAINER_PREFIX}-{index}")
}

pub fn validate_replicas(count: i64) -> Result<u8, Error> {
    if (1..=i64::from(MAX_TUNNEL_REPLICAS)).contains(&count) {
        Ok(count as u8)
    } else {
        Err(Error::Validation(format!(
            "replicas must be between 1 and {MAX_TUNNEL_REPLICAS}"
        )))
    }
}

/// Cloudflare account and zone ids are 32 hexadecimal characters.
pub fn validate_cloudflare_id(label: &str, value: &str) -> Result<(), Error> {
    if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(Error::Validation(format!(
            "{label} must be 32 hexadecimal characters"
        )))
    }
}

/// Reduce what a person pastes (`https://team.cloudflareaccess.com/`) to the bare host.
pub fn normalize_team_domain(input: &str) -> Result<String, Error> {
    let mut host = input.trim().to_ascii_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = host.strip_prefix(prefix) {
            host = rest.to_owned();
        }
    }
    let host = host.trim_end_matches('/');
    let Some(team) = host.strip_suffix(".cloudflareaccess.com") else {
        return Err(Error::Validation(
            "the team domain looks like your-team.cloudflareaccess.com".into(),
        ));
    };
    let valid = !team.is_empty()
        && team.len() <= 63
        && !team.starts_with('-')
        && !team.ends_with('-')
        && team
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(host.to_owned())
    } else {
        Err(Error::Validation("invalid team domain".into()))
    }
}

/// The `iss` claim Cloudflare puts in Access tokens for a team domain.
pub fn access_issuer(team_domain: &str) -> String {
    format!("https://{team_domain}")
}

/// Where a team publishes the public keys that sign Access tokens.
pub fn access_certs_url(team_domain: &str) -> String {
    format!("https://{team_domain}/cdn-cgi/access/certs")
}

/// Lowercase an email address and reject anything that is not plainly one.
pub fn validate_access_email(input: &str) -> Result<String, Error> {
    let email = input.trim().to_ascii_lowercase();
    let invalid = || Error::Validation(format!("'{input}' is not an email address"));
    let (local, domain) = email.split_once('@').ok_or_else(invalid)?;
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && local
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte));
    let domain_ok = domain.contains('.')
        && domain.len() <= 190
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.');
    if local_ok && domain_ok {
        Ok(email)
    } else {
        Err(invalid())
    }
}

/// A Unix account name that `sshd` would accept for a login.
pub fn validate_unix_user(user: &str) -> Result<(), Error> {
    let bytes = user.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 32
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(byte));
    if ok {
        Ok(())
    } else {
        Err(Error::Validation(format!(
            "'{user}' is not a Unix user name"
        )))
    }
}

/// `host` is `zone` itself or a name below it.
pub fn host_in_zone(host: &str, zone: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let zone = zone.to_ascii_lowercase();
    host == zone || host.ends_with(&format!(".{zone}"))
}

/// The container for one `cloudflared` replica. The tunnel token is passed as an
/// environment variable so it does not show up in the process list.
pub fn cloudflared_spec(token: &str, image: &str, index: u8) -> Result<ContainerSpec, Error> {
    let token_ok = !token.is_empty()
        && token.len() <= 4096
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_=.+/".contains(&byte));
    if !token_ok {
        return Err(Error::Validation("the tunnel token is not valid".into()));
    }
    crate::validate_image_ref(image)?;
    if index == 0 || index > MAX_TUNNEL_REPLICAS {
        return Err(Error::Validation("replica number is out of range".into()));
    }
    Ok(ContainerSpec {
        name: replica_container_name(index),
        image: image.to_owned(),
        env: vec![("TUNNEL_TOKEN".into(), token.to_owned())],
        command: vec![
            "tunnel".into(),
            "--no-autoupdate".into(),
            "--metrics".into(),
            format!("0.0.0.0:{CLOUDFLARED_METRICS_PORT}"),
            "run".into(),
        ],
        ports: Vec::new(),
        volumes: Vec::new(),
        memory_bytes: Some(192 * 1024 * 1024),
        nano_cpus: None,
        restart: "unless-stopped".into(),
        network: Some(PROXY_NETWORK.into()),
        labels: vec![(CLOUDFLARED_LABEL.0.into(), CLOUDFLARED_LABEL.1.into())],
    })
}

/// Where `cloudflared` sends requests: Traefik over HTTPS on the proxy network.
pub fn tunnel_origin() -> String {
    format!("https://{TRAEFIK_CONTAINER}:443")
}

/// A tunnel configuration body: one rule per hostname, then a catch-all 404.
///
/// Certificate verification is off because Traefik presents its own default
/// certificate on a network no one else can join; the hostname is passed as the
/// TLS server name so Traefik picks the right router.
pub fn tunnel_config(hosts: &[String]) -> Result<Value, Error> {
    let mut names: Vec<String> = Vec::new();
    for host in hosts {
        let host = host.trim().to_ascii_lowercase();
        crate::validate_host(&host)?;
        if !names.contains(&host) {
            names.push(host);
        }
    }
    names.sort();
    let origin = tunnel_origin();
    let mut ingress: Vec<Value> = names
        .iter()
        .map(|host| {
            json!({
                "hostname": host,
                "service": origin,
                "originRequest": { "noTLSVerify": true, "originServerName": host },
            })
        })
        .collect();
    ingress.push(json!({ "service": "http_status:404" }));
    Ok(json!({ "config": { "ingress": ingress } }))
}

/// The DNS name Cloudflare resolves to a tunnel.
pub fn tunnel_cname(tunnel_id: &str) -> String {
    format!("{tunnel_id}.cfargotunnel.com")
}

/// A DNS record that already exists in the zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRecord {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsAction {
    /// No record for the name: add the CNAME.
    Create { host: String },
    /// The CNAME to this tunnel is already there.
    Keep { host: String },
    /// A different record is there and the operator allowed replacing it.
    Replace {
        host: String,
        record_id: String,
        was: String,
    },
    /// A different record is there and was left alone.
    Conflict { host: String, was: String },
}

/// Decide what to do for each hostname. `existing` holds the records whose name
/// matches one of the hosts. Records Rudder cannot safely replace (MX, TXT, NS)
/// never block a CNAME of another type, but a CNAME cannot sit beside them, so
/// they are reported as conflicts.
pub fn plan_dns(
    hosts: &[String],
    tunnel_id: &str,
    existing: &[DnsRecord],
    overwrite: bool,
) -> Vec<DnsAction> {
    let target = tunnel_cname(tunnel_id);
    hosts
        .iter()
        .map(|host| {
            let records: Vec<&DnsRecord> = existing
                .iter()
                .filter(|record| record.name.eq_ignore_ascii_case(host))
                .collect();
            if records.is_empty() {
                return DnsAction::Create { host: host.clone() };
            }
            if records.iter().any(|record| {
                record.kind == "CNAME" && record.content.eq_ignore_ascii_case(&target)
            }) {
                return DnsAction::Keep { host: host.clone() };
            }
            let replaceable = records
                .iter()
                .find(|record| matches!(record.kind.as_str(), "A" | "AAAA" | "CNAME"));
            let was = records
                .iter()
                .map(|record| format!("{} {}", record.kind, record.content))
                .collect::<Vec<_>>()
                .join(", ");
            match replaceable {
                Some(record) if overwrite && records.len() == 1 => DnsAction::Replace {
                    host: host.clone(),
                    record_id: record.id.clone(),
                    was,
                },
                _ => DnsAction::Conflict {
                    host: host.clone(),
                    was,
                },
            }
        })
        .collect()
}

/// One line of the readiness report shown before the public ports are closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightCheck {
    pub id: &'static str,
    pub ok: bool,
    pub required: bool,
    pub message: String,
}

/// What the readiness report is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreflightInput {
    pub replicas_wanted: usize,
    pub replicas_running: usize,
    pub tunnel_connections: usize,
    pub tunnel_serving: bool,
    pub panel_domain_set: bool,
    pub hostnames_pending: usize,
    pub access_enforced: bool,
    pub ssh_ready: bool,
}

pub fn preflight(input: PreflightInput) -> Vec<PreflightCheck> {
    let check = |id, ok, required, good: &str, bad: &str| PreflightCheck {
        id,
        ok,
        required,
        message: if ok { good } else { bad }.to_owned(),
    };
    vec![
        check(
            "replicas",
            input.replicas_wanted > 0 && input.replicas_running >= input.replicas_wanted,
            true,
            "Every tunnel replica is running.",
            "Some tunnel replicas are not running.",
        ),
        check(
            "connections",
            input.tunnel_serving && input.tunnel_connections > 0,
            true,
            "Cloudflare reports the tunnel connected.",
            "Cloudflare does not report any connection for the tunnel.",
        ),
        check(
            "panel_domain",
            input.panel_domain_set,
            true,
            "The panel has a domain.",
            "Set a panel domain under Certificates first; without one the panel cannot be reached through the tunnel.",
        ),
        check(
            "published",
            input.hostnames_pending == 0,
            true,
            "Every domain is published through the tunnel.",
            "Some domains are not published through the tunnel yet. Press Publish.",
        ),
        check(
            "access",
            input.access_enforced,
            true,
            "Cloudflare Access protects the panel.",
            "Turn on Cloudflare Access for the panel before closing the ports.",
        ),
        check(
            "ssh",
            input.ssh_ready,
            false,
            "SSH is reachable through Cloudflare. Port 22 can be closed too.",
            "SSH through Cloudflare is not set up. Keep port 22 open.",
        ),
    ]
}

/// The ports can close when every required check passes.
pub fn can_close_ports(checks: &[PreflightCheck]) -> bool {
    checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok)
}

/// Accept the public key Cloudflare gives for its SSH CA.
pub fn validate_ssh_ca_public_key(input: &str) -> Result<String, Error> {
    let key = input.trim();
    let known = ["ecdsa-sha2-nistp256 ", "ssh-ed25519 ", "ssh-rsa "]
        .iter()
        .any(|prefix| key.starts_with(prefix));
    let clean = key.len() <= 2048
        && !key.contains('\n')
        && !key.contains('\r')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" +/=@._-".contains(&byte));
    if known && clean {
        Ok(key.to_owned())
    } else {
        Err(Error::Validation(
            "that is not an SSH certificate authority public key".into(),
        ))
    }
}

/// A POSIX shell script, run as root on the server, that makes `sshd` accept
/// certificates signed by Cloudflare's SSH CA. It writes the key, adds
/// `TrustedUserCAKeys` through a drop-in file when `sshd_config` includes the
/// drop-in directory (or to the main file otherwise), checks the result with
/// `sshd -t`, and reloads `sshd`. Nothing else about SSH is changed.
pub fn sshd_trust_script(ca_public_key: &str) -> Result<String, Error> {
    let key = validate_ssh_ca_public_key(ca_public_key)?;
    Ok(format!(
        r#"#!/bin/sh
# Written by Rudder. Makes sshd trust the Cloudflare SSH certificate authority.
set -eu
[ "$(id -u)" -eq 0 ] || {{ echo "run this as root" >&2; exit 1; }}
CA=/etc/ssh/rudder-cloudflare-ca.pub
cat >"$CA" <<'RUDDER_CA'
{key}
RUDDER_CA
chmod 644 "$CA"
CONF=/etc/ssh/sshd_config
if grep -Eq '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/' "$CONF" 2>/dev/null; then
	mkdir -p /etc/ssh/sshd_config.d
	TARGET=/etc/ssh/sshd_config.d/50-rudder-cloudflare.conf
	printf 'PubkeyAuthentication yes\nTrustedUserCAKeys %s\n' "$CA" >"$TARGET"
else
	TARGET="$CONF"
	if ! grep -q 'rudder-cloudflare-ca.pub' "$CONF"; then
		cp "$CONF" "$CONF.rudder-backup"
		printf 'PubkeyAuthentication yes\nTrustedUserCAKeys %s\n' "$CA" | cat - "$CONF.rudder-backup" >"$CONF"
	fi
fi
if ! sshd -t; then
	echo "sshd rejected the configuration; undoing" >&2
	if [ "$TARGET" = "$CONF" ] && [ -f "$CONF.rudder-backup" ]; then cp "$CONF.rudder-backup" "$CONF"; else rm -f "$TARGET"; fi
	exit 1
fi
if command -v systemctl >/dev/null 2>&1; then
	systemctl reload ssh 2>/dev/null || systemctl reload sshd
else
	service ssh reload 2>/dev/null || service sshd reload
fi
echo "sshd now trusts the Cloudflare SSH certificate authority"
"#
    ))
}

/// Claims Rudder reads from a Cloudflare Access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessClaims {
    pub email: Option<String>,
    pub subject: Option<String>,
    pub expires: i64,
}

const TOKEN_LEEWAY_SECS: i64 = 60;

/// Verify the `Cf-Access-Jwt-Assertion` header value against the team's published keys.
///
/// The signature must be RS256 by a key in `jwks`, the issuer must equal
/// `issuer`, `audience` must be listed in `aud`, and the token must not be
/// expired. Every failure returns the same message so a caller cannot learn
/// which check rejected it.
pub fn verify_access_jwt(
    token: &str,
    jwks: &str,
    issuer: &str,
    audience: &str,
    now_unix: i64,
) -> Result<AccessClaims, Error> {
    let rejected = || Error::Validation("invalid Cloudflare Access token".into());
    if token.is_empty() || token.len() > 8192 || audience.is_empty() {
        return Err(rejected());
    }
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(rejected());
    };
    let decode = |part: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .map_err(|_| rejected())
    };
    let header: Value = serde_json::from_slice(&decode(header_b64)?).map_err(|_| rejected())?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err(rejected());
    }
    let kid = header.get("kid").and_then(Value::as_str);
    let signature = decode(signature)?;
    let signing_input = format!("{header_b64}.{payload}");
    let digest = Sha256::digest(signing_input.as_bytes());
    let jwks: Value = serde_json::from_str(jwks).map_err(|_| rejected())?;
    let keys = jwks
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(rejected)?;
    let verified = keys
        .iter()
        .filter(|key| key.get("kty").and_then(Value::as_str) == Some("RSA"))
        .filter(|key| kid.is_none() || key.get("kid").and_then(Value::as_str) == kid)
        .filter_map(|key| {
            let n = key.get("n")?.as_str()?;
            let e = key.get("e")?.as_str()?;
            let n = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(n)
                .ok()?;
            let e = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(e)
                .ok()?;
            RsaPublicKey::new(BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e)).ok()
        })
        .any(|key| {
            key.size() >= 256
                && key
                    .verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &signature)
                    .is_ok()
        });
    if !verified {
        return Err(rejected());
    }
    let claims: Value = serde_json::from_slice(&decode(payload)?).map_err(|_| rejected())?;
    if claims.get("iss").and_then(Value::as_str) != Some(issuer) {
        return Err(rejected());
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(value)) => value == audience,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(audience)),
        _ => false,
    };
    let expires = claims
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(rejected)?;
    let not_before = claims.get("nbf").and_then(Value::as_i64);
    if !audience_ok
        || now_unix > expires.saturating_add(TOKEN_LEEWAY_SECS)
        || not_before.is_some_and(|nbf| nbf > now_unix.saturating_add(TOKEN_LEEWAY_SECS))
    {
        return Err(rejected());
    }
    Ok(AccessClaims {
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        subject: claims.get("sub").and_then(Value::as_str).map(str::to_owned),
        expires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::RsaPrivateKey;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn signed_token(claims: &Value, kid: &str) -> (String, String) {
        let private =
            RsaPrivateKey::from_pkcs8_pem(crate::github_app::tests::TEST_APP_KEY).unwrap();
        let public = private.to_public_key();
        let header = b64(json!({ "alg": "RS256", "kid": kid }).to_string().as_bytes());
        let payload = b64(claims.to_string().as_bytes());
        let input = format!("{header}.{payload}");
        let signature = private
            .sign(
                Pkcs1v15Sign::new::<Sha256>(),
                &Sha256::digest(input.as_bytes()),
            )
            .unwrap();
        let jwks = json!({ "keys": [{
            "kid": kid, "kty": "RSA", "alg": "RS256", "use": "sig",
            "n": b64(&public.n().to_bytes_be()),
            "e": b64(&public.e().to_bytes_be()),
        }] });
        (format!("{input}.{}", b64(&signature)), jwks.to_string())
    }

    const ISSUER: &str = "https://acme.cloudflareaccess.com";

    fn claims(exp: i64) -> Value {
        json!({ "iss": ISSUER, "aud": ["tag-1"], "exp": exp, "email": "ops@acme.dev", "sub": "u1" })
    }

    #[test]
    fn a_valid_access_token_is_accepted() {
        let (token, jwks) = signed_token(&claims(2_000), "k1");
        let parsed = verify_access_jwt(&token, &jwks, ISSUER, "tag-1", 1_000).unwrap();
        assert_eq!(parsed.email.as_deref(), Some("ops@acme.dev"));
        assert_eq!(parsed.expires, 2_000);
    }

    #[test]
    fn access_tokens_fail_on_every_wrong_detail() {
        let (token, jwks) = signed_token(&claims(2_000), "k1");
        assert!(verify_access_jwt(&token, &jwks, ISSUER, "other", 1_000).is_err());
        assert!(verify_access_jwt(
            &token,
            &jwks,
            "https://evil.cloudflareaccess.com",
            "tag-1",
            1_000
        )
        .is_err());
        assert!(verify_access_jwt(&token, &jwks, ISSUER, "tag-1", 2_061).is_err());
        assert!(verify_access_jwt(&token, &jwks, ISSUER, "", 1_000).is_err());
        assert!(verify_access_jwt("", &jwks, ISSUER, "tag-1", 1_000).is_err());
        assert!(verify_access_jwt("a.b", &jwks, ISSUER, "tag-1", 1_000).is_err());
        assert!(verify_access_jwt(&token, "{}", ISSUER, "tag-1", 1_000).is_err());
        // Another key id is not trusted even though the key itself would verify.
        let (other, _) = signed_token(&claims(2_000), "k2");
        assert!(verify_access_jwt(&other, &jwks, ISSUER, "tag-1", 1_000).is_err());
        // A changed payload no longer matches the signature.
        let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
        parts[1] = b64(claims(9_999_999).to_string().as_bytes());
        assert!(verify_access_jwt(&parts.join("."), &jwks, ISSUER, "tag-1", 1_000).is_err());
    }

    #[test]
    fn unsigned_and_hmac_tokens_are_refused() {
        let (_, jwks) = signed_token(&claims(2_000), "k1");
        let header = b64(br#"{"alg":"none"}"#);
        let payload = b64(claims(2_000).to_string().as_bytes());
        assert!(verify_access_jwt(
            &format!("{header}.{payload}."),
            &jwks,
            ISSUER,
            "tag-1",
            1_000
        )
        .is_err());
        let header = b64(br#"{"alg":"HS256","kid":"k1"}"#);
        assert!(verify_access_jwt(
            &format!("{header}.{payload}.{}", b64(b"sig")),
            &jwks,
            ISSUER,
            "tag-1",
            1_000
        )
        .is_err());
    }

    #[test]
    fn team_domains_are_normalized() {
        assert_eq!(
            normalize_team_domain(" https://Acme.cloudflareaccess.com/ ").unwrap(),
            "acme.cloudflareaccess.com"
        );
        assert_eq!(
            normalize_team_domain("acme.cloudflareaccess.com").unwrap(),
            "acme.cloudflareaccess.com"
        );
        for bad in [
            "acme.example.com",
            ".cloudflareaccess.com",
            "-a.cloudflareaccess.com",
            "a_b.cloudflareaccess.com",
            "evil.com/.cloudflareaccess.com",
            "",
        ] {
            assert!(normalize_team_domain(bad).is_err(), "{bad}");
        }
        assert_eq!(
            access_issuer("a.cloudflareaccess.com"),
            "https://a.cloudflareaccess.com"
        );
    }

    #[test]
    fn emails_users_ids_and_replicas_are_validated() {
        assert_eq!(
            validate_access_email(" Ops@Acme.dev ").unwrap(),
            "ops@acme.dev"
        );
        for bad in [
            "",
            "ops",
            "ops@",
            "@acme.dev",
            "a b@acme.dev",
            "ops@acme",
            "ops@acme..dev",
            "o\"ps@acme.dev",
        ] {
            assert!(validate_access_email(bad).is_err(), "{bad}");
        }
        for ok in ["root", "deploy", "_apt", "ops-1"] {
            validate_unix_user(ok).unwrap();
        }
        let long = "a".repeat(33);
        for bad in ["", "Root", "1ops", "a b", "a;b", long.as_str()] {
            assert!(validate_unix_user(bad).is_err(), "{bad}");
        }
        let id = "0123456789abcdef0123456789abcdef";
        validate_cloudflare_id("account id", id).unwrap();
        assert!(validate_cloudflare_id("account id", "short").is_err());
        assert!(validate_cloudflare_id("account id", &"g".repeat(32)).is_err());
        assert_eq!(validate_replicas(1).unwrap(), 1);
        assert_eq!(validate_replicas(5).unwrap(), 5);
        assert!(validate_replicas(0).is_err());
        assert!(validate_replicas(6).is_err());
    }

    #[test]
    fn hosts_are_matched_against_the_zone() {
        assert!(host_in_zone("app.example.com", "example.com"));
        assert!(host_in_zone("Example.com", "example.com"));
        assert!(!host_in_zone("badexample.com", "example.com"));
        assert!(!host_in_zone("example.com.evil.dev", "example.com"));
    }

    #[test]
    fn a_replica_container_hides_the_token_from_the_command_line() {
        let spec = cloudflared_spec("eyJhIjoiabc-_=", DEFAULT_CLOUDFLARED_IMAGE, 2).unwrap();
        assert_eq!(spec.name, "rudder-cloudflared-2");
        assert_eq!(spec.network.as_deref(), Some("rudder-proxy"));
        assert!(spec.ports.is_empty());
        assert!(spec.command.iter().all(|part| !part.contains("eyJ")));
        assert_eq!(
            spec.env,
            vec![("TUNNEL_TOKEN".to_owned(), "eyJhIjoiabc-_=".to_owned())]
        );
        assert!(cloudflared_spec("", DEFAULT_CLOUDFLARED_IMAGE, 1).is_err());
        assert!(cloudflared_spec("a b", DEFAULT_CLOUDFLARED_IMAGE, 1).is_err());
        assert!(cloudflared_spec("abc", DEFAULT_CLOUDFLARED_IMAGE, 0).is_err());
        assert!(cloudflared_spec("abc", DEFAULT_CLOUDFLARED_IMAGE, 6).is_err());
        assert!(cloudflared_spec("abc", "bad image", 1).is_err());
    }

    #[test]
    fn the_tunnel_config_lists_each_host_once_and_ends_with_a_404() {
        let config = tunnel_config(&[
            "b.example.com".into(),
            "A.example.com".into(),
            "b.example.com".into(),
        ])
        .unwrap();
        let ingress = config["config"]["ingress"].as_array().unwrap();
        assert_eq!(ingress.len(), 3);
        assert_eq!(ingress[0]["hostname"], "a.example.com");
        assert_eq!(ingress[0]["service"], "https://rudder-traefik:443");
        assert_eq!(ingress[0]["originRequest"]["noTLSVerify"], true);
        assert_eq!(
            ingress[0]["originRequest"]["originServerName"],
            "a.example.com"
        );
        assert_eq!(ingress[1]["hostname"], "b.example.com");
        assert_eq!(ingress[2], json!({ "service": "http_status:404" }));
        assert!(tunnel_config(&["not a host".into()]).is_err());
        assert!(tunnel_config(&["*.example.com".into()]).is_err());
        let empty = tunnel_config(&[]).unwrap();
        assert_eq!(empty["config"]["ingress"].as_array().unwrap().len(), 1);
    }

    fn record(id: &str, name: &str, kind: &str, content: &str) -> DnsRecord {
        DnsRecord {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            content: content.into(),
        }
    }

    #[test]
    fn dns_changes_never_replace_a_record_unless_asked() {
        let hosts: Vec<String> = [
            "new.example.com",
            "ok.example.com",
            "old.example.com",
            "mail.example.com",
        ]
        .iter()
        .map(|host| (*host).to_owned())
        .collect();
        let existing = vec![
            record("1", "ok.example.com", "CNAME", "TUN.cfargotunnel.com"),
            record("2", "old.example.com", "A", "203.0.113.5"),
            record("3", "mail.example.com", "A", "203.0.113.5"),
            record("4", "mail.example.com", "MX", "mx.example.com"),
        ];
        let kept = plan_dns(&hosts, "TUN", &existing, false);
        assert_eq!(
            kept[0],
            DnsAction::Create {
                host: "new.example.com".into()
            }
        );
        assert_eq!(
            kept[1],
            DnsAction::Keep {
                host: "ok.example.com".into()
            }
        );
        assert!(matches!(&kept[2], DnsAction::Conflict { was, .. } if was == "A 203.0.113.5"));
        assert!(matches!(&kept[3], DnsAction::Conflict { .. }));
        let replaced = plan_dns(&hosts, "TUN", &existing, true);
        assert_eq!(
            replaced[2],
            DnsAction::Replace {
                host: "old.example.com".into(),
                record_id: "2".into(),
                was: "A 203.0.113.5".into()
            }
        );
        // Two records for one name are too ambiguous to replace automatically.
        assert!(matches!(&replaced[3], DnsAction::Conflict { .. }));
    }

    #[test]
    fn closing_the_ports_needs_every_required_check() {
        let good = PreflightInput {
            replicas_wanted: 2,
            replicas_running: 2,
            tunnel_connections: 8,
            tunnel_serving: true,
            panel_domain_set: true,
            hostnames_pending: 0,
            access_enforced: true,
            ssh_ready: false,
        };
        let checks = preflight(good);
        assert!(can_close_ports(&checks));
        assert!(!checks.iter().find(|check| check.id == "ssh").unwrap().ok);
        for broken in [
            PreflightInput {
                replicas_running: 1,
                ..good
            },
            PreflightInput {
                tunnel_connections: 0,
                ..good
            },
            PreflightInput {
                tunnel_serving: false,
                ..good
            },
            PreflightInput {
                panel_domain_set: false,
                ..good
            },
            PreflightInput {
                hostnames_pending: 1,
                ..good
            },
            PreflightInput {
                access_enforced: false,
                ..good
            },
            PreflightInput::default(),
        ] {
            assert!(!can_close_ports(&preflight(broken)), "{broken:?}");
        }
    }

    #[test]
    fn the_sshd_script_embeds_only_a_valid_key() {
        let key = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBExample= open-ssh-ca@cloudflareaccess.org";
        let script = sshd_trust_script(key).unwrap();
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains(key));
        assert!(script.contains("TrustedUserCAKeys"));
        assert!(script.contains("sshd -t"));
        for bad in [
            "",
            "ssh-ed25519",
            "curl evil | sh",
            "ecdsa-sha2-nistp256 AAAA\nrm -rf /",
            "ecdsa-sha2-nistp256 AAAA$(id)",
            "ecdsa-sha2-nistp256 AAAA`id`",
            "ecdsa-sha2-nistp256 AAAA'; id; echo '",
        ] {
            assert!(sshd_trust_script(bad).is_err(), "{bad}");
        }
    }
}
