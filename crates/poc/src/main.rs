//! Proof of concept: control two Tapo L535 bulbs over the local network.
//!
//! Usage (from the repository root):
//!
//! ```text
//! cargo run -p poc -- status
//! cargo run -p poc -- on
//! cargo run -p poc -- off
//! cargo run -p poc -- toggle
//! cargo run -p poc -- brightness 40          # 1..100
//! cargo run -p poc -- temp 2700              # color temperature in Kelvin (2500..6500)
//! cargo run -p poc -- color 120 100          # hue 0..360, saturation 1..100
//! cargo run -p poc -- --ip 192.168.0.2 on    # act on a single bulb
//! ```
//!
//! Credentials come from the environment or from a `.env` file in the current
//! directory: `TAPO_USERNAME`, `TAPO_PASSWORD` and optionally `BULB_IPS`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tapo::ApiClient;

const DEFAULT_IPS: &str = "192.168.0.2,192.168.0.3";

#[derive(Parser)]
#[command(about = "Tapo L535 proof of concept")]
struct Cli {
    /// Bulb IP address (repeatable). Defaults to BULB_IPS or 192.168.0.2,192.168.0.3.
    #[arg(long)]
    ip: Vec<String>,

    /// Print raw device info as JSON.
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Print device info
    Status,
    /// Turn on
    On,
    /// Turn off
    Off,
    /// Invert the current on/off state
    Toggle,
    /// Set brightness 1..100
    Brightness { value: u8 },
    /// Set color temperature in Kelvin (2500..6500)
    Temp { kelvin: u16 },
    /// Set hue (0..360) and saturation (1..100)
    Color { hue: u16, saturation: u8 },
}

/// Minimal .env loader: KEY=VALUE lines, '#' comments, never overrides the real env.
fn load_dotenv(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if std::env::var_os(key).is_none() {
            // SAFETY: called at startup before any other thread exists.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

fn summarize(ip: &str, info: &serde_json::Value) -> String {
    let state = if info["device_on"].as_bool().unwrap_or(false) { "ON " } else { "OFF" };
    let mut parts = vec![
        format!("{ip:<14} {state}"),
        format!("nick={:?}", info["nickname"].as_str().unwrap_or("")),
        format!("model={}", info["model"].as_str().unwrap_or("?")),
        format!("fw={}", info["fw_ver"].as_str().unwrap_or("?")),
        format!("brightness={}", info["brightness"]),
    ];
    match info["color_temp"].as_u64() {
        Some(k) if k > 0 => parts.push(format!("color_temp={k}K")),
        _ => parts.push(format!("hue={} sat={}", info["hue"], info["saturation"])),
    }
    parts.push(format!("rssi={}", info["rssi"]));
    parts.join("  ")
}

async fn run_one(client: ApiClient, ip: String, command: Command, json: bool) -> Result<()> {
    let started = Instant::now();
    let device = client.l535(&ip).await.context("KLAP handshake failed")?;
    let connect_ms = started.elapsed().as_millis();

    match command {
        Command::Status => {}
        Command::On => device.on().await?,
        Command::Off => device.off().await?,
        Command::Toggle => {
            if device.get_device_info().await?.device_on {
                device.off().await?
            } else {
                device.on().await?
            }
        }
        Command::Brightness { value } => device.set_brightness(value).await?,
        Command::Temp { kelvin } => device.set_color_temperature(kelvin).await?,
        Command::Color { hue, saturation } => device.set_hue_saturation(hue, saturation).await?,
    }

    let info = serde_json::to_value(device.get_device_info().await?)?;
    let total_ms = started.elapsed().as_millis();
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "ip": ip, "info": info }))?);
    } else {
        println!("{}", summarize(&ip, &info));
    }
    println!("{:<14} handshake {connect_ms} ms, total {total_ms} ms", "");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv(Path::new(".env"));
    let cli = Cli::parse();

    let username = std::env::var("TAPO_USERNAME").ok().filter(|s| !s.is_empty());
    let password = std::env::var("TAPO_PASSWORD").ok().filter(|s| !s.is_empty());
    let (Some(username), Some(password)) = (username, password) else {
        eprintln!(
            "Missing TAPO_USERNAME / TAPO_PASSWORD. Copy .env.example to .env and fill in your Tapo account credentials."
        );
        std::process::exit(2);
    };

    let ips: Vec<String> = if cli.ip.is_empty() {
        std::env::var("BULB_IPS")
            .unwrap_or_else(|_| DEFAULT_IPS.to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        cli.ip
    };

    let client = ApiClient::new(username, password).with_timeout(Duration::from_secs(10));

    let tasks: Vec<_> = ips
        .iter()
        .map(|ip| tokio::spawn(run_one(client.clone(), ip.clone(), cli.command.clone(), cli.json)))
        .collect();

    let mut failures = 0;
    for (ip, task) in ips.iter().zip(tasks) {
        if let Err(err) = task.await? {
            failures += 1;
            eprintln!("{ip:<14} ERROR {err:#}");
        }
    }
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}
