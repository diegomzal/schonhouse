//! Configuration: the `schonhouse.toml` file plus credentials from the environment.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address and port the HTTP server listens on.
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// How often each device is polled for its current state, in seconds.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            poll_interval_secs: default_poll_interval(),
        }
    }
}

fn default_bind() -> SocketAddr {
    "0.0.0.0:8080".parse().expect("valid default bind address")
}

fn default_poll_interval() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// Stable identifier used in URLs (letters, digits, '-' and '_').
    pub id: String,
    /// Optional display name. When absent, the name reported by the device (the
    /// nickname set in the vendor app) is used.
    #[serde(default)]
    pub name: Option<String>,
    pub kind: DeviceKind,
    /// IPv4 address on the local network.
    pub ip: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    TapoL535,
}

pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.server.poll_interval_secs == 0 {
            bail!("server.poll_interval_secs must be at least 1");
        }
        let mut ids = HashSet::new();
        for device in &self.devices {
            if device.id.is_empty()
                || !device
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!(
                    "device id {:?} must contain only letters, digits, '-' or '_'",
                    device.id
                );
            }
            if !ids.insert(device.id.as_str()) {
                bail!("duplicate device id {:?}", device.id);
            }
            if device.ip.parse::<std::net::IpAddr>().is_err() {
                bail!("device {:?} has an invalid ip {:?}", device.id, device.ip);
            }
        }
        Ok(())
    }
}

/// Minimal .env loader: KEY=VALUE lines, '#' comments, never overrides the real env.
pub fn load_dotenv(path: &Path) {
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

pub fn credentials_from_env() -> Result<Credentials> {
    let username = std::env::var("TAPO_USERNAME").ok().filter(|s| !s.is_empty());
    let password = std::env::var("TAPO_PASSWORD").ok().filter(|s| !s.is_empty());
    match (username, password) {
        (Some(username), Some(password)) => Ok(Credentials { username, password }),
        _ => bail!(
            "TAPO_USERNAME and TAPO_PASSWORD must be set (in the environment or in .env)"
        ),
    }
}
