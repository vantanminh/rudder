use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use sqlx::PgPool;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use crate::agent::AgentRegistry;
use crate::driver::LocalDriver;
use crate::logs::LogHub;
use crate::Config;
use rudder_core::Cipher;

/// Per-server backup concurrency limit and its semaphore.
pub type BackupSlots = Mutex<HashMap<Uuid, (usize, Arc<Semaphore>)>>;

pub type LoginLimiter =
    RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock, NoOpMiddleware>;

/// Shared handle for HTTP handlers and background jobs.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub cipher: Arc<Cipher>,
    pub login_limiter: Arc<LoginLimiter>,
    /// Registration and token-endpoint requests, per client IP.
    pub oauth_limiter: Arc<LoginLimiter>,
    /// MCP JSON-RPC requests, per client IP.
    pub mcp_limiter: Arc<LoginLimiter>,
    pub local_driver: Arc<LocalDriver>,
    pub agents: Arc<AgentRegistry>,
    pub backup_slots: Arc<BackupSlots>,
    pub logs: Arc<LogHub>,
    pub updates: Arc<crate::update::Updates>,
    /// Running compose stack actions, so one can be cancelled.
    pub stack_runs: Arc<crate::stacks::StackRuns>,
    /// Host load, and whether new builds may start.
    pub guard: Arc<crate::guard::LoadGuard>,
    /// Rate-limit buckets and counters for the edge gate.
    pub edge: Arc<crate::gate::EdgeMemory>,
    /// Cached Cloudflare Access settings and signing keys.
    pub cloudflare: Arc<crate::cloudflare::Runtime>,
}

impl AppState {
    pub fn new(pool: PgPool, config: Config) -> Self {
        let cipher = Arc::new(Cipher::new(*config.master_key()));
        let per_minute = NonZeroU32::new(10).expect("login quota");
        let login_limiter = Arc::new(RateLimiter::keyed(Quota::per_minute(per_minute)));
        let oauth_per_minute = NonZeroU32::new(30).expect("oauth quota");
        let oauth_limiter = Arc::new(RateLimiter::keyed(Quota::per_minute(oauth_per_minute)));
        let mcp_per_minute = NonZeroU32::new(300).expect("mcp quota");
        let mcp_limiter = Arc::new(RateLimiter::keyed(Quota::per_minute(mcp_per_minute)));
        let local_driver = Arc::new(
            LocalDriver::connect(&config.data_dir, config.docker_socket.as_deref())
                .with_cache_admin(&config.cache_admin_url)
                .with_host_proc(&config.host_proc),
        );
        Self {
            pool,
            config: Arc::new(config),
            cipher,
            login_limiter,
            oauth_limiter,
            mcp_limiter,
            local_driver,
            agents: Arc::new(AgentRegistry::default()),
            backup_slots: Arc::new(Mutex::new(HashMap::new())),
            logs: Arc::new(LogHub::default()),
            updates: Arc::default(),
            stack_runs: Arc::default(),
            guard: Arc::default(),
            edge: crate::gate::memory(),
            cloudflare: Arc::default(),
        }
    }
}
