//! LAN discovery of Tapo devices over UDP broadcast (port 20002).
//!
//! Uses the vendor's unauthenticated discovery query, so it only yields what devices
//! announce publicly: ip, mac, model, type and protocol. Nothing is logged into.

use std::collections::BTreeMap;

use serde::Serialize;
use tapo::ApiClient;
use tokio_stream::StreamExt;
use tracing::warn;

use crate::config::DeviceKind;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Discovered {
    pub ip: String,
    pub mac: Option<String>,
    pub model: Option<String>,
    pub device_type: Option<String>,
    pub device_id: Option<String>,
    pub protocol: Option<String>,
    /// The kind this server can control, if the model is supported.
    pub kind: Option<DeviceKind>,
    /// Id of the already configured device with this ip, if any.
    pub known_as: Option<String>,
}

/// Maps a vendor model string to a supported kind.
pub fn kind_for_model(model: &str) -> Option<DeviceKind> {
    let base: String = model
        .trim()
        .to_ascii_uppercase()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    match base.as_str() {
        "L535" | "L535E" | "L535B" => Some(DeviceKind::TapoL535),
        _ => None,
    }
}

fn from_message(ip: String, message: &serde_json::Value) -> Discovered {
    let result = message.get("result").cloned().unwrap_or_default();
    let text = |key: &str| result.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let model = text("device_model");
    Discovered {
        ip,
        mac: text("mac"),
        kind: model.as_deref().and_then(kind_for_model),
        model,
        device_type: text("device_type"),
        device_id: text("device_id"),
        protocol: result
            .pointer("/mgt_encrypt_schm/encrypt_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        known_as: None,
    }
}

/// Broadcasts a discovery query and collects answers until `timeout_s` elapses.
pub async fn scan(broadcast: &str, timeout_s: u64) -> Result<Vec<Discovered>, String> {
    let mut stream = ApiClient::discover_devices_raw(broadcast, timeout_s)
        .await
        .map_err(|e| e.to_string())?;
    let mut found: BTreeMap<String, Discovered> = BTreeMap::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(raw) => {
                let ip = raw.ip.to_string();
                found
                    .entry(ip.clone())
                    .or_insert_with(|| from_message(ip, &raw.message));
            }
            Err(err) => warn!("discovery error: {err}"),
        }
    }
    let mut list: Vec<Discovered> = found.into_values().collect();
    list.sort_by_key(|d| {
        d.ip.parse::<std::net::IpAddr>()
            .ok()
            .map(|ip| ip.to_string())
            .unwrap_or_default()
    });
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_mapping() {
        assert_eq!(kind_for_model("L535"), Some(DeviceKind::TapoL535));
        assert_eq!(kind_for_model("l535e(eu)"), Some(DeviceKind::TapoL535));
        assert_eq!(kind_for_model("P110"), None);
        assert_eq!(kind_for_model(""), None);
    }

    #[test]
    fn parses_discovery_message() {
        let message = serde_json::json!({
            "result": {
                "device_type": "SMART.TAPOBULB",
                "device_model": "L535E(EU)",
                "mac": "AA-BB-CC-DD-EE-FF",
                "device_id": "abc",
                "mgt_encrypt_schm": { "encrypt_type": "KLAP", "http_port": 80 }
            },
            "error_code": 0
        });
        let d = from_message("192.168.0.2".into(), &message);
        assert_eq!(d.model.as_deref(), Some("L535E(EU)"));
        assert_eq!(d.kind, Some(DeviceKind::TapoL535));
        assert_eq!(d.protocol.as_deref(), Some("KLAP"));
        assert_eq!(d.mac.as_deref(), Some("AA-BB-CC-DD-EE-FF"));

        let empty = from_message("1.2.3.4".into(), &serde_json::Value::Null);
        assert_eq!(empty.model, None);
        assert_eq!(empty.kind, None);
    }
}
