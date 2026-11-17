# Architecture

Schonhouse is a single Rust binary (`crates/schonhouse`) that talks to smart devices on
the LAN and exposes them through an HTTP API and an embedded web UI. There is no
database, no message broker and no frontend build step.

```
 browser / phone ──HTTP──▶ axum router ──▶ Registry ──mpsc──▶ device actor ──KLAP──▶ bulb
        ▲                                    │                     │
        └──────── SSE (/api/events) ◀── broadcast ◀── publish ◀────┘
```

## Modules

| File | Responsibility |
|------|----------------|
| `src/main.rs` | CLI flags, logging, config loading, server start, graceful shutdown |
| `src/config.rs` | `schonhouse.toml` schema and validation, `.env` loading, credentials |
| `src/devices.rs` | Device state types, the `Registry`, and one actor task per device |
| `src/api.rs` | REST routes, SSE stream, embedded static assets |
| `web/` | Vanilla HTML/CSS/JS UI, embedded into the binary with `rust-embed` |

## Runtime model

- **One actor per device.** Each device runs as a Tokio task that owns the `tapo`
  handler (the authenticated KLAP session). It polls the device every
  `poll_interval_secs`, applies commands received through an `mpsc` channel, and
  publishes a `DeviceSnapshot` whenever the state changes.
- **Failure handling.** Any error (handshake, poll, command) marks the device offline,
  drops the session and reconnects with exponential backoff (1 s doubling up to 60 s).
  Commands received while offline fail immediately with `502` instead of queueing.
- **Registry.** Holds the latest snapshot of every device behind an `RwLock`, the
  command senders, and a `broadcast` channel of snapshots that feeds SSE clients.
- **Commands are partial updates.** `LightCommand` has optional `on`, `brightness`,
  `color_temp`, `hue`+`saturation`. Color and brightness are applied before `on/off`,
  so `{ "brightness": 30, "on": false }` ends with the bulb off.

## HTTP API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | Web UI |
| `GET` | `/api/health` | `{ "status": "ok", "version": "..." }` |
| `GET` | `/api/info` | Server info: version, uptime, platform, bind, poll interval |
| `GET` | `/api/log` | Recent event log (bounded, newest last) |
| `GET` | `/api/devices` | All device snapshots |
| `GET` | `/api/devices/{id}` | One snapshot (`404` if unknown) |
| `POST` | `/api/devices/{id}/state` | Apply a `LightCommand`; returns the new snapshot |
| `POST` | `/api/devices/{id}/refresh` | Force a poll; returns the new snapshot |
| `GET` | `/api/devices/{id}/raw` | Live, unfiltered device info JSON from the device |
| `GET` | `/api/devices/{id}/usage` | Live usage counters (time on, energy) |
| `POST` | `/api/devices/{id}/reboot` | Reboot the device (`202`) |
| `GET` | `/api/events` | SSE: every snapshot on connect, then `device` and `log` events |

Snapshot shape:

```json
{
  "id": "light-1", "name": "Living room", "kind": "tapo-l535",
  "capabilities": ["power", "brightness", "color-temperature", "color"],
  "ip": "192.168.0.2", "online": true,
  "state": { "on": true, "brightness": 60, "color_temp": 2700, "hue": null, "saturation": null },
  "info": { "nickname": "Living room", "model": "L535", "device_type": "SMART.TAPOBULB",
            "fw_ver": "...", "hw_ver": "...", "mac": "...", "device_id": "...", "ssid": "...",
            "rssi": -52, "signal_level": 3, "on_time": 1234, "overheated": false, "region": "..." },
  "error": null,
  "updated_at": 1763400000, "last_seen": 1763400000
}
```

`name` is the configured name, else the device nickname, else the id. `capabilities`
tells the UI which controls to show, so other device kinds (plugs, white-only bulbs)
can be added without touching the frontend. `color_temp` is `0` when the bulb is in
color mode. Error responses are `{ "error": "message" }` with `400` (invalid command),
`404` (unknown device), `502` (device rejected or unreachable) or `503` (actor not
running). A body that does not fit the types (for example `"brightness": 500`) is
rejected by the JSON extractor with `422`.

Log entries are `{ "ts", "level": "info|warn|error", "device": "id or null", "message" }`
and record connects, failures, applied commands and reboots. The log is in memory only
(last 300 entries).

## Configuration

`schonhouse.toml` (path overridable with `--config` or `SCHONHOUSE_CONFIG`):

```toml
[server]
bind = "0.0.0.0:8080"
poll_interval_secs = 10

[[devices]]
id = "light-1"          # letters, digits, '-' or '_'; used in URLs
name = "Light 1"
kind = "tapo-l535"
ip = "192.168.0.2"
```

Credentials come only from `TAPO_USERNAME` / `TAPO_PASSWORD` (environment or `.env`),
so the config file can be committed. Unknown keys are rejected to catch typos.

## Web UI

`web/index.html`, `style.css` and `app.js`. The page loads `/api/devices`, `/api/info`
and `/api/log` once, then listens to `/api/events`. Two tabs:

- **Devices**: one card per device with a status dot, power button, and a slider plus
  a typed number input for each supported capability (brightness, white temperature
  with presets, hue, saturation with color swatches). A `Details` panel shows the data
  reported by the device (model, firmware, MAC, Wi-Fi signal, on-time, ...) and offers
  Refresh, Usage, Raw JSON and Reboot.
- **System**: server info, event-stream status, the live event log, and the raw
  snapshot JSON.

Slider input is throttled to one command every 180 ms, and incoming snapshots never
overwrite a control the user is dragging or typing into. Dark and light themes follow
the system preference.

## Footprint

Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
Measured numbers (binary size, RSS on the Pi 5) are recorded in `docs/deployment.md`
once the first deployment is done.

## Planned next

- Deployment: `systemd` unit, build on the Pi, measured footprint.
- Scenes and schedules (sunset, timers); SQLite via `rusqlite` only when history is
  needed.
- More device kinds behind the same `DeviceKind` enum (Tapo plugs, other bulbs).
- Optional device discovery on the LAN.
