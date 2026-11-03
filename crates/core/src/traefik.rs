//! Traefik v3 dynamic configuration.
//!
//! Routes are file-provider routers. Custom certificates are listed in the
//! TLS store so Traefik selects them by SNI, including wildcards.

use crate::Error;

/// Docker network shared by Traefik and routed containers.
pub const PROXY_NETWORK: &str = "rudder-proxy";

/// Name of the per-server Traefik container. Its JSON access log is collected as `proxy` logs.
pub const TRAEFIK_CONTAINER: &str = "rudder-traefik";

/// Path inside the Traefik container. The install mounts the panel data directory here.
pub const TRAEFIK_CERT_DIR: &str = "/etc/rudder/traefik/certs";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    None,
    LetsEncrypt,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRoute {
    pub name: String,
    pub host: String,
    pub path_prefix: String,
    pub container: String,
    pub port: u16,
    pub https_redirect: bool,
    pub tls: TlsMode,
    pub basic_auth_user: Option<String>,
    pub basic_auth_hash: Option<String>,
    /// Ask the panel before Traefik forwards the request.
    pub gate: bool,
    /// Also publish `/_rudder` on this host so Access can set a cookie.
    pub access_handoff: bool,
}

/// How Traefik reaches the panel for the shared gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeGate {
    pub secret: String,
    pub panel_container: String,
    pub panel_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsMaterial {
    pub cert_file: String,
    pub key_file: String,
}

pub fn validate_host(host: &str) -> Result<(), Error> {
    if host.len() > 253 || host.is_empty() || host.contains('*') {
        return Err(Error::Validation("invalid host".into()));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(Error::Validation("host needs a dot".into()));
    }
    for label in labels {
        if label.is_empty() || label.len() > 63 {
            return Err(Error::Validation("invalid host".into()));
        }
        let bytes = label.as_bytes();
        if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
            return Err(Error::Validation("invalid host".into()));
        }
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Err(Error::Validation("invalid host".into()));
        }
    }
    Ok(())
}

pub fn validate_path_prefix(path: &str) -> Result<(), Error> {
    if path == "/" {
        return Ok(());
    }
    if path.len() > 200
        || !path.starts_with('/')
        || path.contains("//")
        || path.split('/').any(|part| part == "..")
        || !path.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~')
        })
    {
        return Err(Error::Validation("invalid path prefix".into()));
    }
    Ok(())
}

pub fn preview_hostname(service: &str, nonce: &str, public_ip: &str) -> Result<String, Error> {
    crate::validate_name(service)?;
    if nonce.len() != 8 || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Validation(
            "preview nonce must be 8 hex characters".into(),
        ));
    }
    let ip = if public_ip.is_empty() {
        "127.0.0.1"
    } else {
        public_ip
    };
    if !is_ipv4(ip) {
        return Err(Error::Validation(
            "preview domains need an IPv4 public address".into(),
        ));
    }
    let prefix: String = service.chars().take(20).collect();
    let nonce = nonce.to_ascii_lowercase();
    let host = format!("{prefix}-{nonce}.{ip}.sslip.io");
    validate_host(&host)?;
    Ok(host)
}

fn is_ipv4(value: &str) -> bool {
    let octets: Vec<&str> = value.split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    octets.iter().all(|octet| {
        !octet.is_empty()
            && octet.len() <= 3
            && octet.bytes().all(|byte| byte.is_ascii_digit())
            && octet.parse::<u8>().is_ok()
            && !(octet.len() > 1 && octet.starts_with('0'))
    })
}

/// How traffic reaches Traefik on a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeMode {
    /// Visitors connect to ports 80 and 443 directly. Routes follow their SSL mode.
    #[default]
    Direct,
    /// Only `cloudflared` connects, over HTTPS on the proxy network. Every route is
    /// served on `websecure` with the default certificate store: Cloudflare shows the
    /// public certificate, no ACME request is made, and nothing redirects to HTTPS.
    Tunnel,
}

/// Render the file-provider document. Inputs are validated so the YAML cannot be injected.
pub fn render_dynamic_config(
    routes: &[ProxyRoute],
    certs: &[TlsMaterial],
    gate: Option<&EdgeGate>,
) -> Result<String, Error> {
    render_dynamic_config_for(routes, certs, gate, EdgeMode::Direct)
}

/// Like [`render_dynamic_config`], for a server reached directly or through a tunnel.
pub fn render_dynamic_config_for(
    routes: &[ProxyRoute],
    certs: &[TlsMaterial],
    gate: Option<&EdgeGate>,
    mode: EdgeMode,
) -> Result<String, Error> {
    let tunnel = mode == EdgeMode::Tunnel;
    // Traefik's file provider rejects empty sections such as `middlewares: {}`,
    // so only sections with entries are written.
    let needs_gate = routes
        .iter()
        .any(|route| route.gate || route.access_handoff);
    if needs_gate {
        let Some(gate) = gate else {
            return Err(Error::Validation(
                "a gated route needs the panel gate settings".into(),
            ));
        };
        validate_gate(gate)?;
    }
    let mut routers = String::new();
    let mut services = String::new();
    let mut middlewares = String::new();
    let mut handoff_hosts = std::collections::BTreeSet::new();
    let mut needs_panel = false;
    for route in routes {
        validate_route(route)?;
        let rule = format!(
            "Host(`{}`) && PathPrefix(`{}`)",
            route.host, route.path_prefix
        );
        routers.push_str(&format!("    {}:\n", route.name));
        routers.push_str(&format!("      rule: \"{rule}\"\n"));
        routers.push_str(&format!("      service: {}\n", route.name));
        let tls = if tunnel { TlsMode::Custom } else { route.tls };
        write_tls(&mut routers, tls);
        let mut middleware_names = Vec::new();
        if route.gate {
            middleware_names.push("rudder-gate".to_owned());
        }
        if let (Some(user), Some(hash)) = (&route.basic_auth_user, &route.basic_auth_hash) {
            middleware_names.push(format!("{}-auth", route.name));
            middlewares.push_str(&format!("    {}-auth:\n", route.name));
            middlewares.push_str("      basicAuth:\n        users:\n");
            middlewares.push_str(&format!("          - \"{user}:{hash}\"\n"));
        }
        write_middlewares(&mut routers, &middleware_names);
        if route.https_redirect && route.tls != TlsMode::None && !tunnel {
            routers.push_str(&format!("    {}-http:\n", route.name));
            routers.push_str(&format!("      rule: \"{rule}\"\n"));
            routers.push_str("      entryPoints:\n        - web\n");
            routers.push_str(&format!(
                "      middlewares:\n        - {}-https\n",
                route.name
            ));
            routers.push_str(&format!("      service: {}\n", route.name));
            middlewares.push_str(&format!("    {}-https:\n", route.name));
            middlewares.push_str(
                "      redirectScheme:\n        scheme: https\n        permanent: true\n",
            );
        }
        if route.access_handoff && handoff_hosts.insert(route.host.clone()) {
            needs_panel = true;
            let name = format!("{}cb", route.name);
            crate::validate_name(&name)?;
            routers.push_str(&format!("    {name}:\n"));
            routers.push_str(&format!(
                "      rule: \"Host(`{}`) && PathPrefix(`/_rudder`)\"\n",
                route.host
            ));
            routers.push_str("      priority: 1000\n");
            routers.push_str("      service: rudder-panel\n");
            write_tls(&mut routers, route.tls);
        }
        services.push_str(&format!("    {}:\n", route.name));
        services.push_str("      loadBalancer:\n        servers:\n");
        services.push_str(&format!(
            "          - url: \"http://{}:{}\"\n",
            route.container, route.port
        ));
    }
    if routes.iter().any(|route| route.gate) {
        let gate = gate.expect("checked above");
        middlewares.push_str("    rudder-gate:\n");
        middlewares.push_str("      forwardAuth:\n");
        middlewares.push_str(&format!(
            "        address: \"http://{}:{}/api/v1/edge/gate?k={}\"\n",
            gate.panel_container, gate.panel_port, gate.secret
        ));
        middlewares.push_str("        trustForwardHeader: true\n");
        middlewares.push_str("        authResponseHeaders:\n");
        middlewares.push_str("          - X-Rudder-Access-Email\n");
        middlewares.push_str("          - X-Rudder-Access-User\n");
        middlewares.push_str("          - X-Rudder-Access-Departments\n");
        middlewares.push_str("          - X-Rudder-Access-Ranks\n");
    }
    if needs_panel {
        let gate = gate.expect("checked above");
        services.push_str("    rudder-panel:\n");
        services.push_str("      loadBalancer:\n        servers:\n");
        services.push_str(&format!(
            "          - url: \"http://{}:{}\"\n",
            gate.panel_container, gate.panel_port
        ));
    }
    let mut yaml = String::from("# Written by Rudder. Changes are overwritten.\n");
    if !routers.is_empty() {
        yaml.push_str("http:\n  routers:\n");
        yaml.push_str(&routers);
        yaml.push_str("  services:\n");
        yaml.push_str(&services);
        if !middlewares.is_empty() {
            yaml.push_str("  middlewares:\n");
            yaml.push_str(&middlewares);
        }
    }
    if !certs.is_empty() {
        yaml.push_str("tls:\n  certificates:\n");
    }
    for cert in certs {
        validate_cert_path(&cert.cert_file)?;
        validate_cert_path(&cert.key_file)?;
        yaml.push_str(&format!("    - certFile: {}\n", cert.cert_file));
        yaml.push_str(&format!("      keyFile: {}\n", cert.key_file));
    }
    Ok(yaml)
}

fn write_tls(routers: &mut String, tls: TlsMode) {
    match tls {
        TlsMode::None => {
            routers.push_str("      entryPoints:\n        - web\n");
        }
        TlsMode::LetsEncrypt => {
            routers.push_str("      entryPoints:\n        - websecure\n");
            routers.push_str("      tls:\n        certResolver: letsencrypt\n");
        }
        TlsMode::Custom => {
            routers.push_str("      entryPoints:\n        - websecure\n");
            routers.push_str("      tls: {}\n");
        }
    }
}

fn write_middlewares(routers: &mut String, names: &[String]) {
    if names.is_empty() {
        return;
    }
    routers.push_str("      middlewares:\n");
    for name in names {
        routers.push_str(&format!("        - {name}\n"));
    }
}

fn validate_gate(gate: &EdgeGate) -> Result<(), Error> {
    crate::validate_name(&gate.panel_container)?;
    if gate.panel_port == 0 {
        return Err(Error::Validation("panel port is invalid".into()));
    }
    let ok = (32..=128).contains(&gate.secret.len())
        && gate.secret.bytes().all(|byte| byte.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(Error::Validation(
            "gate secret must be 32 to 128 hex characters".into(),
        ))
    }
}

fn validate_route(route: &ProxyRoute) -> Result<(), Error> {
    crate::validate_name(&route.name)?;
    crate::validate_name(&route.container)?;
    validate_host(&route.host)?;
    validate_path_prefix(&route.path_prefix)?;
    if route.port == 0 {
        return Err(Error::Validation(
            "target port must be between 1 and 65535".into(),
        ));
    }
    if route.basic_auth_user.is_some() != route.basic_auth_hash.is_some() {
        return Err(Error::Validation(
            "basic auth needs both a user and a hash".into(),
        ));
    }
    if let Some(user) = &route.basic_auth_user {
        if user.is_empty()
            || user.len() > 64
            || !user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(Error::Validation("invalid basic auth user".into()));
        }
    }
    if let Some(hash) = &route.basic_auth_hash {
        if hash.len() < 20
            || hash.len() > 128
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'$'))
        {
            return Err(Error::Validation("invalid basic auth hash".into()));
        }
    }
    Ok(())
}

fn validate_cert_path(path: &str) -> Result<(), Error> {
    if path.starts_with(TRAEFIK_CERT_DIR)
        && !path.contains("..")
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_'))
    {
        Ok(())
    } else {
        Err(Error::Validation("invalid certificate path".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(tls: TlsMode) -> ProxyRoute {
        ProxyRoute {
            name: "rapp".into(),
            host: "app.example.com".into(),
            path_prefix: "/api".into(),
            container: "app-1".into(),
            port: 8080,
            https_redirect: true,
            tls,
            basic_auth_user: None,
            basic_auth_hash: None,
            gate: false,
            access_handoff: false,
        }
    }

    #[test]
    fn a_tunnel_serves_every_route_on_websecure_without_acme_or_redirects() {
        let mut http_only = route(TlsMode::None);
        http_only.name = "rplain".into();
        http_only.host = "plain.example.com".into();
        let mut gated = route(TlsMode::LetsEncrypt);
        gated.name = "rgate".into();
        gated.host = "gate.example.com".into();
        gated.access_handoff = true;
        let gate = EdgeGate {
            secret: "ab".repeat(16),
            panel_container: "rudder".into(),
            panel_port: 3000,
        };
        let routes = [route(TlsMode::LetsEncrypt), http_only, gated];
        let direct = render_dynamic_config(&routes, &[], Some(&gate)).unwrap();
        assert!(direct.contains("certResolver: letsencrypt"));
        assert!(direct.contains("redirectScheme"));
        let tunneled =
            render_dynamic_config_for(&routes, &[], Some(&gate), EdgeMode::Tunnel).unwrap();
        assert!(!tunneled.contains("certResolver"));
        assert!(!tunneled.contains("redirectScheme"));
        assert!(!tunneled.contains(
            "- web
"
        ));
        assert!(!tunneled.contains("-http:"));
        assert_eq!(tunneled.matches("- websecure").count(), 4);
        assert_eq!(tunneled.matches("tls: {}").count(), 4);
        assert!(tunneled.contains("PathPrefix(`/_rudder`)"));
    }

    #[test]
    fn letsencrypt_uses_the_resolver_and_redirects() {
        let yaml = render_dynamic_config(&[route(TlsMode::LetsEncrypt)], &[], None).unwrap();
        assert!(yaml.contains("Host(`app.example.com`) && PathPrefix(`/api`)"));
        assert!(yaml.contains("certResolver: letsencrypt"));
        assert!(yaml.contains("redirectScheme:"));
        assert!(yaml.contains("http://app-1:8080"));
        assert!(!yaml.contains("certFile:"));
    }

    #[test]
    fn custom_certificates_are_listed_for_sni() {
        let yaml = render_dynamic_config(
            &[route(TlsMode::Custom)],
            &[TlsMaterial {
                cert_file: format!("{TRAEFIK_CERT_DIR}/cert.crt"),
                key_file: format!("{TRAEFIK_CERT_DIR}/cert.key"),
            }],
            None,
        )
        .unwrap();
        assert!(yaml.contains("tls: {}"));
        assert!(yaml.contains(&format!("certFile: {TRAEFIK_CERT_DIR}/cert.crt")));
        assert!(!yaml.contains("certResolver"));
    }

    #[test]
    fn hosts_and_paths_reject_injection() {
        assert!(validate_host("app.example.com").is_ok());
        assert!(validate_host("bad`host.example").is_err());
        assert!(validate_host("*.example.com").is_err());
        assert!(validate_path_prefix("/").is_ok());
        assert!(validate_path_prefix("/api").is_ok());
        assert!(validate_path_prefix("/../etc").is_err());
    }

    #[test]
    fn access_gate_and_handoff_are_written_once_per_host() {
        let mut app = route(TlsMode::LetsEncrypt);
        app.gate = true;
        app.access_handoff = true;
        let mut other = route(TlsMode::LetsEncrypt);
        other.name = "other".into();
        other.host = "other.example.com".into();
        other.gate = true;
        other.access_handoff = true;
        let gate = EdgeGate {
            secret: "ab".repeat(16),
            panel_container: "rudder".into(),
            panel_port: 3000,
        };
        let yaml = render_dynamic_config(&[app, other], &[], Some(&gate)).unwrap();
        assert!(yaml.contains("forwardAuth:"));
        assert!(yaml.contains("/api/v1/edge/gate?k=abababababababababababababababab"));
        assert!(yaml.contains("PathPrefix(`/_rudder`)"));
        assert!(yaml.contains("http://rudder:3000"));
        assert_eq!(yaml.matches("rudder-gate:").count(), 1);
        assert!(render_dynamic_config(
            &[route(TlsMode::None)],
            &[],
            Some(&EdgeGate {
                secret: "not-hex".into(),
                panel_container: "rudder".into(),
                panel_port: 3000,
            })
        )
        .is_ok());
        let mut gated = route(TlsMode::None);
        gated.gate = true;
        assert!(render_dynamic_config(&[gated], &[], None).is_err());
    }

    #[test]
    fn preview_host_uses_sslip() {
        let host = preview_hostname("web", "ab12cd34", "203.0.113.8").unwrap();
        assert_eq!(host, "web-ab12cd34.203.0.113.8.sslip.io");
        assert!(preview_hostname("web", "short", "203.0.113.8").is_err());
        assert!(preview_hostname("web", "ab12cd34", "not-an-ip").is_err());
    }
}
