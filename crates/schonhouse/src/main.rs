//! Schonhouse: a lightweight home assistant service.
//!
//! Loads `schonhouse.toml`, opens the SQLite store, starts one actor per stored
//! device, and serves a REST API, a Server-Sent Events stream and an embedded web UI
//! on the local network.

mod api;
mod config;
mod devices;
mod discovery;
mod store;
mod usage;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::devices::{LogLevel, Registry};
use crate::store::Store;

#[derive(Parser)]
#[command(version, about = "Lightweight home assistant service")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, env = "SCHONHOUSE_CONFIG", default_value = "schonhouse.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    config::load_dotenv(std::path::Path::new(".env"));
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let credentials = config::credentials_from_env()?;

    let store = Store::open(&config.storage.path)?;
    info!("database {}", config.storage.path.display());

    // Seed: import config devices that are not in the database yet.
    let stored = store.list_devices().await?;
    for device in &config.devices {
        if stored.iter().any(|d| d.id == device.id) {
            continue;
        }
        match store.insert_device(device.clone()).await {
            Ok(()) => info!(device = %device.id, "imported from {}", cli.config.display()),
            Err(err) => warn!(device = %device.id, "not imported: {err}"),
        }
    }

    let poll = Duration::from_secs(config.server.poll_interval_secs);
    let registry = Registry::new(&credentials, poll);
    let devices = store.list_devices().await?;
    for device in devices {
        if let Err(err) = registry.add(device.clone()).await {
            warn!(device = %device.id, "not started: {}", err.message());
        }
    }
    info!("{} device(s) loaded", registry.snapshots().await.len());
    registry.log(
        LogLevel::Info,
        format!("server {} started", env!("CARGO_PKG_VERSION")),
    );

    if config.server.usage_sample_interval_mins > 0 {
        usage::spawn(
            Arc::clone(&registry),
            store.clone(),
            Duration::from_secs(config.server.usage_sample_interval_mins * 60),
        );
    }

    let state = Arc::new(api::AppState {
        registry,
        store,
        started: Instant::now(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        config_path: cli.config.display().to_string(),
        bind: config.server.bind.to_string(),
        poll_interval_secs: config.server.poll_interval_secs,
        discovery_broadcast: config.server.discovery_broadcast.clone(),
        usage_sample_interval_mins: config.server.usage_sample_interval_mins,
    });
    let app = api::router(state);

    let listener = TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("cannot bind {}", config.server.bind))?;
    info!("listening on http://{}", config.server.bind);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::warn!("cannot listen for ctrl-c: {err}");
        std::future::pending::<()>().await;
    }
    info!("shutdown requested");
}
