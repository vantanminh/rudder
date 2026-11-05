//! Host and container metrics, rollup resolution, and alert evaluation.
//!
//! Shared by the panel (local server) and the agent (remote servers).

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Seconds between samples.
pub const SAMPLE_INTERVAL_SECS: u64 = 10;

/// Server-wide resource usage at one instant. Network counters are cumulative.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HostStats {
    pub cpu_percent: f64,
    pub cpu_count: u32,
    pub memory_total_bytes: u64,
    pub memory_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub load1: f64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

impl HostStats {
    pub fn memory_percent(&self) -> f64 {
        percent(self.memory_used_bytes, self.memory_total_bytes)
    }

    pub fn disk_percent(&self) -> f64 {
        percent(self.disk_used_bytes, self.disk_total_bytes)
    }
}

pub fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub total: u64,
}

/// First `cpu` line of `/proc/stat`.
pub fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal; guest time is already in user.
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    let total = values.iter().take(8).sum();
    Some(CpuTimes { idle, total })
}

pub fn cpu_percent(previous: CpuTimes, current: CpuTimes) -> f64 {
    let total = current.total.saturating_sub(previous.total);
    let idle = current.idle.saturating_sub(previous.idle);
    if total == 0 {
        return 0.0;
    }
    ((total - idle.min(total)) as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
}

/// `(total, available)` bytes from `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kib| kib * 1024)
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:").or_else(|| {
        Some(field("MemFree:")? + field("Buffers:").unwrap_or(0) + field("Cached:").unwrap_or(0))
    })?;
    Some((total, available.min(total)))
}

/// Interfaces that carry container-internal traffic and would double count.
fn virtual_interface(name: &str) -> bool {
    name == "lo"
        || name.starts_with("veth")
        || name.starts_with("docker")
        || name.starts_with("br-")
        || name.starts_with("cni")
        || name.starts_with("flannel")
}

/// Total `(rx, tx)` bytes of physical-looking interfaces from `/proc/net/dev`.
pub fn parse_net_dev(text: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if virtual_interface(name.trim()) {
            continue;
        }
        let values: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        if values.len() >= 9 {
            rx = rx.saturating_add(values[0]);
            tx = tx.saturating_add(values[8]);
        }
    }
    (rx, tx)
}

pub fn parse_loadavg(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Read host stats from a procfs root (normally `/proc`, or a host mount such as
/// `/host/proc`) and the filesystem holding `disk_path`. `previous` carries CPU
/// counters between calls; the first call reports 0% CPU.
pub fn read_host_stats(
    proc_root: &Path,
    disk_path: &Path,
    previous: &mut Option<CpuTimes>,
) -> Result<HostStats, Error> {
    let read = |name: &str| {
        std::fs::read_to_string(proc_root.join(name))
            .map_err(|error| Error::Node(format!("read {name}: {error}")))
    };
    let stat = read("stat")?;
    let current = parse_proc_stat(&stat).ok_or_else(|| Error::Node("parse /proc/stat".into()))?;
    let cpu = previous.map_or(0.0, |prev| cpu_percent(prev, current));
    *previous = Some(current);
    let cpu_count = stat
        .lines()
        .filter(|line| {
            line.starts_with("cpu") && line.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
        })
        .count() as u32;
    let (memory_total, available) =
        parse_meminfo(&read("meminfo")?).ok_or_else(|| Error::Node("parse meminfo".into()))?;
    let (net_rx, net_tx) = read("net/dev").map(|t| parse_net_dev(&t)).unwrap_or((0, 0));
    let load1 = read("loadavg")
        .ok()
        .and_then(|t| parse_loadavg(&t))
        .unwrap_or(0.0);
    #[cfg(unix)]
    let (disk_total, disk_used) = match rustix::fs::statvfs(disk_path) {
        Ok(vfs) => {
            let total = vfs.f_blocks.saturating_mul(vfs.f_frsize);
            let free = vfs.f_bfree.saturating_mul(vfs.f_frsize);
            (total, total.saturating_sub(free))
        }
        Err(_) => (0, 0),
    };
    #[cfg(not(unix))]
    let (disk_total, disk_used) = {
        let _ = disk_path;
        (0, 0)
    };
    Ok(HostStats {
        cpu_percent: cpu,
        cpu_count: cpu_count.max(1),
        memory_total_bytes: memory_total,
        memory_used_bytes: memory_total - available,
        disk_total_bytes: disk_total,
        disk_used_bytes: disk_used,
        load1,
        net_rx_bytes: net_rx,
        net_tx_bytes: net_tx,
    })
}

/// Per-second rate between two cumulative counter readings. Counter resets read as 0.
pub fn rate(previous: u64, current: u64, seconds: f64) -> f64 {
    if seconds <= 0.0 || current < previous {
        0.0
    } else {
        (current - previous) as f64 / seconds
    }
}

/// Docker's CPU percentage formula, scaled to the whole machine (0–100).
pub fn container_cpu_percent(
    cpu_total: u64,
    precpu_total: u64,
    system: u64,
    presystem: u64,
) -> f64 {
    let cpu_delta = cpu_total.saturating_sub(precpu_total) as f64;
    let system_delta = system.saturating_sub(presystem) as f64;
    if system_delta <= 0.0 {
        0.0
    } else {
        (cpu_delta / system_delta * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolution {
    Raw,
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "1h")]
    H1,
}

impl Resolution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::H1 => "1h",
        }
    }

    pub fn seconds(self) -> i64 {
        match self {
            Self::Raw => SAMPLE_INTERVAL_SECS as i64,
            Self::M1 => 60,
            Self::M5 => 300,
            Self::H1 => 3600,
        }
    }
}

/// Coarsest data that still gives a few hundred points for a chart over `range_secs`.
pub fn resolution_for(range_secs: i64) -> Resolution {
    match range_secs {
        ..=3600 => Resolution::Raw,
        3601..=21_600 => Resolution::M1,
        21_601..=172_800 => Resolution::M5,
        _ => Resolution::H1,
    }
}

/// Supported alert rule kinds.
pub const ALERT_KINDS: &[(&str, &str)] = &[
    ("server_cpu", "Server CPU above threshold %"),
    ("server_memory", "Server memory above threshold %"),
    ("server_disk", "Server disk above threshold %"),
    ("server_offline", "Remote server not seen for the duration"),
    ("service_cpu", "Service CPU above threshold %"),
    ("service_memory", "Service memory above threshold MiB"),
    (
        "service_down",
        "Service expected running but no container is up",
    ),
    (
        "http_errors",
        "More than threshold HTTP 5xx responses within the duration",
    ),
    ("event", "Forward panel events (deploys, backups, updates)"),
];

pub fn validate_alert_kind(kind: &str) -> Result<(), Error> {
    if ALERT_KINDS.iter().any(|(k, _)| *k == kind) {
        Ok(())
    } else {
        Err(Error::Validation(format!(
            "alert kind must be one of {}",
            ALERT_KINDS
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

pub fn alert_needs_threshold(kind: &str) -> bool {
    !matches!(kind, "service_down" | "server_offline" | "event")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertState {
    Ok,
    Pending,
    Firing,
}

impl AlertState {
    pub fn parse(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "firing" => Self::Firing,
            _ => Self::Ok,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Pending => "pending",
            Self::Firing => "firing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertTransition {
    None,
    Fired,
    Resolved,
}

/// Advance an alert. A breach must hold for `duration_secs` (measured from
/// `since`, when the current state began) before the alert fires.
pub fn step_alert(
    state: AlertState,
    since: DateTime<Utc>,
    breached: bool,
    duration_secs: i64,
    now: DateTime<Utc>,
) -> (AlertState, AlertTransition) {
    match (state, breached) {
        (AlertState::Ok, false) => (AlertState::Ok, AlertTransition::None),
        (AlertState::Ok, true) if duration_secs <= 0 => {
            (AlertState::Firing, AlertTransition::Fired)
        }
        (AlertState::Ok, true) => (AlertState::Pending, AlertTransition::None),
        (AlertState::Pending, false) => (AlertState::Ok, AlertTransition::None),
        (AlertState::Pending, true) => {
            if (now - since).num_seconds() >= duration_secs {
                (AlertState::Firing, AlertTransition::Fired)
            } else {
                (AlertState::Pending, AlertTransition::None)
            }
        }
        (AlertState::Firing, true) => (AlertState::Firing, AlertTransition::None),
        (AlertState::Firing, false) => (AlertState::Ok, AlertTransition::Resolved),
    }
}

/// Whether an event kind matches a rule's patterns (`*`, exact, or `prefix.*`).
pub fn event_matches(patterns: &[String], kind: &str) -> bool {
    patterns.iter().any(|pattern| {
        pattern == "*"
            || pattern == kind
            || pattern.strip_suffix(".*").is_some_and(|prefix| {
                kind.strip_prefix(prefix)
                    .is_some_and(|r| r.starts_with('.'))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn proc_parsers_read_linux_formats() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 100 0 50 800 50 0 0 0 0 0\n";
        let first = parse_proc_stat(stat).unwrap();
        assert_eq!(
            first,
            CpuTimes {
                idle: 850,
                total: 1000
            }
        );
        let second = parse_proc_stat("cpu  200 0 100 850 50 0 0 0 0 0\n").unwrap();
        assert!((cpu_percent(first, second) - 75.0).abs() < 1e-9);

        let mem = "MemTotal:       2048 kB\nMemFree:         100 kB\nMemAvailable:    512 kB\n";
        assert_eq!(parse_meminfo(mem), Some((2048 * 1024, 512 * 1024)));
        let old_kernel = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 150 kB\n";
        assert_eq!(parse_meminfo(old_kernel), Some((1000 * 1024, 300 * 1024)));

        let net = "Inter-|   Receive\n face |bytes packets\n    lo: 999 1 0 0 0 0 0 0 999 1 0 0 0 0 0 0\n  eth0: 1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\nveth12: 5 1 0 0 0 0 0 0 5 1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(net), (1000, 2000));
        assert_eq!(parse_loadavg("0.52 0.40 0.30 1/200 999"), Some(0.52));
    }

    #[test]
    fn host_stats_read_from_this_machine() {
        if !Path::new("/proc/stat").exists() {
            return;
        }
        let mut previous = None;
        let first = read_host_stats(Path::new("/proc"), Path::new("/"), &mut previous).unwrap();
        assert!(first.memory_total_bytes > 0);
        assert!(first.disk_total_bytes > 0);
        assert!(previous.is_some());
    }

    #[test]
    fn rates_and_container_cpu() {
        assert_eq!(rate(100, 300, 10.0), 20.0);
        assert_eq!(rate(300, 100, 10.0), 0.0);
        assert_eq!(container_cpu_percent(200, 100, 2000, 1000), 10.0);
        assert_eq!(container_cpu_percent(200, 100, 1000, 1000), 0.0);
    }

    #[test]
    fn resolution_scales_with_range() {
        assert_eq!(resolution_for(3600), Resolution::Raw);
        assert_eq!(resolution_for(6 * 3600), Resolution::M1);
        assert_eq!(resolution_for(24 * 3600), Resolution::M5);
        assert_eq!(resolution_for(30 * 86_400), Resolution::H1);
    }

    #[test]
    fn alerts_fire_after_duration_and_resolve() {
        let t0 = Utc::now();
        let (state, transition) = step_alert(AlertState::Ok, t0, true, 60, t0);
        assert_eq!(
            (state, transition),
            (AlertState::Pending, AlertTransition::None)
        );
        let (state, transition) = step_alert(state, t0, true, 60, t0 + Duration::seconds(30));
        assert_eq!(
            (state, transition),
            (AlertState::Pending, AlertTransition::None)
        );
        let (state, transition) = step_alert(state, t0, true, 60, t0 + Duration::seconds(61));
        assert_eq!(
            (state, transition),
            (AlertState::Firing, AlertTransition::Fired)
        );
        let (state, transition) = step_alert(state, t0, true, 60, t0 + Duration::seconds(90));
        assert_eq!(
            (state, transition),
            (AlertState::Firing, AlertTransition::None)
        );
        let (state, transition) = step_alert(state, t0, false, 60, t0 + Duration::seconds(95));
        assert_eq!(
            (state, transition),
            (AlertState::Ok, AlertTransition::Resolved)
        );
        assert_eq!(
            step_alert(AlertState::Pending, t0, false, 60, t0),
            (AlertState::Ok, AlertTransition::None)
        );
        assert_eq!(
            step_alert(AlertState::Ok, t0, true, 0, t0),
            (AlertState::Firing, AlertTransition::Fired)
        );
    }

    #[test]
    fn event_patterns() {
        let patterns = vec!["backup.*".to_string(), "deploy.failed".to_string()];
        assert!(event_matches(&patterns, "backup.failed"));
        assert!(event_matches(&patterns, "deploy.failed"));
        assert!(!event_matches(&patterns, "deploy.succeeded"));
        assert!(!event_matches(&patterns, "backupx.failed"));
        assert!(event_matches(&["*".to_string()], "anything"));
        assert!(validate_alert_kind("server_cpu").is_ok());
        assert!(validate_alert_kind("nope").is_err());
    }
}
