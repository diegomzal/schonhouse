//! Periodic sampling of device usage counters (time on, energy) into the store.
//!
//! Tapo bulbs report minutes of use and Wh for today, the past 7 and the past 30 days.
//! One reading per device per interval is enough to draw daily charts; samples older
//! than `RETENTION` are pruned.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use crate::devices::Registry;
use crate::store::{Store, UsageSample};

const RETENTION: Duration = Duration::from_secs(90 * 24 * 3600);
const FIRST_SAMPLE_DELAY: Duration = Duration::from_secs(90);

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Converts the vendor's usage JSON into a sample. Missing counters stay `None`.
pub fn parse_usage(device_id: &str, ts: i64, value: &serde_json::Value) -> UsageSample {
    let get = |section: &str, period: &str| {
        value
            .pointer(&format!("/{section}/{period}"))
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)))
    };
    UsageSample {
        device_id: device_id.to_string(),
        ts,
        time_today: get("time_usage", "today"),
        time_past7: get("time_usage", "past7"),
        time_past30: get("time_usage", "past30"),
        power_today: get("power_usage", "today"),
        power_past7: get("power_usage", "past7"),
        power_past30: get("power_usage", "past30"),
    }
}

/// Reads the usage counters of every online device once. Returns how many samples
/// were stored.
pub async fn sample_all(registry: &Registry, store: &Store) -> usize {
    let ts = now();
    let mut stored = 0;
    for device in registry.snapshots().await {
        if !device.online {
            continue;
        }
        match registry.usage(&device.id).await {
            Ok(value) => {
                let sample = parse_usage(&device.id, ts, &value);
                match store.insert_usage_sample(sample).await {
                    Ok(()) => stored += 1,
                    Err(err) => warn!(device = %device.id, "usage sample not stored: {err}"),
                }
            }
            Err(err) => debug!(device = %device.id, "usage not read: {}", err.message()),
        }
    }
    stored
}

/// Background task: samples every `interval`, starting shortly after boot so the
/// devices have had time to connect.
pub fn spawn(registry: Arc<Registry>, store: Store, interval: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SAMPLE_DELAY).await;
        loop {
            let stored = sample_all(&registry, &store).await;
            info!("usage sampled for {stored} device(s)");
            if let Err(err) = store.prune_usage(now() - RETENTION.as_secs() as i64).await {
                warn!("usage prune failed: {err}");
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vendor_usage() {
        let value = serde_json::json!({
            "time_usage": { "today": 12, "past7": 340, "past30": 1200 },
            "power_usage": { "today": 3, "past7": 80.0, "past30": 300 },
            "saved_power": { "today": 1, "past7": 2, "past30": 3 }
        });
        let s = parse_usage("a", 42, &value);
        assert_eq!(s.device_id, "a");
        assert_eq!(s.ts, 42);
        assert_eq!(
            (s.time_today, s.time_past7, s.time_past30),
            (Some(12), Some(340), Some(1200))
        );
        assert_eq!(
            (s.power_today, s.power_past7, s.power_past30),
            (Some(3), Some(80), Some(300))
        );

        let partial = parse_usage("a", 1, &serde_json::json!({ "time_usage": { "today": 5 } }));
        assert_eq!(partial.time_today, Some(5));
        assert_eq!(partial.power_today, None);
    }
}
