//! Schonhouse: a lightweight home assistant service.
//!
//! Loads `schonhouse.toml`, starts one actor per configured device, and serves a
//! REST API, a Server-Sent Events stream and an embedded web UI on the local network.

mod api;
mod config;
mod devices;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::devices::Registry;

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
    info!(
        "loaded {} with {} device(s)",
        cli.config.display(),
        config.devices.len()
    );

    let poll = Duration::from_secs(config.server.poll_interval_secs);
    let registry = Registry::start(&config, &credentials, poll).await;
    registry.log(
        devices::LogLevel::Info,
        format!("server {} started", env!("CARGO_PKG_VERSION")),
    );
    let state = Arc::new(api::AppState {
        registry,
        started: Instant::now(),
        started_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        config_path: cli.config.display().to_string(),
        bind: config.server.bind.to_string(),
        poll_interval_secs: config.server.poll_interval_secs,
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
