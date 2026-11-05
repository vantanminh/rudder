//! Rudder control plane library.
//!
//! The binary in `main.rs` loads configuration, initializes tracing, and calls
//! [`run`]. Tests construct an [`AppState`] and the [`router`] directly.

mod agent;
mod audit;
mod auth;
mod backup;
mod build_queue;
mod cache;
mod cloudflare;
mod config;
mod db;
mod deploy;
mod driver;
mod embed;
mod error;
mod events;
mod gate;
mod gitkey;
mod guard;
mod logs;
mod mailer;
mod mcp;
mod monitor;
mod nodes;
mod openapi;
mod panel;
mod previews;
mod publish;
mod routes;
mod s3;
mod security;
mod shed;
mod stacks;
mod state;
pub mod telemetry;
mod templates;
mod update;

use anyhow::Context;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;

pub use agent::serve as serve_agents;
pub use build_queue::build_once;
pub use config::{Config, ConfigError, LogFormat};
pub use db::ensure_partitions;
pub use error::ApiError;
pub use logs::retention::run_once as apply_log_retention;
pub use monitor::notify::{deliver_once as deliver_notifications, sign as sign_webhook};
pub use monitor::rollup::{
    retention_once as apply_metrics_retention, rollup_once as rollup_metrics,
};
pub use monitor::{evaluate_alerts, insert_samples, Sample};
pub use nodes::driver_for as node_driver;
pub use panel::{Manifest as PanelManifest, RestoreOptions, RestoreReport};
pub use state::AppState;
pub use update::{
    apply_panel, check as check_for_update, run_updater, update_components, UpdaterArgs,
};

/// Bind the HTTP server, run migrations, and serve until shutdown.
pub async fn run(config: Config) -> anyhow::Result<()> {
    agent::install_crypto_provider();
    let pool = db::connect(&config).await?;
    db::MIGRATOR
        .run(&pool)
        .await
        .context("apply database migrations")?;
    db::ensure_partitions(&pool)
        .await
        .context("ensure daily partitions")?;
    db::ensure_local_server(&pool)
        .await
        .context("ensure local server")?;
    db::recover_interrupted(&pool)
        .await
        .context("recover interrupted work")?;
    let settings = db::load_panel_settings(&pool).await?;
    tracing::info!(
        github_repo = %settings.github_repo,
        update_channel = %settings.update_channel,
        auto_update = settings.auto_update,
        "panel settings loaded"
    );

    let addr = config.bind;
    let state = AppState::new(pool, config);
    cloudflare::refresh_mode(&state).await;
    logs::layer::install(state.logs.sender());
    start_log_pipeline(&state);
    let retention_pool = state.pool.clone();
    tokio::spawn(async move {
        loop {
            match logs::retention::run_once(&retention_pool).await {
                Ok(report) if report.deleted_rows > 0 || !report.dropped_partitions.is_empty() => {
                    tracing::info!(
                        deleted = report.deleted_rows,
                        dropped = ?report.dropped_partitions,
                        "log retention applied"
                    )
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "log retention failed"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    start_monitoring(&state);
    tokio::spawn(guard::run(state.clone()));
    let cache_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = routes::cache::sample_cache(&cache_state).await {
                tracing::warn!(message = %error.public_message(), "cache sample failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
    let edge_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = edge_state.edge.flush(&edge_state).await {
                tracing::warn!(message = %error.public_message(), "edge counter flush failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
    let maintain_state = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = mcp::maintain(&maintain_state).await {
                tracing::warn!(message = %error.public_message(), "agent login cleanup failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    let publish_state = state.clone();
    tokio::spawn(async move {
        let local = sqlx::query_scalar!(r#"SELECT id FROM servers WHERE kind = 'local' LIMIT 1"#)
            .fetch_optional(&publish_state.pool)
            .await;
        if let Ok(Some(id)) = local {
            if let Err(error) = routes::catalog::probe(&publish_state, id).await {
                tracing::warn!(message = %error.public_message(), "local Docker probe failed");
            }
            if let Err(error) = publish::publish_server(&publish_state, id).await {
                tracing::warn!(message = %error.public_message(), "startup publish failed");
            }
        }
    });
    let watch_state = state.clone();
    let backup_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            match backup::schedule_once(&backup_state).await {
                Ok(0) => {}
                Ok(started) => tracing::info!(started, "scheduled backups started"),
                Err(error) => tracing::warn!(%error, "backup scheduler query failed"),
            }
        }
    });
    let build_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            match build_queue::build_once(&build_state).await {
                Ok(true) => tracing::debug!("build queue claimed a job"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(message = %error.public_message(), "build queue failed")
                }
            }
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            if let Err(error) = deploy::watch_once(&watch_state).await {
                tracing::warn!(%error, "image watch query failed");
            }
        }
    });
    if let Some(agent_addr) = state.config.agent_bind {
        let agent_listener = TcpListener::bind(agent_addr)
            .await
            .with_context(|| format!("bind agent listener {agent_addr}"))?;
        let agent_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = agent::serve(agent_state, agent_listener).await {
                tracing::error!(error = %format!("{error:#}"), "agent listener stopped");
            }
        });
    }
    update::spawn_background(state.clone());
    let app = router(state);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let bound = listener.local_addr().context("local address")?;
    tracing::info!(%bound, "rudder control plane listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("http server")?;
    tracing::info!("rudder control plane stopped");
    Ok(())
}

/// Start the log writer and the local container log collector.
pub fn start_log_pipeline(state: &AppState) {
    tokio::spawn(state.logs.clone().run_writer(state.pool.clone()));
    tokio::spawn(logs::collector::run(state.clone()));
}

/// Start metrics sampling, alert evaluation, rollups, and notification delivery.
pub fn start_monitoring(state: &AppState) {
    tokio::spawn(monitor::run(state.clone()));
    tokio::spawn(monitor::notify::run(
        state.pool.clone(),
        state.cipher.clone(),
    ));
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let mut last_retention: Option<std::time::Instant> = None;
        loop {
            if let Err(error) = monitor::rollup::rollup_once(&pool, chrono::Utc::now()).await {
                tracing::warn!(%error, "metrics rollup failed");
            }
            if last_retention.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(3600))
            {
                last_retention = Some(std::time::Instant::now());
                if let Err(error) = monitor::rollup::retention_once(&pool).await {
                    tracing::warn!(%error, "metrics retention failed");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

/// Highest schema migration this binary applies.
pub fn known_migration() -> i64 {
    db::known_migration()
}

/// Restore a panel backup into an empty database before the panel starts.
pub async fn restore_panel(options: RestoreOptions) -> anyhow::Result<RestoreReport> {
    panel::restore(options).await
}

/// Write an encrypted panel backup to `out`, or to stdout when `out` is `-`.
/// Returns the number of bytes written.
pub async fn backup_panel(
    config: &Config,
    passphrase: &str,
    include_history: bool,
    out: &std::path::Path,
) -> anyhow::Result<u64> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let pool = db::connect(config).await?;
    let archive = panel::archive(&pool, config, include_history)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut stream =
        panel::seal(archive, passphrase).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let mut writer: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>> =
        if out == std::path::Path::new("-") {
            Box::pin(tokio::io::stdout())
        } else {
            Box::pin(
                {
                    let mut o = tokio::fs::OpenOptions::new();
                    #[cfg(unix)]
                    o.mode(0o600);
                    o
                }
                .write(true)
                .create(true)
                .truncate(true)
                .open(out)
                .await
                .with_context(|| format!("create {}", out.display()))?,
            )
        };
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| anyhow::anyhow!(error.to_string()))?;
        writer.write_all(&chunk).await?;
        written += chunk.len() as u64;
    }
    writer.flush().await?;
    writer.shutdown().await?;
    pool.close().await;
    Ok(written)
}

/// The OpenAPI document as pretty JSON, used to generate frontend types.
pub fn openapi_json() -> String {
    openapi::document()
        .to_pretty_json()
        .expect("OpenAPI document serializes")
}

/// HTTP router. Additional route groups are nested here as features land.
pub fn router(state: AppState) -> axum::Router {
    axum::Router::new()
        .merge(routes::health::router())
        .merge(routes::account::router())
        .merge(routes::access::router())
        .merge(routes::security::router())
        .merge(routes::cloudflare::router())
        .merge(routes::catalog::router())
        .merge(routes::edge::router())
        .merge(routes::panel_ssl::router())
        .merge(routes::git::router())
        .merge(routes::github_app::router())
        .merge(routes::github_repos::router())
        .merge(routes::databases::router())
        .merge(routes::backups::router())
        .merge(routes::images::router())
        .merge(routes::stacks::router())
        .merge(routes::cache::router())
        .merge(routes::logs::router())
        .merge(routes::monitoring::router())
        .merge(routes::agents::router())
        .merge(routes::system::router())
        .merge(routes::load::router())
        .merge(routes::updates::router())
        .merge(routes::mcp::router())
        .merge(mcp::router())
        .merge(
            utoipa_swagger_ui::SwaggerUi::new("/api/docs")
                .url("/api/openapi.json", openapi::document()),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            mcp::gate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            shed::slots(),
            shed::middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            security::middleware,
        ))
        .with_state(state.clone())
        .fallback(embed::spa)
        .layer(axum::middleware::from_fn_with_state(
            state,
            cloudflare::enforce_access,
        ))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to listen for ctrl-c");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "failed to listen for sigterm"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
