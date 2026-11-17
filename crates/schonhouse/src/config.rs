//! Configuration: the `schonhouse.toml` file plus credentials from the environment.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    /// Optional seed list. Devices are stored in the database; entries here are
    /// imported on startup when their id is not in the database yet.
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
    /// Address the UDP discovery query is broadcast to.
    #[serde(default = "default_discovery_broadcast")]
    pub discovery_broadcast: String,
    /// Minutes between usage (time on, energy) samples. 0 disables sampling.
    #[serde(default = "default_usage_interval")]
    pub usage_sample_interval_mins: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            poll_interval_secs: default_poll_interval(),
            discovery_broadcast: default_discovery_broadcast(),
            usage_sample_interval_mins: default_usage_interval(),
        }
    }
}

fn default_usage_interval() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// SQLite database file. Relative paths are resolved against the working directory.
    #[serde(default = "default_db_path")]
    pub path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
        }
    }
}

fn default_bind() -> SocketAddr {
    "0.0.0.0:8080".parse().expect("valid default bind address")
}

fn default_poll_interval() -> u64 {
    10
}

fn default_discovery_broadcast() -> String {
    "255.255.255.255".to_string()
}

fn default_db_path() -> PathBuf {
    PathBuf::from("schonhouse.db")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        if self
            .server
            .discovery_broadcast
            .parse::<std::net::IpAddr>()
            .is_err()
        {
            bail!("server.discovery_broadcast must be an IP address");
        }
        let mut ids = HashSet::new();
        for device in &self.devices {
            device.validate()?;
            if !ids.insert(device.id.as_str()) {
                bail!("duplicate device id {:?}", device.id);
            }
        }
        Ok(())
    }
}

/// Maximum length of user-provided names (devices, groups, scenes).
pub const MAX_NAME_LEN: usize = 64;

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_NAME_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn valid_name(name: &str) -> bool {
    let trimmed = name.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_NAME_LEN
        && !name.chars().any(char::is_control)
}

impl DeviceConfig {
    pub fn validate(&self) -> Result<()> {
        if !valid_id(&self.id) {
            bail!(
                "device id {:?} must be 1-{MAX_NAME_LEN} letters, digits, '-' or '_'",
                self.id
            );
        }
        if let Some(name) = &self.name
            && !valid_name(name)
        {
            bail!("device {:?} has an invalid name", self.id);
        }
        if self.ip.parse::<std::net::IpAddr>().is_err() {
            bail!("device {:?} has an invalid ip {:?}", self.id, self.ip);
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
    let username = std::env::var("TAPO_USERNAME")
        .ok()
        .filter(|s| !s.is_empty());
    let password = std::env::var("TAPO_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    match (username, password) {
        (Some(username), Some(password)) => Ok(Credentials { username, password }),
        _ => bail!("TAPO_USERNAME and TAPO_PASSWORD must be set (in the environment or in .env)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_to_an_empty_config() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.server.bind.port(), 8080);
        assert_eq!(config.server.poll_interval_secs, 10);
        assert_eq!(config.storage.path, PathBuf::from("schonhouse.db"));
        assert!(config.devices.is_empty());
    }

    #[test]
    fn full_config_parses() {
        let text = r#"
            [server]
            bind = "127.0.0.1:9000"
            poll_interval_secs = 5
            discovery_broadcast = "192.168.0.255"

            [storage]
            path = "/var/lib/schonhouse/db.sqlite"

            [[devices]]
            id = "desk"
            name = "Desk lamp"
            kind = "tapo-l535"
            ip = "192.168.0.2"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.server.bind.port(), 9000);
        assert_eq!(config.devices[0].kind, DeviceKind::TapoL535);
        assert_eq!(config.devices[0].name.as_deref(), Some("Desk lamp"));
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        assert!(toml::from_str::<Config>("[server]\nport = 1").is_err());
        assert!(
            toml::from_str::<Config>(
                "[[devices]]\nid = \"a\"\nkind = \"tapo-p110\"\nip = \"1.1.1.1\""
            )
            .is_err()
        );

        let dup: Config = toml::from_str(
            "[[devices]]\nid = \"a\"\nkind = \"tapo-l535\"\nip = \"1.1.1.1\"\n[[devices]]\nid = \"a\"\nkind = \"tapo-l535\"\nip = \"1.1.1.2\"",
        )
        .unwrap();
        assert!(dup.validate().is_err());

        let bad_ip: Config =
            toml::from_str("[[devices]]\nid = \"a\"\nkind = \"tapo-l535\"\nip = \"not-an-ip\"")
                .unwrap();
        assert!(bad_ip.validate().is_err());

        let bad_id: Config = toml::from_str(
            "[[devices]]\nid = \"has space\"\nkind = \"tapo-l535\"\nip = \"1.1.1.1\"",
        )
        .unwrap();
        assert!(bad_id.validate().is_err());
    }

    #[test]
    fn ids_and_names() {
        assert!(valid_id("light-1"));
        assert!(valid_id("Desk_Lamp2"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(valid_name("Living room"));
        assert!(!valid_name("   "));
        assert!(!valid_name("bad\nname"));
        assert!(!valid_name(&"x".repeat(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn dotenv_does_not_override_existing_env() {
        let dir = std::env::temp_dir().join(format!("schonhouse-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# comment\nSCHONHOUSE_TEST_A=\"from file\"\nSCHONHOUSE_TEST_B=keep\n",
        )
        .unwrap();
        // SAFETY: test-only, single-threaded use of the environment.
        unsafe { std::env::set_var("SCHONHOUSE_TEST_B", "existing") };
        load_dotenv(&path);
        assert_eq!(std::env::var("SCHONHOUSE_TEST_A").unwrap(), "from file");
        assert_eq!(std::env::var("SCHONHOUSE_TEST_B").unwrap(), "existing");
        let _ = std::fs::remove_dir_all(dir);
    }
}
