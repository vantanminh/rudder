//! Permission scopes and ready-made templates for AI agents.
//!
//! A scope is one area of the panel plus read or write. Templates are the
//! choices shown when someone approves an agent. **Full control** is every
//! scope, including writes. Adding a panel area means adding a scope here and
//! including it in full control; the server test rejects a documented route
//! whose scope is missing.

/// One permission an agent grant can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpScope {
    pub id: &'static str,
    pub group: &'static str,
    /// `false` for read, `true` for write.
    pub write: bool,
    pub summary: &'static str,
}

/// A ready-made set of scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpTemplate {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
}

const GROUPS: &[(&str, &str)] = &[
    ("auth", "Users, sessions, API tokens, and the audit log"),
    ("catalog", "Servers, projects, environments, and services"),
    ("agent", "Agent install tokens"),
    ("edge", "Domains, certificates, registries, and deploys"),
    ("source", "Git providers and builds"),
    ("databases", "Managed databases"),
    ("backups", "Backups and S3 destinations"),
    ("stacks", "Compose stacks and app templates"),
    ("cache", "HTTP cache policies, purge, and stats"),
    ("logs", "Logs and retention"),
    ("monitoring", "Metrics, alerts, and notifications"),
    ("system", "Panel settings, load, and updates"),
    (
        "access",
        "Rudder Access people, departments, and app sign-in",
    ),
    ("security", "Security checks, SSL activity, and rate limits"),
    (
        "cloudflare",
        "Cloudflare tunnel, Access, and SSH for the optional Zero Trust mode",
    ),
];

/// Every scope an agent can be given. Order is the order shown in the panel.
pub static MCP_SCOPES: &[McpScope] = &build_scopes();

const fn build_scopes() -> [McpScope; 30] {
    let mut scopes = [McpScope {
        id: "",
        group: "",
        write: false,
        summary: "",
    }; 30];
    let mut index = 0;
    let mut group_index = 0;
    while group_index < GROUPS.len() {
        let (group, summary) = GROUPS[group_index];
        scopes[index] = McpScope {
            id: read_id(group),
            group,
            write: false,
            summary,
        };
        index += 1;
        scopes[index] = McpScope {
            id: write_id(group),
            group,
            write: true,
            summary,
        };
        index += 1;
        group_index += 1;
    }
    scopes
}

const fn read_id(group: &'static str) -> &'static str {
    match group.as_bytes() {
        b"auth" => "auth:read",
        b"catalog" => "catalog:read",
        b"agent" => "agent:read",
        b"edge" => "edge:read",
        b"source" => "source:read",
        b"databases" => "databases:read",
        b"backups" => "backups:read",
        b"stacks" => "stacks:read",
        b"cache" => "cache:read",
        b"logs" => "logs:read",
        b"monitoring" => "monitoring:read",
        b"system" => "system:read",
        b"access" => "access:read",
        b"security" => "security:read",
        b"cloudflare" => "cloudflare:read",
        _ => "",
    }
}

const fn write_id(group: &'static str) -> &'static str {
    match group.as_bytes() {
        b"auth" => "auth:write",
        b"catalog" => "catalog:write",
        b"agent" => "agent:write",
        b"edge" => "edge:write",
        b"source" => "source:write",
        b"databases" => "databases:write",
        b"backups" => "backups:write",
        b"stacks" => "stacks:write",
        b"cache" => "cache:write",
        b"logs" => "logs:write",
        b"monitoring" => "monitoring:write",
        b"system" => "system:write",
        b"access" => "access:write",
        b"security" => "security:write",
        b"cloudflare" => "cloudflare:write",
        _ => "",
    }
}

/// Choices on the approval screen. `custom` is not listed; it means the
/// person picked scopes one by one.
pub const MCP_TEMPLATES: &[McpTemplate] = &[
    McpTemplate {
        id: "full",
        name: "Full control",
        summary: "Read and change every setting and all data.",
    },
    McpTemplate {
        id: "read-all",
        name: "Read everything",
        summary: "Read every setting and all data. Cannot change anything.",
    },
    McpTemplate {
        id: "operator",
        name: "Operate",
        summary: "Deploy and manage apps, Access, security, caches, logs, and monitors. Cannot change users, tokens, or panel updates.",
    },
    McpTemplate {
        id: "observer",
        name: "Watch",
        summary: "Read health, logs, metrics, cache stats, and security checks.",
    },
];

const OPERATOR_WRITE: &[&str] = &[
    "catalog",
    "agent",
    "edge",
    "source",
    "databases",
    "stacks",
    "cache",
    "logs",
    "monitoring",
    "access",
    "security",
];
const OPERATOR_READ_ONLY: &[&str] = &["backups", "system", "cloudflare"];
const OBSERVER: &[&str] = &[
    "catalog",
    "logs",
    "monitoring",
    "cache",
    "system",
    "security",
];

/// Scopes for a ready-made template. `None` when `id` is not a template
/// (`custom` is resolved separately).
pub fn scopes_for_template(id: &str) -> Option<Vec<&'static str>> {
    let scopes: Vec<&'static str> = match id {
        "full" => MCP_SCOPES.iter().map(|scope| scope.id).collect(),
        "read-all" => MCP_SCOPES
            .iter()
            .filter(|scope| !scope.write)
            .map(|scope| scope.id)
            .collect(),
        "operator" => MCP_SCOPES
            .iter()
            .filter(|scope| {
                (OPERATOR_WRITE.contains(&scope.group))
                    || (!scope.write && OPERATOR_READ_ONLY.contains(&scope.group))
            })
            .map(|scope| scope.id)
            .collect(),
        "observer" => MCP_SCOPES
            .iter()
            .filter(|scope| !scope.write && OBSERVER.contains(&scope.group))
            .map(|scope| scope.id)
            .collect(),
        _ => return None,
    };
    Some(scopes)
}

/// Resolve a template id, or `custom` plus an explicit list, into the scopes
/// to store. A write scope always brings the matching read scope so the agent
/// can see what it is allowed to change.
pub fn validate_agent_scopes(
    template: &str,
    custom: &[String],
) -> Result<Vec<String>, &'static str> {
    if template == "custom" {
        return normalize_custom(custom);
    }
    scopes_for_template(template)
        .map(|scopes| scopes.into_iter().map(str::to_owned).collect())
        .ok_or("unknown permission template")
}

fn normalize_custom(custom: &[String]) -> Result<Vec<String>, &'static str> {
    if custom.is_empty() {
        return Err("choose at least one permission");
    }
    if custom.len() > MCP_SCOPES.len() {
        return Err("too many permissions");
    }
    let mut out = Vec::new();
    for scope in custom {
        if !MCP_SCOPES.iter().any(|known| known.id == scope) {
            return Err("unknown permission");
        }
        if !out.iter().any(|existing: &String| existing == scope) {
            out.push(scope.clone());
        }
        if let Some(read) = scope.strip_suffix(":write") {
            let read = format!("{read}:read");
            if MCP_SCOPES.iter().any(|known| known.id == read)
                && !out.iter().any(|existing| existing == &read)
            {
                out.push(read);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `true` when the scope id is one this panel knows.
pub fn scope_known(id: &str) -> bool {
    MCP_SCOPES.iter().any(|scope| scope.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ids_are_unique_and_well_formed() {
        assert_eq!(MCP_SCOPES.len(), GROUPS.len() * 2);
        let mut seen = std::collections::BTreeSet::new();
        for scope in MCP_SCOPES {
            assert!(seen.insert(scope.id), "duplicate {}", scope.id);
            assert!(!scope.id.is_empty(), "missing id for {}", scope.group);
            assert!(scope.id.starts_with(scope.group));
            assert!(scope
                .id
                .ends_with(if scope.write { ":write" } else { ":read" }));
        }
    }

    #[test]
    fn full_control_is_every_scope_and_includes_writes() {
        let full = scopes_for_template("full").unwrap();
        assert_eq!(full.len(), MCP_SCOPES.len());
        assert!(full.contains(&"auth:write"));
        assert!(full.contains(&"system:write"));
        assert!(full.iter().all(|id| scope_known(id)));
        let read_all = scopes_for_template("read-all").unwrap();
        assert!(read_all.iter().all(|id| id.ends_with(":read")));
        assert_eq!(read_all.len(), GROUPS.len());
    }

    #[test]
    fn operate_cannot_change_users_or_updates() {
        let scopes = scopes_for_template("operator").unwrap();
        assert!(scopes.contains(&"catalog:write"));
        assert!(scopes.contains(&"cache:write"));
        assert!(scopes.contains(&"backups:read"));
        assert!(!scopes.contains(&"backups:write"));
        assert!(!scopes.contains(&"auth:write"));
        assert!(!scopes.contains(&"auth:read"));
        assert!(!scopes.contains(&"system:write"));
        assert!(scopes.contains(&"system:read"));
        assert!(scopes.contains(&"access:read"));
        assert!(scopes.contains(&"access:write"));
        assert!(scopes.contains(&"security:write"));
        assert!(scopes.contains(&"cloudflare:read"));
        assert!(!scopes.contains(&"cloudflare:write"));
        let watch = scopes_for_template("observer").unwrap();
        assert!(watch.iter().all(|id| id.ends_with(":read")));
        assert!(watch.contains(&"logs:read"));
        assert!(watch.contains(&"security:read"));
        assert!(!watch.contains(&"access:read"));
        assert!(!watch.contains(&"auth:read"));
    }

    #[test]
    fn custom_scopes_reject_unknown_ids_and_add_the_matching_read() {
        let scopes = validate_agent_scopes("custom", &["cache:write".into()]).unwrap();
        assert_eq!(
            scopes,
            vec!["cache:read".to_owned(), "cache:write".to_owned()]
        );
        assert!(validate_agent_scopes("custom", &[]).is_err());
        assert!(validate_agent_scopes("custom", &["nope".into()]).is_err());
        assert!(validate_agent_scopes("nope", &[]).is_err());
        assert_eq!(
            validate_agent_scopes("full", &[]).unwrap().len(),
            MCP_SCOPES.len()
        );
    }
}
