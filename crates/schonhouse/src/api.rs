//! HTTP layer: REST API, Server-Sent Events stream and the embedded web UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::{DeviceConfig, DeviceKind, valid_id, valid_name};
use crate::devices::{
    ApplyResult, Capability, CommandError, DeviceSnapshot, LightCommand, LogEntry, Registry,
    ServerEvent,
};
use crate::discovery;
use crate::store::{DeviceLayout, Group, Preset, Room, Scene, SceneEntry, Store, UsageSample};
use crate::usage;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

pub struct AppState {
    pub registry: Arc<Registry>,
    pub store: Store,
    pub started: Instant,
    pub started_at: u64,
    pub config_path: String,
    pub bind: String,
    pub poll_interval_secs: u64,
    pub discovery_broadcast: String,
    pub usage_sample_interval_mins: u64,
}

pub fn router(state: Arc<AppState>) -> Router {
    // The UI is a single-origin app: scripts and styles only from this server, no
    // framing, no inline script. Styles set through the CSSOM from app.js are allowed
    // under this policy; inline style attributes are not used. The service worker and
    // the web manifest fall under script-src / default-src 'self'.
    let csp = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
               connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/info", get(server_info))
        .route("/api/log", get(recent_log))
        .route("/api/discover", get(discover))
        .route("/api/devices", get(list_devices).post(create_device))
        .route("/api/devices/state", post(set_all_state))
        .route("/api/devices/toggle", post(toggle_all))
        .route(
            "/api/devices/{id}",
            get(get_device).patch(patch_device).delete(delete_device),
        )
        .route("/api/devices/{id}/state", post(set_device_state))
        .route("/api/devices/{id}/toggle", post(toggle_device))
        .route("/api/devices/{id}/refresh", post(refresh_device))
        .route("/api/devices/{id}/raw", get(raw_device))
        .route("/api/devices/{id}/usage", get(device_usage))
        .route("/api/devices/{id}/reboot", post(reboot_device))
        .route("/api/rooms", get(list_rooms).post(create_room))
        .route("/api/rooms/{id}", put(update_room).delete(delete_room))
        .route("/api/layout", get(get_layout).put(set_layout))
        .route("/api/groups", get(list_groups).post(create_group))
        .route("/api/groups/{id}", put(update_group).delete(delete_group))
        .route("/api/groups/{id}/state", post(set_group_state))
        .route("/api/groups/{id}/toggle", post(toggle_group))
        .route("/api/scenes", get(list_scenes).post(create_scene))
        .route("/api/scenes/capture", post(capture_scene))
        .route("/api/scenes/{id}", put(update_scene).delete(delete_scene))
        .route("/api/scenes/{id}/apply", post(apply_scene))
        .route("/api/presets", get(list_presets).post(create_preset))
        .route(
            "/api/presets/{id}",
            put(update_preset).delete(delete_preset),
        )
        .route("/api/usage", get(list_usage))
        .route("/api/usage/latest", get(latest_usage))
        .route("/api/usage/sample", post(sample_usage))
        .route("/api/events", get(events))
        .route("/{*path}", get(asset))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(csp),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .with_state(state)
}

// ---------------------------------------------------------------- errors

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// Store errors: constraint messages become client errors, anything else is a 500.
    fn store(err: anyhow::Error) -> Self {
        let message = err.to_string();
        let status = if message.contains("already exists") {
            StatusCode::CONFLICT
        } else if message.contains("unknown") || message.contains("does not exist") {
            StatusCode::BAD_REQUEST
        } else {
            tracing::error!("store error: {err:#}");
            return Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "storage error".into(),
            };
        };
        Self { status, message }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<CommandError> for ApiError {
    fn from(err: CommandError) -> Self {
        let status = match err {
            CommandError::NotFound => StatusCode::NOT_FOUND,
            CommandError::Invalid(_) => StatusCode::BAD_REQUEST,
            CommandError::Device(_) => StatusCode::BAD_GATEWAY,
            CommandError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        ApiError {
            status,
            message: err.message(),
        }
    }
}

// ---------------------------------------------------------------- server

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

#[derive(Serialize)]
struct ServerInfo {
    version: &'static str,
    started_at: u64,
    uptime_secs: u64,
    os: &'static str,
    arch: &'static str,
    config_path: String,
    bind: String,
    poll_interval_secs: u64,
    discovery_broadcast: String,
    usage_sample_interval_mins: u64,
    device_count: usize,
    debug_build: bool,
}

async fn server_info(State(app): State<Arc<AppState>>) -> Json<ServerInfo> {
    Json(ServerInfo {
        version: env!("CARGO_PKG_VERSION"),
        started_at: app.started_at,
        uptime_secs: app.started.elapsed().as_secs(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        config_path: app.config_path.clone(),
        bind: app.bind.clone(),
        poll_interval_secs: app.poll_interval_secs,
        discovery_broadcast: app.discovery_broadcast.clone(),
        usage_sample_interval_mins: app.usage_sample_interval_mins,
        device_count: app.registry.snapshots().await.len(),
        debug_build: cfg!(debug_assertions),
    })
}

async fn recent_log(State(app): State<Arc<AppState>>) -> Json<Vec<LogEntry>> {
    Json(app.registry.recent_log())
}

#[derive(Deserialize)]
struct DiscoverParams {
    #[serde(default = "default_discover_timeout")]
    timeout: u64,
}

fn default_discover_timeout() -> u64 {
    5
}

async fn discover(
    State(app): State<Arc<AppState>>,
    Query(params): Query<DiscoverParams>,
) -> Result<Json<Vec<discovery::Discovered>>, ApiError> {
    if !(1..=30).contains(&params.timeout) {
        return Err(ApiError::bad_request("timeout must be between 1 and 30"));
    }
    let mut found = discovery::scan(&app.discovery_broadcast, params.timeout)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: e,
        })?;
    let known = app.registry.snapshots().await;
    for item in &mut found {
        item.known_as = known.iter().find(|d| d.ip == item.ip).map(|d| d.id.clone());
    }
    Ok(Json(found))
}

// ---------------------------------------------------------------- devices

async fn list_devices(State(app): State<Arc<AppState>>) -> Json<Vec<DeviceSnapshot>> {
    Json(app.registry.snapshots().await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDevice {
    id: Option<String>,
    name: Option<String>,
    kind: DeviceKind,
    ip: String,
}

/// Derives a URL-safe id from a name, e.g. "Living room" -> "living-room".
fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

async fn create_device(
    State(app): State<Arc<AppState>>,
    Json(body): Json<NewDevice>,
) -> Result<(StatusCode, Json<DeviceSnapshot>), ApiError> {
    let name = body
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    let id = body
        .id
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty())
        .or_else(|| name.as_deref().map(slug).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("device-{}", body.ip.trim().replace(['.', ':'], "-")));
    if !valid_id(&id) {
        return Err(ApiError::bad_request(
            "id must be 1-64 letters, digits, '-' or '_'",
        ));
    }
    let device = DeviceConfig {
        id,
        name,
        kind: body.kind,
        ip: body.ip.trim().to_string(),
    };
    device
        .validate()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    app.store
        .insert_device(device.clone())
        .await
        .map_err(ApiError::store)?;
    match app.registry.add(device.clone()).await {
        Ok(snapshot) => {
            app.registry.notify_changed("layout");
            Ok((StatusCode::CREATED, Json(snapshot)))
        }
        Err(err) => {
            let _ = app.store.delete_device(device.id).await;
            Err(err.into())
        }
    }
}

async fn get_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<DeviceSnapshot>, ApiError> {
    app.registry
        .snapshot(&id)
        .await
        .map(Json)
        .ok_or_else(|| CommandError::NotFound.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DevicePatch {
    /// `null` or empty clears the override and shows the device's own name again.
    name: Option<String>,
}

async fn patch_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<DevicePatch>,
) -> Result<Json<DeviceSnapshot>, ApiError> {
    let name = body
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if let Some(name) = &name
        && !valid_name(name)
    {
        return Err(ApiError::bad_request("invalid name"));
    }
    if !app.registry.contains(&id).await {
        return Err(CommandError::NotFound.into());
    }
    app.store
        .update_device_name(id.clone(), name.clone())
        .await
        .map_err(ApiError::store)?;
    Ok(Json(app.registry.rename(&id, name).await?))
}

async fn delete_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let in_store = app
        .store
        .delete_device(id.clone())
        .await
        .map_err(ApiError::store)?;
    let in_registry = app.registry.remove(&id).await;
    if in_store || in_registry {
        for what in ["groups", "scenes", "layout"] {
            app.registry.notify_changed(what);
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(CommandError::NotFound.into())
    }
}

async fn set_device_state(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(command): Json<LightCommand>,
) -> Result<Json<DeviceSnapshot>, ApiError> {
    Ok(Json(app.registry.command(&id, command).await?))
}

#[derive(Serialize)]
struct BatchResponse {
    results: Vec<ApplyResult>,
}

async fn set_all_state(
    State(app): State<Arc<AppState>>,
    Json(command): Json<LightCommand>,
) -> Result<Json<BatchResponse>, ApiError> {
    command.validate().map_err(ApiError::bad_request)?;
    let items = app
        .registry
        .snapshots()
        .await
        .into_iter()
        .map(|d| (d.id, command.clone()))
        .collect();
    Ok(Json(BatchResponse {
        results: app.registry.apply_many(items).await,
    }))
}

/// The command a wall-switch style toggle sends to a set of devices: off when any of
/// them is on, otherwise on. Devices without a known state (offline) count as off, so
/// a set that is entirely unreachable is asked to turn on and reports the failures.
fn toggle_command<'a>(devices: impl IntoIterator<Item = &'a DeviceSnapshot>) -> LightCommand {
    let any_on = devices
        .into_iter()
        .any(|d| d.state.as_ref().is_some_and(|s| s.on));
    LightCommand {
        on: Some(!any_on),
        ..LightCommand::default()
    }
}

/// Turns every device off when any is on, else every device on. Takes no body, so a
/// physical button only needs to fire one request without knowing the current state.
async fn toggle_all(State(app): State<Arc<AppState>>) -> Result<Json<BatchResponse>, ApiError> {
    let devices = app.registry.snapshots().await;
    let command = toggle_command(&devices);
    let items = devices
        .into_iter()
        .map(|d| (d.id, command.clone()))
        .collect();
    Ok(Json(BatchResponse {
        results: app.registry.apply_many(items).await,
    }))
}

/// Flips one device between on and off; returns the new snapshot.
async fn toggle_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<DeviceSnapshot>, ApiError> {
    let snapshot = app
        .registry
        .snapshot(&id)
        .await
        .ok_or(CommandError::NotFound)?;
    let command = toggle_command([&snapshot]);
    Ok(Json(app.registry.command(&id, command).await?))
}

async fn refresh_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<DeviceSnapshot>, ApiError> {
    Ok(Json(app.registry.refresh(&id).await?))
}

async fn raw_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(app.registry.raw(&id).await?))
}

async fn device_usage(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(app.registry.usage(&id).await?))
}

async fn reboot_device(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.registry.reboot(&id).await?;
    Ok(StatusCode::ACCEPTED)
}

// ---------------------------------------------------------------- rooms and layout

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoomBody {
    name: String,
    #[serde(default)]
    position: Option<i64>,
}

async fn list_rooms(State(app): State<Arc<AppState>>) -> Result<Json<Vec<Room>>, ApiError> {
    app.store
        .list_rooms()
        .await
        .map(Json)
        .map_err(ApiError::store)
}

async fn create_room(
    State(app): State<Arc<AppState>>,
    Json(body): Json<RoomBody>,
) -> Result<(StatusCode, Json<Room>), ApiError> {
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("invalid room name"));
    }
    let room = app
        .store
        .save_room(Room {
            id: 0,
            name: body.name.trim().to_string(),
            position: body.position.unwrap_or(0),
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("layout");
    Ok((StatusCode::CREATED, Json(room)))
}

async fn update_room(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<RoomBody>,
) -> Result<Json<Room>, ApiError> {
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("invalid room name"));
    }
    let current = app
        .store
        .list_rooms()
        .await
        .map_err(ApiError::store)?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| ApiError::not_found("unknown room"))?;
    let room = app
        .store
        .save_room(Room {
            id,
            name: body.name.trim().to_string(),
            position: body.position.unwrap_or(current.position),
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("layout");
    Ok(Json(room))
}

async fn delete_room(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if app.store.delete_room(id).await.map_err(ApiError::store)? {
        app.registry.notify_changed("layout");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("unknown room"))
    }
}

#[derive(Serialize)]
struct Layout {
    rooms: Vec<Room>,
    devices: Vec<DeviceLayout>,
}

async fn get_layout(State(app): State<Arc<AppState>>) -> Result<Json<Layout>, ApiError> {
    Ok(Json(Layout {
        rooms: app.store.list_rooms().await.map_err(ApiError::store)?,
        devices: app.store.list_layout().await.map_err(ApiError::store)?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutBody {
    devices: Vec<DeviceLayout>,
}

/// Bulk update of room and position, used after reordering in the UI.
async fn set_layout(
    State(app): State<Arc<AppState>>,
    Json(body): Json<LayoutBody>,
) -> Result<StatusCode, ApiError> {
    if body.devices.iter().any(|d| !valid_id(&d.id)) {
        return Err(ApiError::bad_request("invalid device id"));
    }
    app.store
        .set_layout(body.devices)
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("layout");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- groups

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupBody {
    name: String,
    devices: Vec<String>,
}

fn validate_group(body: &GroupBody) -> Result<(), ApiError> {
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("invalid group name"));
    }
    if body.devices.is_empty() {
        return Err(ApiError::bad_request("a group needs at least one device"));
    }
    if body.devices.iter().any(|id| !valid_id(id)) {
        return Err(ApiError::bad_request("invalid device id"));
    }
    Ok(())
}

/// A stored group plus the capabilities every member shares, so the UI can show the
/// controls that make sense for the whole group.
#[derive(Serialize)]
struct GroupView {
    #[serde(flatten)]
    group: Group,
    capabilities: Vec<Capability>,
}

/// Intersection of capability lists, in the order of the first list. Empty when there
/// are no lists.
fn common_capabilities<'a>(sets: impl IntoIterator<Item = &'a [Capability]>) -> Vec<Capability> {
    let mut common: Option<Vec<Capability>> = None;
    for set in sets {
        common = Some(match common {
            None => set.to_vec(),
            Some(current) => current.into_iter().filter(|c| set.contains(c)).collect(),
        });
    }
    common.unwrap_or_default()
}

/// Members that are not (or no longer) registered are ignored for the intersection.
fn group_view(group: Group, devices: &[DeviceSnapshot]) -> GroupView {
    let capabilities = common_capabilities(
        group
            .devices
            .iter()
            .filter_map(|id| devices.iter().find(|d| &d.id == id))
            .map(|d| d.capabilities.as_slice()),
    );
    GroupView {
        group,
        capabilities,
    }
}

async fn list_groups(State(app): State<Arc<AppState>>) -> Result<Json<Vec<GroupView>>, ApiError> {
    let groups = app.store.list_groups().await.map_err(ApiError::store)?;
    let devices = app.registry.snapshots().await;
    Ok(Json(
        groups
            .into_iter()
            .map(|g| group_view(g, &devices))
            .collect(),
    ))
}

async fn create_group(
    State(app): State<Arc<AppState>>,
    Json(body): Json<GroupBody>,
) -> Result<(StatusCode, Json<GroupView>), ApiError> {
    validate_group(&body)?;
    let group = app
        .store
        .save_group(Group {
            id: 0,
            name: body.name.trim().to_string(),
            devices: body.devices,
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("groups");
    let devices = app.registry.snapshots().await;
    Ok((StatusCode::CREATED, Json(group_view(group, &devices))))
}

async fn update_group(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<GroupBody>,
) -> Result<Json<GroupView>, ApiError> {
    validate_group(&body)?;
    let group = app
        .store
        .save_group(Group {
            id,
            name: body.name.trim().to_string(),
            devices: body.devices,
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("groups");
    let devices = app.registry.snapshots().await;
    Ok(Json(group_view(group, &devices)))
}

async fn delete_group(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if app.store.delete_group(id).await.map_err(ApiError::store)? {
        app.registry.notify_changed("groups");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("unknown group"))
    }
}

async fn set_group_state(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(command): Json<LightCommand>,
) -> Result<Json<BatchResponse>, ApiError> {
    command.validate().map_err(ApiError::bad_request)?;
    let group = app
        .store
        .get_group(id)
        .await
        .map_err(ApiError::store)?
        .ok_or_else(|| ApiError::not_found("unknown group"))?;
    let items = group
        .devices
        .into_iter()
        .map(|d| (d, command.clone()))
        .collect();
    Ok(Json(BatchResponse {
        results: app.registry.apply_many(items).await,
    }))
}

/// Like `toggle_all`, over the members of a group.
async fn toggle_group(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<BatchResponse>, ApiError> {
    let group = app
        .store
        .get_group(id)
        .await
        .map_err(ApiError::store)?
        .ok_or_else(|| ApiError::not_found("unknown group"))?;
    let devices = app.registry.snapshots().await;
    let command = toggle_command(devices.iter().filter(|d| group.devices.contains(&d.id)));
    let items = group
        .devices
        .into_iter()
        .map(|d| (d, command.clone()))
        .collect();
    Ok(Json(BatchResponse {
        results: app.registry.apply_many(items).await,
    }))
}

// ---------------------------------------------------------------- scenes

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneBody {
    name: String,
    entries: Vec<SceneEntry>,
}

fn validate_scene(body: &SceneBody) -> Result<(), ApiError> {
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("invalid scene name"));
    }
    if body.entries.is_empty() {
        return Err(ApiError::bad_request("a scene needs at least one entry"));
    }
    for entry in &body.entries {
        if !valid_id(&entry.device_id) {
            return Err(ApiError::bad_request("invalid device id"));
        }
        entry
            .command
            .validate()
            .map_err(|e| ApiError::bad_request(format!("{}: {e}", entry.device_id)))?;
    }
    Ok(())
}

async fn list_scenes(State(app): State<Arc<AppState>>) -> Result<Json<Vec<Scene>>, ApiError> {
    app.store
        .list_scenes()
        .await
        .map(Json)
        .map_err(ApiError::store)
}

async fn create_scene(
    State(app): State<Arc<AppState>>,
    Json(body): Json<SceneBody>,
) -> Result<(StatusCode, Json<Scene>), ApiError> {
    validate_scene(&body)?;
    let scene = app
        .store
        .save_scene(Scene {
            id: 0,
            name: body.name.trim().to_string(),
            entries: body.entries,
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("scenes");
    Ok((StatusCode::CREATED, Json(scene)))
}

async fn update_scene(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<SceneBody>,
) -> Result<Json<Scene>, ApiError> {
    validate_scene(&body)?;
    let scene = app
        .store
        .save_scene(Scene {
            id,
            name: body.name.trim().to_string(),
            entries: body.entries,
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("scenes");
    Ok(Json(scene))
}

async fn delete_scene(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if app.store.delete_scene(id).await.map_err(ApiError::store)? {
        app.registry.notify_changed("scenes");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("unknown scene"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBody {
    name: String,
    /// Device ids to include; all devices with a known state when omitted.
    #[serde(default)]
    devices: Option<Vec<String>>,
}

/// Saves the current state of the devices as a new scene.
async fn capture_scene(
    State(app): State<Arc<AppState>>,
    Json(body): Json<CaptureBody>,
) -> Result<(StatusCode, Json<Scene>), ApiError> {
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("invalid scene name"));
    }
    let entries: Vec<SceneEntry> = app
        .registry
        .snapshots()
        .await
        .into_iter()
        .filter(|d| body.devices.as_ref().is_none_or(|ids| ids.contains(&d.id)))
        .filter_map(|d| {
            d.state.map(|state| SceneEntry {
                device_id: d.id,
                command: state.to_command(),
            })
        })
        .collect();
    if entries.is_empty() {
        return Err(ApiError::bad_request(
            "no device with a known state to capture",
        ));
    }
    let scene = app
        .store
        .save_scene(Scene {
            id: 0,
            name: body.name.trim().to_string(),
            entries,
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("scenes");
    Ok((StatusCode::CREATED, Json(scene)))
}

async fn apply_scene(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<BatchResponse>, ApiError> {
    let scene = app
        .store
        .get_scene(id)
        .await
        .map_err(ApiError::store)?
        .ok_or_else(|| ApiError::not_found("unknown scene"))?;
    let items = scene
        .entries
        .into_iter()
        .map(|e| (e.device_id, e.command))
        .collect();
    Ok(Json(BatchResponse {
        results: app.registry.apply_many(items).await,
    }))
}

// ---------------------------------------------------------------- presets

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetBody {
    name: String,
    command: LightCommand,
    #[serde(default)]
    position: Option<i64>,
}

fn validate_preset(body: &PresetBody) -> Result<(), ApiError> {
    if !valid_name(&body.name) {
        return Err(ApiError::bad_request("invalid preset name"));
    }
    body.command.validate().map_err(ApiError::bad_request)
}

async fn list_presets(State(app): State<Arc<AppState>>) -> Result<Json<Vec<Preset>>, ApiError> {
    app.store
        .list_presets()
        .await
        .map(Json)
        .map_err(ApiError::store)
}

async fn create_preset(
    State(app): State<Arc<AppState>>,
    Json(body): Json<PresetBody>,
) -> Result<(StatusCode, Json<Preset>), ApiError> {
    validate_preset(&body)?;
    let preset = app
        .store
        .save_preset(Preset {
            id: 0,
            name: body.name.trim().to_string(),
            command: body.command,
            position: body.position.unwrap_or(0),
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("presets");
    Ok((StatusCode::CREATED, Json(preset)))
}

async fn update_preset(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<PresetBody>,
) -> Result<Json<Preset>, ApiError> {
    validate_preset(&body)?;
    let current = app
        .store
        .list_presets()
        .await
        .map_err(ApiError::store)?
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| ApiError::not_found("unknown preset"))?;
    let preset = app
        .store
        .save_preset(Preset {
            id,
            name: body.name.trim().to_string(),
            command: body.command,
            position: body.position.unwrap_or(current.position),
        })
        .await
        .map_err(ApiError::store)?;
    app.registry.notify_changed("presets");
    Ok(Json(preset))
}

async fn delete_preset(
    State(app): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    if app.store.delete_preset(id).await.map_err(ApiError::store)? {
        app.registry.notify_changed("presets");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("unknown preset"))
    }
}

// ---------------------------------------------------------------- usage

#[derive(Deserialize)]
struct UsageParams {
    device: Option<String>,
    /// Unix seconds; defaults to 30 days ago.
    since: Option<i64>,
}

async fn list_usage(
    State(app): State<Arc<AppState>>,
    Query(params): Query<UsageParams>,
) -> Result<Json<Vec<UsageSample>>, ApiError> {
    if let Some(device) = &params.device
        && !valid_id(device)
    {
        return Err(ApiError::bad_request("invalid device id"));
    }
    let since = params
        .since
        .unwrap_or_else(|| app.started_at as i64 - 30 * 24 * 3600);
    app.store
        .list_usage(params.device, since)
        .await
        .map(Json)
        .map_err(ApiError::store)
}

async fn latest_usage(
    State(app): State<Arc<AppState>>,
) -> Result<Json<Vec<UsageSample>>, ApiError> {
    app.store
        .latest_usage()
        .await
        .map(Json)
        .map_err(ApiError::store)
}

/// Reads the counters of every online device right now.
async fn sample_usage(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let sampled = usage::sample_all(&app.registry, &app.store).await;
    app.registry.notify_changed("usage");
    Json(json!({ "sampled": sampled }))
}

// ---------------------------------------------------------------- events and assets

/// Streams every device snapshot once on connect, then every change and log entry.
async fn events(
    State(app): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let initial = app
        .registry
        .snapshots()
        .await
        .into_iter()
        .map(|s| ServerEvent::Device(Box::new(s)));
    let live = BroadcastStream::new(app.registry.subscribe()).filter_map(|item| item.ok());
    let stream = tokio_stream::iter(initial)
        .chain(live)
        .map(|event| Ok(to_sse(&event)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn to_sse(event: &ServerEvent) -> Event {
    let (name, result) = match event {
        ServerEvent::Device(snapshot) => ("device", Event::default().json_data(snapshot)),
        ServerEvent::DeviceRemoved(id) => (
            "device-removed",
            Event::default().json_data(json!({ "id": id })),
        ),
        ServerEvent::Changed(what) => (
            "changed",
            Event::default().json_data(json!({ "what": what })),
        ),
        ServerEvent::Log(entry) => ("log", Event::default().json_data(entry)),
    };
    result
        .map(|e| e.event(name))
        .unwrap_or_else(|_| Event::default().comment("serialization error"))
}

async fn index() -> Response {
    serve_asset("index.html")
}

async fn asset(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

fn serve_asset(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let mut mime = file.metadata.mimetype().to_string();
            if path.ends_with(".webmanifest") {
                mime = "application/manifest+json".to_string();
            }
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_intersection() {
        use Capability::*;
        let color: &[Capability] = &[Power, Brightness, ColorTemperature, Color];
        let white: &[Capability] = &[Power, Brightness, ColorTemperature];
        let plug: &[Capability] = &[Power];
        assert_eq!(common_capabilities([color, color]), color.to_vec());
        assert_eq!(common_capabilities([color, white]), white.to_vec());
        assert_eq!(common_capabilities([white, color, plug]), vec![Power]);
        assert!(common_capabilities(Vec::<&[Capability]>::new()).is_empty());
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Living room"), "living-room");
        assert_eq!(slug("  Desk  Lamp #2 "), "desk-lamp-2");
        assert_eq!(slug("***"), "");
        assert_eq!(slug("Árbol"), "rbol");
    }

    fn snapshot(id: &str, on: Option<bool>) -> DeviceSnapshot {
        DeviceSnapshot {
            id: id.into(),
            name: id.into(),
            custom_name: None,
            kind: DeviceKind::TapoL535,
            capabilities: Vec::new(),
            ip: "192.168.0.2".into(),
            online: on.is_some(),
            state: on.map(|on| crate::devices::LightState {
                on,
                brightness: 50,
                color_temp: 2700,
                hue: None,
                saturation: None,
            }),
            info: None,
            error: None,
            updated_at: 0,
            last_seen: None,
        }
    }

    #[test]
    fn toggle_turns_off_when_any_is_on_else_on() {
        let on = |cmd: LightCommand| cmd.on.expect("toggle always sets on");
        assert!(!on(toggle_command([
            &snapshot("a", Some(true)),
            &snapshot("b", Some(false))
        ])));
        assert!(!on(toggle_command([
            &snapshot("a", Some(true)),
            &snapshot("b", None)
        ])));
        assert!(on(toggle_command([
            &snapshot("a", Some(false)),
            &snapshot("b", Some(false))
        ])));
        assert!(on(toggle_command([&snapshot("a", None)])));
        assert!(on(toggle_command(std::iter::empty::<&DeviceSnapshot>())));
        let cmd = toggle_command([&snapshot("a", Some(true))]);
        assert_eq!(
            cmd,
            LightCommand {
                on: Some(false),
                ..LightCommand::default()
            }
        );
    }
}
