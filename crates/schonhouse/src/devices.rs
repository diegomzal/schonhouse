//! Device registry and per-device actors.
//!
//! Each configured device runs as an independent Tokio task that owns the connection
//! to the physical device, polls its state, applies requests sent through a channel,
//! and reconnects with exponential backoff when the device becomes unreachable.
//! Changes are published to a shared snapshot map and to a broadcast channel that
//! feeds the Server-Sent Events stream. A bounded in-memory event log is kept for the
//! debug view.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tapo::responses::DeviceInfoColorLightResult;
use tapo::{ApiClient, ColorLightHandler};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::{Config, Credentials, DeviceConfig, DeviceKind};

const MAX_BACKOFF: Duration = Duration::from_secs(60);
const EVENT_BUFFER: usize = 128;
const LOG_CAPACITY: usize = 300;

/// What a device can do. The UI renders controls based on this list, so new device
/// kinds (plugs, white-only bulbs, sensors) only need to declare their capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    Power,
    Brightness,
    ColorTemperature,
    Color,
}

impl DeviceKind {
    pub fn capabilities(self) -> &'static [Capability] {
        match self {
            DeviceKind::TapoL535 => &[
                Capability::Power,
                Capability::Brightness,
                Capability::ColorTemperature,
                Capability::Color,
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LightState {
    pub on: bool,
    /// 1..=100
    pub brightness: u8,
    /// Kelvin; 0 when the bulb is in color (hue/saturation) mode.
    pub color_temp: u16,
    pub hue: Option<u16>,
    pub saturation: Option<u16>,
}

/// Static and diagnostic information reported by the device itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    /// Name given in the vendor app.
    pub nickname: String,
    pub model: String,
    pub device_type: String,
    pub fw_ver: String,
    pub hw_ver: String,
    pub mac: String,
    pub device_id: String,
    pub ssid: String,
    /// Wi-Fi signal in dBm.
    pub rssi: i16,
    /// Vendor signal level, 0..=3.
    pub signal_level: u8,
    /// Seconds since the device was switched on, if reported.
    pub on_time: Option<u64>,
    pub overheated: bool,
    pub region: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceSnapshot {
    pub id: String,
    /// Configured name, else the device nickname, else the id.
    pub name: String,
    pub kind: DeviceKind,
    pub capabilities: Vec<Capability>,
    pub ip: String,
    pub online: bool,
    pub state: Option<LightState>,
    pub info: Option<DeviceInfo>,
    pub error: Option<String>,
    /// Unix timestamp (seconds) of the last update of this snapshot.
    pub updated_at: u64,
    /// Unix timestamp (seconds) of the last successful read from the device.
    pub last_seen: Option<u64>,
}

/// Partial update sent by API clients. Fields that are `None` are left untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightCommand {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_temp: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hue: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation: Option<u8>,
}

impl LightCommand {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(b) = self.brightness
            && !(1..=100).contains(&b)
        {
            return Err("brightness must be between 1 and 100".into());
        }
        if let Some(k) = self.color_temp
            && !(2500..=6500).contains(&k)
        {
            return Err("color_temp must be between 2500 and 6500".into());
        }
        if let Some(h) = self.hue
            && h > 360
        {
            return Err("hue must be between 0 and 360".into());
        }
        if let Some(s) = self.saturation
            && !(1..=100).contains(&s)
        {
            return Err("saturation must be between 1 and 100".into());
        }
        if self.hue.is_some() != self.saturation.is_some() {
            return Err("hue and saturation must be sent together".into());
        }
        if self.on.is_none()
            && self.brightness.is_none()
            && self.color_temp.is_none()
            && self.hue.is_none()
        {
            return Err("empty command".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub ts: u64,
    pub level: LogLevel,
    pub device: Option<String>,
    pub message: String,
}

/// Everything that goes out on the SSE stream.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    Device(DeviceSnapshot),
    Log(LogEntry),
}

#[derive(Debug)]
pub enum CommandError {
    NotFound,
    Invalid(String),
    Device(String),
    Unavailable,
}

type Reply<T> = oneshot::Sender<Result<T, String>>;

enum Request {
    Command { command: LightCommand, reply: Reply<DeviceSnapshot> },
    Refresh { reply: Reply<DeviceSnapshot> },
    Raw { reply: Reply<serde_json::Value> },
    Usage { reply: Reply<serde_json::Value> },
    Reboot { reply: Reply<()> },
}

impl Request {
    fn reject(self, message: String) {
        match self {
            Request::Command { reply, .. } => drop(reply.send(Err(message))),
            Request::Refresh { reply } => drop(reply.send(Err(message))),
            Request::Raw { reply } => drop(reply.send(Err(message))),
            Request::Usage { reply } => drop(reply.send(Err(message))),
            Request::Reboot { reply } => drop(reply.send(Err(message))),
        }
    }
}

/// Shared write side: the snapshot map, the event log and the broadcast.
#[derive(Clone)]
struct Publisher {
    snapshots: Arc<RwLock<BTreeMap<String, DeviceSnapshot>>>,
    log: Arc<Mutex<VecDeque<LogEntry>>>,
    events: broadcast::Sender<ServerEvent>,
}

impl Publisher {
    async fn publish(&self, snapshot: DeviceSnapshot) {
        self.snapshots
            .write()
            .await
            .insert(snapshot.id.clone(), snapshot.clone());
        // Nobody listening is not an error.
        let _ = self.events.send(ServerEvent::Device(snapshot));
    }

    fn log(&self, level: LogLevel, device: Option<&str>, message: impl Into<String>) {
        let entry = LogEntry {
            ts: now(),
            level,
            device: device.map(str::to_string),
            message: message.into(),
        };
        {
            let mut log = self.log.lock().expect("log mutex poisoned");
            if log.len() >= LOG_CAPACITY {
                log.pop_front();
            }
            log.push_back(entry.clone());
        }
        let _ = self.events.send(ServerEvent::Log(entry));
    }
}

pub struct Registry {
    publisher: Publisher,
    senders: BTreeMap<String, mpsc::Sender<Request>>,
}

impl Registry {
    /// Spawns one actor per configured device and returns the shared registry.
    pub async fn start(config: &Config, credentials: &Credentials, poll: Duration) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let publisher = Publisher {
            snapshots: Arc::new(RwLock::new(BTreeMap::new())),
            log: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAPACITY))),
            events,
        };
        let client = ApiClient::new(
            credentials.username.clone(),
            credentials.password.clone(),
        )
        .with_timeout(Duration::from_secs(10));

        let mut senders = BTreeMap::new();
        for device in &config.devices {
            let (tx, rx) = mpsc::channel(16);
            senders.insert(device.id.clone(), tx);
            let tracked = Tracked::new(device.clone());
            publisher
                .snapshots
                .write()
                .await
                .insert(device.id.clone(), tracked.snapshot(false, None));
            match device.kind {
                DeviceKind::TapoL535 => {
                    tokio::spawn(run_light(tracked, client.clone(), poll, rx, publisher.clone()));
                }
            }
        }
        Arc::new(Self { publisher, senders })
    }

    pub async fn snapshots(&self) -> Vec<DeviceSnapshot> {
        self.publisher.snapshots.read().await.values().cloned().collect()
    }

    pub async fn snapshot(&self, id: &str) -> Option<DeviceSnapshot> {
        self.publisher.snapshots.read().await.get(id).cloned()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.publisher.events.subscribe()
    }

    pub fn recent_log(&self) -> Vec<LogEntry> {
        self.publisher
            .log
            .lock()
            .expect("log mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.publisher.log(level, None, message);
    }

    pub async fn command(
        &self,
        id: &str,
        command: LightCommand,
    ) -> Result<DeviceSnapshot, CommandError> {
        command.validate().map_err(CommandError::Invalid)?;
        self.ask(id, |reply| Request::Command { command, reply }).await
    }

    pub async fn refresh(&self, id: &str) -> Result<DeviceSnapshot, CommandError> {
        self.ask(id, |reply| Request::Refresh { reply }).await
    }

    pub async fn raw(&self, id: &str) -> Result<serde_json::Value, CommandError> {
        self.ask(id, |reply| Request::Raw { reply }).await
    }

    pub async fn usage(&self, id: &str) -> Result<serde_json::Value, CommandError> {
        self.ask(id, |reply| Request::Usage { reply }).await
    }

    pub async fn reboot(&self, id: &str) -> Result<(), CommandError> {
        self.ask(id, |reply| Request::Reboot { reply }).await
    }

    /// Sends a request to a device actor and waits for its answer.
    async fn ask<T>(
        &self,
        id: &str,
        make: impl FnOnce(Reply<T>) -> Request,
    ) -> Result<T, CommandError> {
        let sender = self.senders.get(id).ok_or(CommandError::NotFound)?;
        let (reply, rx) = oneshot::channel();
        sender
            .send(make(reply))
            .await
            .map_err(|_| CommandError::Unavailable)?;
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(CommandError::Device(message)),
            Err(_) => Err(CommandError::Unavailable),
        }
    }
}

/// First sentence of a device error, capped, for the event log. Vendor errors can be
/// paragraphs long; the full text stays in the snapshot's `error` field.
fn short(message: &str) -> String {
    let first = message
        .split_once(". ")
        .map(|(first, _)| format!("{first}."))
        .unwrap_or_else(|| message.to_string());
    if first.chars().count() > 200 {
        let cut: String = first.chars().take(200).collect();
        format!("{cut}…")
    } else {
        first
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Last known data for one device, owned by its actor.
struct Tracked {
    device: DeviceConfig,
    state: Option<LightState>,
    info: Option<DeviceInfo>,
    last_seen: Option<u64>,
}

impl Tracked {
    fn new(device: DeviceConfig) -> Self {
        Self {
            device,
            state: None,
            info: None,
            last_seen: None,
        }
    }

    fn id(&self) -> &str {
        &self.device.id
    }

    fn snapshot(&self, online: bool, error: Option<String>) -> DeviceSnapshot {
        let name = self
            .device
            .name
            .clone()
            .or_else(|| {
                self.info
                    .as_ref()
                    .map(|i| i.nickname.trim().to_string())
                    .filter(|n| !n.is_empty())
            })
            .unwrap_or_else(|| self.device.id.clone());
        DeviceSnapshot {
            id: self.device.id.clone(),
            name,
            kind: self.device.kind,
            capabilities: self.device.kind.capabilities().to_vec(),
            ip: self.device.ip.clone(),
            online,
            state: self.state.clone(),
            info: self.info.clone(),
            error,
            updated_at: now(),
            last_seen: self.last_seen,
        }
    }

    /// Records a fresh device read. Returns whether anything visible changed.
    fn update(&mut self, raw: &DeviceInfoColorLightResult) -> bool {
        let state = LightState {
            on: raw.device_on,
            brightness: raw.brightness,
            color_temp: raw.color_temp,
            hue: raw.hue,
            saturation: raw.saturation,
        };
        let info = DeviceInfo {
            nickname: raw.nickname.clone(),
            model: raw.model.clone(),
            device_type: raw.r#type.clone(),
            fw_ver: raw.fw_ver.clone(),
            hw_ver: raw.hw_ver.clone(),
            mac: raw.mac.clone(),
            device_id: raw.device_id.clone(),
            ssid: raw.ssid.clone(),
            rssi: raw.rssi,
            signal_level: raw.signal_level,
            on_time: raw.on_time,
            overheated: raw.overheated,
            region: raw.region.clone(),
        };
        let changed = self.state.as_ref() != Some(&state) || self.info.as_ref() != Some(&info);
        self.state = Some(state);
        self.info = Some(info);
        self.last_seen = Some(now());
        changed
    }
}

/// Applies a partial update. Color/brightness go first so that an explicit `on: false`
/// in the same command still wins (setting brightness turns a Tapo bulb on).
async fn apply(handler: &ColorLightHandler, command: &LightCommand) -> Result<(), tapo::Error> {
    if let Some(brightness) = command.brightness {
        handler.set_brightness(brightness).await?;
    }
    if let Some(kelvin) = command.color_temp {
        handler.set_color_temperature(kelvin).await?;
    }
    if let (Some(hue), Some(saturation)) = (command.hue, command.saturation) {
        handler.set_hue_saturation(hue, saturation).await?;
    }
    match command.on {
        Some(true) => handler.on().await?,
        Some(false) => handler.off().await?,
        None => {}
    }
    Ok(())
}

/// Marks the device offline after a failed operation and returns the error text.
/// Background polls log as `warn`; failures of user-initiated requests log as `error`.
async fn broken(
    publisher: &Publisher,
    tracked: &Tracked,
    level: LogLevel,
    what: &str,
    err: &tapo::Error,
) -> String {
    let message = err.to_string();
    warn!(device = %tracked.id(), "{what}: {message}");
    publisher.log(level, Some(tracked.id()), format!("{what}: {}", short(&message)));
    publisher
        .publish(tracked.snapshot(false, Some(message.clone())))
        .await;
    message
}

/// Serves one request while connected. `Err(())` means the session is broken and the
/// actor must reconnect.
async fn handle_request(
    handler: &ColorLightHandler,
    tracked: &mut Tracked,
    publisher: &Publisher,
    request: Request,
) -> Result<(), ()> {
    match request {
        Request::Command { command, reply } => {
            let result = match apply(handler, &command).await {
                Ok(()) => handler.get_device_info().await,
                Err(err) => Err(err),
            };
            match result {
                Ok(raw) => {
                    tracked.update(&raw);
                    let snapshot = tracked.snapshot(true, None);
                    let described = serde_json::to_string(&command).unwrap_or_default();
                    debug!(device = %tracked.id(), "applied {described}");
                    publisher.log(LogLevel::Info, Some(tracked.id()), format!("applied {described}"));
                    publisher.publish(snapshot.clone()).await;
                    let _ = reply.send(Ok(snapshot));
                    Ok(())
                }
                Err(err) => {
                    let message =
                        broken(publisher, tracked, LogLevel::Error, "command failed", &err).await;
                    let _ = reply.send(Err(message));
                    Err(())
                }
            }
        }
        Request::Refresh { reply } => match handler.get_device_info().await {
            Ok(raw) => {
                tracked.update(&raw);
                let snapshot = tracked.snapshot(true, None);
                publisher.publish(snapshot.clone()).await;
                let _ = reply.send(Ok(snapshot));
                Ok(())
            }
            Err(err) => {
                let message =
                    broken(publisher, tracked, LogLevel::Error, "refresh failed", &err).await;
                let _ = reply.send(Err(message));
                Err(())
            }
        },
        Request::Raw { reply } => match handler.get_device_info().await {
            Ok(raw) => {
                if tracked.update(&raw) {
                    publisher.publish(tracked.snapshot(true, None)).await;
                }
                let _ = reply.send(serde_json::to_value(&raw).map_err(|e| e.to_string()));
                Ok(())
            }
            Err(err) => {
                let message =
                    broken(publisher, tracked, LogLevel::Error, "raw read failed", &err).await;
                let _ = reply.send(Err(message));
                Err(())
            }
        },
        Request::Usage { reply } => match handler.get_device_usage().await {
            Ok(usage) => {
                let _ = reply.send(serde_json::to_value(&usage).map_err(|e| e.to_string()));
                Ok(())
            }
            Err(err) => {
                let message =
                    broken(publisher, tracked, LogLevel::Error, "usage read failed", &err).await;
                let _ = reply.send(Err(message));
                Err(())
            }
        },
        // A short delay lets the device acknowledge the request before it goes down.
        Request::Reboot { reply } => match handler.device_reboot(1).await {
            Ok(()) => {
                info!(device = %tracked.id(), "reboot requested");
                publisher.log(LogLevel::Warn, Some(tracked.id()), "reboot requested");
                publisher
                    .publish(tracked.snapshot(false, Some("rebooting".into())))
                    .await;
                let _ = reply.send(Ok(()));
                // The session is gone after a reboot: reconnect from scratch.
                Err(())
            }
            Err(err) => {
                let message =
                    broken(publisher, tracked, LogLevel::Error, "reboot failed", &err).await;
                let _ = reply.send(Err(message));
                Err(())
            }
        },
    }
}

/// Actor for a Tapo color bulb.
async fn run_light(
    mut tracked: Tracked,
    client: ApiClient,
    poll: Duration,
    mut requests: mpsc::Receiver<Request>,
    publisher: Publisher,
) {
    let mut backoff = Duration::from_secs(1);
    let mut ticker = tokio::time::interval(poll);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Connect (KLAP handshake). While offline, answer requests immediately with an
        // error instead of queueing them, and retry with exponential backoff.
        let handler = match client.clone().l535(&tracked.device.ip).await {
            Ok(handler) => {
                info!(device = %tracked.id(), ip = %tracked.device.ip, "connected");
                publisher.log(
                    LogLevel::Info,
                    Some(tracked.id()),
                    format!("connected to {}", tracked.device.ip),
                );
                backoff = Duration::from_secs(1);
                handler
            }
            Err(err) => {
                let message = err.to_string();
                warn!(device = %tracked.id(), "connect failed: {message} (retry in {backoff:?})");
                publisher.log(
                    LogLevel::Warn,
                    Some(tracked.id()),
                    format!(
                        "connect failed, retry in {}s: {}",
                        backoff.as_secs(),
                        short(&message)
                    ),
                );
                publisher
                    .publish(tracked.snapshot(false, Some(message.clone())))
                    .await;
                let sleep = tokio::time::sleep(backoff);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        request = requests.recv() => match request {
                            None => return,
                            Some(request) => request.reject(format!("device offline: {message}")),
                        },
                    }
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        // Connected: serve polls and requests until the first error.
        ticker.reset_immediately();
        loop {
            tokio::select! {
                _ = ticker.tick() => match handler.get_device_info().await {
                    Ok(raw) => {
                        if tracked.update(&raw) {
                            debug!(device = %tracked.id(), "state changed");
                            publisher.publish(tracked.snapshot(true, None)).await;
                        }
                    }
                    Err(err) => {
                        broken(&publisher, &tracked, LogLevel::Warn, "poll failed", &err).await;
                        break;
                    }
                },
                request = requests.recv() => {
                    let Some(request) = request else { return };
                    if handle_request(&handler, &mut tracked, &publisher, request).await.is_err() {
                        break;
                    }
                }
            }
        }
        // Fall through to reconnect.
    }
}
