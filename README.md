# Schonhouse

[![CI](https://github.com/diegomzal/schonhouse/actions/workflows/ci.yml/badge.svg)](https://github.com/diegomzal/schonhouse/actions/workflows/ci.yml)

A self-built, lightweight home assistant designed to run on a Raspberry Pi 5 and be
controlled from a web UI on the local network. One Rust binary, an embedded web UI, a
SQLite file. First supported devices: TP-Link Tapo L535 color bulbs.

## Features

- Local control of Tapo bulbs over the KLAP protocol, no cloud round-trips.
- Live web UI (Server-Sent Events): power, brightness, white temperature with presets,
  color with swatches, typed values, device details and diagnostics.
- Groups (apply one command to several devices) and scenes (saved states, captured
  from the current state with one click, editable per device).
- Rooms with manual ordering, and your own color/white presets.
- Energy tab: time on and energy per device and in total, with a 30-day daily chart.
- Installable as an app on phones (PWA) with an offline-capable shell.
- LAN discovery: scan for Tapo devices and add them from the UI; no config editing.
- Embedded SQLite storage for devices, rooms, groups, scenes, presets and usage
  samples, with automatic schema upgrades.
- Event log, server info and raw JSON views for debugging.
- Small footprint: release profile with LTO, single static binary, no runtime.

## Roadmap

1. **Stack decision** – done, see [docs/adr/0001-stack-decision.md](docs/adr/0001-stack-decision.md).
2. **Proof of concept** – done: `crates/poc` (Rust) and `poc/poc.py` (Python).
3. **Service** – in progress: `crates/schonhouse`. See [docs/architecture.md](docs/architecture.md).
4. **Deployment** – single binary on the Pi 5 under `systemd`, see
   [docs/deployment.md](docs/deployment.md). Waiting for the hardware.
5. Next: schedules and automations, more device kinds.

## Running the service

Prerequisites: Rust 1.88+ (install with [rustup](https://rustup.rs)) plus a C toolchain
and CMake, because the `tapo` crate's TLS stack (`rustls` + `aws-lc-rs`) compiles a C
library:

- Windows: Visual Studio 2022 Build Tools with the "Desktop development with C++"
  workload, CMake, and NASM (all installable with `winget`). If Smart App Control is
  on, Cargo fails with "An Application Control policy has blocked this file"; it has to
  be turned off or the build has to run inside WSL.
- Raspberry Pi OS / Debian / Ubuntu: `sudo apt install build-essential cmake`.

Then:

1. Copy `.env.example` to `.env` and fill in `TAPO_USERNAME` and `TAPO_PASSWORD` (the
   Tapo app account). They are only used for the local handshake with the bulbs.
2. Optionally edit [schonhouse.toml](schonhouse.toml) (bind address, poll interval,
   database path). Devices can be listed there as a seed, but the normal way is to add
   them from the UI.
3. Run:

```bash
cargo run -p schonhouse
```

Open `http://<this-machine-ip>:8080` from any device on the LAN, press
"Add device…" → "Scan", and add the bulbs that answer. The database `schonhouse.db` is
created next to the config file.

Logging level is controlled with `RUST_LOG` (default `info`). `--config path.toml` or
`SCHONHOUSE_CONFIG` point at another config file.

## Security

Read [docs/security.md](docs/security.md) before exposing the service anywhere. Short
version: it is designed for a trusted LAN, it has no authentication, and it must not be
reachable from the internet. Credentials never go into the repository, the config or
the database.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same three commands plus release builds for `x86_64` and `aarch64` Linux.

### Proofs of concept

`crates/poc` is a Rust command-line check of the bulbs (`cargo run -p poc -- status`,
`on`, `off`, `toggle`, `brightness 40`, `temp 2700`, `color 120 100`, `--ip` to target
one bulb, `--json` for raw info). `poc/poc.py` is the same in Python with the `tapo`
package, kept as a zero-toolchain protocol check:

```bash
python -m venv .venv
```

```bash
.venv\Scripts\pip install -r poc\requirements.txt
```

```bash
.venv\Scripts\python poc\poc.py status
```

## Repository layout

```
Cargo.toml          Cargo workspace (release profile tuned for small binaries)
schonhouse.toml     Service configuration (server, storage, optional device seed)
crates/schonhouse/  The service: config, store, device actors, discovery, API, web UI
crates/poc/         Rust proof of concept
deploy/             systemd unit for the Pi
docs/               Architecture, security, deployment notes and decision records
poc/                Python proof of concept
.github/workflows/  CI: fmt, clippy, tests, Linux release builds
```

## License

MIT, see [LICENSE](LICENSE).
