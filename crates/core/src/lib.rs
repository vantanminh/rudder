//! Shared domain primitives for the Rudder control plane, agent, and CLI.
//!
//! Infrastructure crates depend on this crate. It does not talk to Docker,
//! PostgreSQL, or the network.

mod access;
mod backup;
mod buildspec;
mod cache;
mod certs;
mod cloudflare;
mod compose;
mod crypto;
mod database;
mod driver;
mod envelope;
mod envfile;
mod error;
mod exposure;
mod github_app;
mod guard;
mod image_ref;
mod isolate;
mod logs;
mod mcp;
mod metrics;
mod ratelimit;
mod release;
mod retention;
mod rollout;
mod rudder_image;
mod schedule;
mod stack;
mod template;
mod traefik;
mod validate;
mod webhook;

pub use access::{
    accept_turnstile, access_allowed, department_closure, parse_turnstile, safe_return_path,
    AccessRule, Membership, TurnstileResult, ACCESS_LOGIN_ACTION,
};
pub use backup::{
    dump_command, dump_extension, filter_databases, mysql_list_databases_command, object_key,
    redis_bgsave_command, redis_persistence_command, restore_command, retry_backoff_secs,
    validate_extra_args, validate_prefix, validate_tool_options, Compression, Consistency,
    DumpCredentials, KeyVars, RedisPersistence, RetentionSpec, ToolCommand, ToolOptions,
};
pub use buildspec::{
    can_start, docker_build_argv, git_clone_argv, nixpacks_argv, redact, static_dockerfile,
    validate_branch, validate_git_url, validate_rel_path, BuildPlan, BuildStrategy,
};
pub use cache::{
    cache_path, cache_preset, effective_cache_budget, encoding_class, validate_upstream,
    CacheControl, CacheDocument, CacheHostStats, CachePolicy, CachePreset, CachePurge,
    CachePurgeResult, CacheRequest, CacheRule, CacheSite, CacheStats, Freshness, RequestDecision,
    RuleAction, RuleMatch, RuleMode, CACHE_CONTAINER, CACHE_PORT, CACHE_PRESETS, CACHE_RULES_FILE,
    CACHE_TOKEN_FILE, DEFAULT_CACHE_CAP_BYTES, DEFAULT_MAX_OBJECT_BYTES, DEFAULT_MEMORY_BYTES,
};
pub use certs::{
    certificate_matches_host, describe as describe_certificate, matching_certificate,
    validate_key_and_chain, CertificateDetails, HostMatch,
};
pub use cloudflare::{
    access_certs_url, access_issuer, can_close_ports, cloudflared_spec, host_in_zone,
    normalize_team_domain, plan_dns, preflight, replica_container_name, sshd_trust_script,
    tunnel_cname, tunnel_config, tunnel_origin, validate_access_email, validate_cloudflare_id,
    validate_replicas, validate_ssh_ca_public_key, validate_unix_user, verify_access_jwt,
    AccessClaims, DnsAction, DnsRecord, PreflightCheck, PreflightInput,
    CLOUDFLARED_CONTAINER_PREFIX, CLOUDFLARED_LABEL, CLOUDFLARED_METRICS_PORT,
    DEFAULT_CLOUDFLARED_IMAGE, MAX_TUNNEL_REPLICAS,
};
pub use compose::{
    compose_args, docker_config_with_logins, ComposeAction, ComposeOptions, LogSink,
};
pub use crypto::Cipher;
pub use database::{connection_string, database_runtime, DatabaseRequest, DatabaseRuntime, Engine};
pub use driver::{
    validate_host_path, ArchiveSource, ByteStream, ContainerInfo, ContainerSpec, ContainerStats,
    DockerInfo, ExecOutput, ExecSpec, HealthState, LogLine, LogLineStream, LogStreamKind,
    NodeDriver, PortMapping, ReadyProbe, RegistryAuth, ReplaceContainer, ReplaceReport,
    TerminalInput, TerminalInputStream, VolumeMount,
};
pub use envelope::{Decryptor, Encryptor, MAGIC as ENVELOPE_MAGIC};
pub use envfile::{
    parse_env_text, EnvEntry, EnvParse, EnvParseError, MAX_ENV_VALUE_BYTES, MAX_IMPORT_ENTRIES,
};
pub use error::Error;
pub use exposure::{classify_probe, host_is_ip, Exposure, Probe, PROBES};
pub use github_app::{
    github_app_jwt, github_app_manifest, github_repo_full_name, is_private_host, validate_app_name,
    validate_github_login, AppManifest,
};
pub use guard::{
    parse_load_averages, parse_pressure, parse_swap, read_host_pressure, Assessment, GuardLevel,
    GuardLimits, GuardReason, GuardTracker, HostPressure, CONTROL_PLANE_CPU_SHARES,
    CONTROL_PLANE_OOM_SCORE_ADJ, DEFAULT_GUARD_CPU_PERCENT, DEFAULT_MEMORY_RESERVE_PERCENT,
    GUARD_INTERVAL_SECS, PROXY_CPU_SHARES, WORKLOAD_OOM_SCORE_ADJ,
};
pub use image_ref::{
    container_name, image_registry, normalize_registry, select_registry, validate_registry_url,
    RegistryRecord,
};
pub use isolate::{isolated_ports, network_isolation_enabled};
pub use logs::{
    escape_like, normalize_level, parse_docker_timestamp, parse_line, partition_day,
    partitions_to_drop, validate_source as validate_log_source, ParsedLog, LOG_LEVELS,
    LOG_PARTITION_PREFIX, LOG_SOURCES, MAX_LOG_MESSAGE,
};
pub use mcp::{
    scope_known, scopes_for_template, validate_agent_scopes, McpScope, McpTemplate, MCP_SCOPES,
    MCP_TEMPLATES,
};
pub use metrics::{
    alert_needs_threshold, container_cpu_percent, cpu_percent, event_matches, parse_loadavg,
    parse_meminfo, parse_net_dev, parse_proc_stat, percent, rate, read_host_stats, resolution_for,
    step_alert, validate_alert_kind, AlertState, AlertTransition, CpuTimes, HostStats, Resolution,
    ALERT_KINDS, SAMPLE_INTERVAL_SECS,
};
pub use ratelimit::{
    decide as decide_rate, validate_rate_rule, Bucket, RateDecision, RateKey, RateRequest, RateRule,
};
pub use release::{
    in_maintenance_window, newer_releases, parse_maintenance_window, parse_sha256sums,
    parse_version, pick_release, sha256_hex, validate_pinned_image, verify_minisign,
    verify_release, GithubAsset, GithubRelease, ReleaseImages, ReleaseManifest, UpdateChannel,
    AGENT_TARGETS, RELEASE_FORMAT, RELEASE_MANIFEST, RELEASE_SIGNATURE, RELEASE_SUMS,
};
pub use retention::{expired_ids, BackupPoint, RetentionPolicy};
pub use rollout::{
    retire_container, rollout, HealthKind, HealthSpec, RolloutOutput, RolloutPlan, StartedContainer,
};
pub use rudder_image::{
    generate_device_code, image_object_key, images_to_prune_from_r2, normalize_device_code,
    validate_cli_auth_method, validate_cli_callback_url, validate_compression, validate_image_tag,
    ImagePoint, ImageRetentionPolicy, CLI_AUTH_METHODS, CLI_AUTH_TTL_SECS, DEVICE_CODE_LENGTH,
};
pub use schedule::{is_due, next_after, parse_timezone, validate_schedule, PRESETS};
pub use stack::{
    compose_variable_refs, compose_variables, render_env_file, transform_compose,
    validate_compose_service, StackPlan, StackService, VariableRef, MAX_COMPOSE_BYTES,
};
pub use template::{ResolvedTemplate, Template, TemplateVariable, VariableKind};
pub use traefik::{
    preview_hostname, render_dynamic_config, render_dynamic_config_for, validate_host,
    validate_path_prefix, EdgeGate, EdgeMode, ProxyRoute, TlsMaterial, TlsMode, PROXY_NETWORK,
    TRAEFIK_CERT_DIR, TRAEFIK_CONTAINER,
};
pub use validate::{validate_env_key, validate_image_ref, validate_name};
pub use webhook::{evaluate_hook, verify_github_signature, HookInput, HookKind, HookOutcome};

/// Fill `N` bytes from the process CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    bytes
}

/// Version shared by every Rudder binary. Release builds set
/// `RUDDER_BUILD_VERSION` from the tag; other builds use the crate version.
pub const VERSION: &str = match option_env!("RUDDER_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// Product name used in logs, installers, and the panel.
pub const PRODUCT: &str = "rudder";
