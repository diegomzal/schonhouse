//! Device registry and per-device actors.
//!
//! Each device runs as an independent Tokio task that owns the connection to the
//! physical device, polls its state, applies requests sent through a channel, and
//! reconnects with exponential backoff when the device becomes unreachable. Devices can
//! be added and removed at runtime. Changes are published to a shared snapshot map and
//! to a broadcast channel that feeds the Server-Sent Events stream. A bounded in-memory
//! event log is kept for the debug view.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tapo::responses::DeviceInfoColorLightResult;
use tapo::{ApiClient, ColorLightHandler};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::config::{Credentials, DeviceConfig, DeviceKind};

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

impl LightState {
    /// The command that reproduces this state, used when capturing scenes.
    pub fn to_command(&self) -> LightCommand {
        if !self.on {
            return LightCommand {
                on: Some(false),
                ..Default::default()
            };
        }
        let mut command = LightCommand {
            on: Some(true),
            brightness: Some(self.brightness),
            ..Default::default()
        };
        if self.color_temp > 0 {
            command.color_temp = Some(self.color_temp);
        } else if let (Some(hue), Some(saturation)) = (self.hue, self.saturation) {
            command.hue = Some(hue);
            command.saturation = Some(saturation.min(100) as u8);
        }
        command
    }
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
    /// The configured override, if any (so the UI can edit it).
    pub custom_name: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    Device(Box<DeviceSnapshot>),
    DeviceRemoved(String),
    /// A stored collection ("groups" or "scenes") changed; clients should reload it.
    Changed(&'static str),
    Log(LogEntry),
}

#[derive(Debug)]
pub enum CommandError {
    NotFound,
    Invalid(String),
    Device(String),
    Unavailable,
}

impl CommandError {
    pub fn message(&self) -> String {
        match self {
            CommandError::NotFound => "unknown device".into(),
            CommandError::Invalid(m) | CommandError::Device(m) => m.clone(),
            CommandError::Unavailable => "device worker is not running".into(),
        }
    }
}

/// Outcome of applying a command to one device as part of a batch.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyResult {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<DeviceSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type Reply<T> = oneshot::Sender<Result<T, String>>;

enum Request {
    Command {
        command: LightCommand,
        reply: Reply<DeviceSnapshot>,
    },
    Refresh {
        reply: Reply<DeviceSnapshot>,
    },
    Raw {
        reply: Reply<serde_json::Value>,
    },
    Usage {
        reply: Reply<serde_json::Value>,
    },
    Reboot {
        reply: Reply<()>,
    },
    SetName {
        name: Option<String>,
        reply: Reply<DeviceSnapshot>,
    },
}

impl Request {
    fn reject(self, message: String) {
        match self {
            Request::Command { reply, .. } => drop(reply.send(Err(message))),
            Request::Refresh { reply } => drop(reply.send(Err(message))),
            Request::Raw { reply } => drop(reply.send(Err(message))),
            Request::Usage { reply } => drop(reply.send(Err(message))),
            Request::Reboot { reply } => drop(reply.send(Err(message))),
            Request::SetName { reply, .. } => drop(reply.send(Err(message))),
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
        let mut map = self.snapshots.write().await;
        // A removed device may still have an actor winding down: drop its updates.
        if !map.contains_key(&snapshot.id) {
            return;
        }
        map.insert(snapshot.id.clone(), snapshot.clone());
        drop(map);
        // Nobody listening is not an error.
        let _ = self.events.send(ServerEvent::Device(Box::new(snapshot)));
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
    senders: RwLock<BTreeMap<String, mpsc::Sender<Request>>>,
    client: ApiClient,
    poll: Duration,
}

impl Registry {
    pub fn new(credentials: &Credentials, poll: Duration) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let publisher = Publisher {
            snapshots: Arc::new(RwLock::new(BTreeMap::new())),
            log: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAPACITY))),
            events,
        };
        let client = ApiClient::new(credentials.username.clone(), credentials.password.clone())
            .with_timeout(Duration::from_secs(10));
        Arc::new(Self {
            publisher,
            senders: RwLock::new(BTreeMap::new()),
            client,
            poll,
        })
    }

    /// Starts an actor for the device. Fails if the id is already registered.
    pub async fn add(&self, device: DeviceConfig) -> Result<DeviceSnapshot, CommandError> {
        device
            .validate()
            .map_err(|e| CommandError::Invalid(e.to_string()))?;
        let mut senders = self.senders.write().await;
        if senders.contains_key(&device.id) {
            return Err(CommandError::Invalid(format!(
                "device {:?} already exists",
                device.id
            )));
        }
        let (tx, rx) = mpsc::channel(16);
        senders.insert(device.id.clone(), tx);
        let tracked = Tracked::new(device.clone());
        let snapshot = tracked.snapshot(false, None);
        self.publisher
            .snapshots
            .write()
            .await
            .insert(device.id.clone(), snapshot.clone());
        drop(senders);
        match device.kind {
            DeviceKind::TapoL535 => {
                tokio::spawn(run_light(
                    tracked,
                    self.client.clone(),
                    self.poll,
                    rx,
                    self.publisher.clone(),
                ));
            }
        }
        let _ = self
            .publisher
            .events
            .send(ServerEvent::Device(Box::new(snapshot.clone())));
        self.publisher.log(
            LogLevel::Info,
            Some(&device.id),
            format!("added ({})", device.ip),
        );
        Ok(snapshot)
    }

    /// Stops the actor and forgets the device. Returns `false` if it was unknown.
    pub async fn remove(&self, id: &str) -> bool {
        let removed = self.senders.write().await.remove(id).is_some();
        if removed {
            self.publisher.snapshots.write().await.remove(id);
            let _ = self
                .publisher
                .events
                .send(ServerEvent::DeviceRemoved(id.to_string()));
            self.publisher.log(LogLevel::Info, Some(id), "removed");
        }
        removed
    }

    pub async fn rename(
        &self,
        id: &str,
        name: Option<String>,
    ) -> Result<DeviceSnapshot, CommandError> {
        self.ask(id, |reply| Request::SetName { name, reply }).await
    }

    pub fn notify_changed(&self, what: &'static str) {
        let _ = self.publisher.events.send(ServerEvent::Changed(what));
    }

    pub async fn snapshots(&self) -> Vec<DeviceSnapshot> {
        self.publisher
            .snapshots
            .read()
            .await
            .values()
            .cloned()
            .collect()
    }

    pub async fn snapshot(&self, id: &str) -> Option<DeviceSnapshot> {
        self.publisher.snapshots.read().await.get(id).cloned()
    }

    pub async fn contains(&self, id: &str) -> bool {
        self.senders.read().await.contains_key(id)
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
        self.ask(id, |reply| Request::Command { command, reply })
            .await
    }

    /// Applies one command per device concurrently and reports every outcome in the
    /// order given.
    pub async fn apply_many(
        self: &Arc<Self>,
        items: Vec<(String, LightCommand)>,
    ) -> Vec<ApplyResult> {
        let mut set = JoinSet::new();
        for (index, (id, command)) in items.into_iter().enumerate() {
            let registry = Arc::clone(self);
            set.spawn(async move {
                let result = registry.command(&id, command).await;
                (index, id, result)
            });
        }
        let mut results: Vec<Option<ApplyResult>> = Vec::new();
        while let Some(joined) = set.join_next().await {
            let Ok((index, id, result)) = joined else {
                continue;
            };
            if results.len() <= index {
                results.resize_with(index + 1, || None);
            }
            results[index] = Some(match result {
                Ok(snapshot) => ApplyResult {
                    id,
                    ok: true,
                    snapshot: Some(snapshot),
                    error: None,
                },
                Err(err) => ApplyResult {
                    id,
                    ok: false,
                    snapshot: None,
                    error: Some(err.message()),
                },
            });
        }
        results.into_iter().flatten().collect()
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
        let sender = self
            .senders
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or(CommandError::NotFound)?;
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
pub fn short(message: &str) -> String {
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
            custom_name: self.device.name.clone(),
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
    publisher.log(
        level,
        Some(tracked.id()),
        format!("{what}: {}", short(&message)),
    );
    publisher
        .publish(tracked.snapshot(false, Some(message.clone())))
        .await;
    message
}

/// Renames work whether the device is online or not.
async fn set_name(
    tracked: &mut Tracked,
    publisher: &Publisher,
    online: bool,
    error: Option<String>,
    name: Option<String>,
    reply: Reply<DeviceSnapshot>,
) {
    tracked.device.name = name;
    let snapshot = tracked.snapshot(online, error);
    publisher.publish(snapshot.clone()).await;
    let _ = reply.send(Ok(snapshot));
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
                    publisher.log(
                        LogLevel::Info,
                        Some(tracked.id()),
                        format!("applied {described}"),
                    );
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
                let message = broken(
                    publisher,
                    tracked,
                    LogLevel::Error,
                    "usage read failed",
                    &err,
                )
                .await;
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
        Request::SetName { name, reply } => {
            set_name(tracked, publisher, true, None, name, reply).await;
            Ok(())
        }
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
                            Some(Request::SetName { name, reply }) => {
                                set_name(&mut tracked, &publisher, false, Some(message.clone()), name, reply).await;
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_validation() {
        assert!(LightCommand::default().validate().is_err());
        assert!(
            LightCommand {
                on: Some(true),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            LightCommand {
                brightness: Some(0),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            LightCommand {
                brightness: Some(100),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
        assert!(
            LightCommand {
                color_temp: Some(2400),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            LightCommand {
                hue: Some(10),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            LightCommand {
                hue: Some(361),
                saturation: Some(50),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            LightCommand {
                hue: Some(360),
                saturation: Some(1),
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn command_json_rejects_unknown_fields() {
        assert!(serde_json::from_str::<LightCommand>(r#"{"on":true,"colour":1}"#).is_err());
        let c: LightCommand = serde_json::from_str(r#"{"brightness":40}"#).unwrap();
        assert_eq!(c.brightness, Some(40));
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"brightness":40}"#);
    }

    #[test]
    fn state_to_command() {
        let off = LightState {
            on: false,
            brightness: 50,
            color_temp: 0,
            hue: Some(1),
            saturation: Some(1),
        };
        assert_eq!(
            off.to_command(),
            LightCommand {
                on: Some(false),
                ..Default::default()
            }
        );

        let white = LightState {
            on: true,
            brightness: 60,
            color_temp: 2700,
            hue: None,
            saturation: None,
        };
        let c = white.to_command();
        assert_eq!(
            (c.on, c.brightness, c.color_temp, c.hue),
            (Some(true), Some(60), Some(2700), None)
        );

        let color = LightState {
            on: true,
            brightness: 30,
            color_temp: 0,
            hue: Some(120),
            saturation: Some(100),
        };
        let c = color.to_command();
        assert_eq!(
            (c.color_temp, c.hue, c.saturation),
            (None, Some(120), Some(100))
        );
        assert!(c.validate().is_ok());
    }

    #[test]
    fn short_keeps_first_sentence() {
        assert_eq!(
            short("Unauthorized: X. Long explanation. More."),
            "Unauthorized: X."
        );
        assert_eq!(short("no period here"), "no period here");
        assert!(short(&"a".repeat(500)).chars().count() <= 201);
    }
}
