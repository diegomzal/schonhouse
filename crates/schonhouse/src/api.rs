//! HTTP layer: REST API, Server-Sent Events stream and the embedded web UI.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rust_embed::RustEmbed;
use serde::Serialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::devices::{
    CommandError, DeviceSnapshot, LightCommand, LogEntry, Registry, ServerEvent,
};

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

pub struct AppState {
    pub registry: Arc<Registry>,
    pub started: Instant,
    pub started_at: u64,
    pub config_path: String,
    pub bind: String,
    pub poll_interval_secs: u64,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/info", get(server_info))
        .route("/api/log", get(recent_log))
        .route("/api/devices", get(list_devices))
        .route("/api/devices/{id}", get(get_device))
        .route("/api/devices/{id}/state", post(set_device_state))
        .route("/api/devices/{id}/refresh", post(refresh_device))
        .route("/api/devices/{id}/raw", get(raw_device))
        .route("/api/devices/{id}/usage", get(device_usage))
        .route("/api/devices/{id}/reboot", post(reboot_device))
        .route("/api/events", get(events))
        .route("/{*path}", get(asset))
        .with_state(state)
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<CommandError> for ApiError {
    fn from(err: CommandError) -> Self {
        match err {
            CommandError::NotFound => ApiError {
                status: StatusCode::NOT_FOUND,
                message: "unknown device".into(),
            },
            CommandError::Invalid(message) => ApiError {
                status: StatusCode::BAD_REQUEST,
                message,
            },
            CommandError::Device(message) => ApiError {
                status: StatusCode::BAD_GATEWAY,
                message,
            },
            CommandError::Unavailable => ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "device worker is not running".into(),
            },
        }
    }
}

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
        device_count: app.registry.snapshots().await.len(),
        debug_build: cfg!(debug_assertions),
    })
}

async fn recent_log(State(app): State<Arc<AppState>>) -> Json<Vec<LogEntry>> {
    Json(app.registry.recent_log())
}

async fn list_devices(State(app): State<Arc<AppState>>) -> Json<Vec<DeviceSnapshot>> {
    Json(app.registry.snapshots().await)
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

async fn set_device_state(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(command): Json<LightCommand>,
) -> Result<Json<DeviceSnapshot>, ApiError> {
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

/// Streams every device snapshot once on connect, then every change and log entry.
async fn events(
    State(app): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let initial = app
        .registry
        .snapshots()
        .await
        .into_iter()
        .map(ServerEvent::Device);
    let live = BroadcastStream::new(app.registry.subscribe()).filter_map(|item| item.ok());
    let stream = tokio_stream::iter(initial)
        .chain(live)
        .map(|event| Ok(to_sse(&event)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn to_sse(event: &ServerEvent) -> Event {
    let (name, result) = match event {
        ServerEvent::Device(snapshot) => ("device", Event::default().json_data(snapshot)),
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
        Some(file) => (
            [
                (header::CONTENT_TYPE, file.metadata.mimetype().to_string()),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            file.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
