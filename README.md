# Schonhouse

A self-built, lightweight home assistant designed to run on a Raspberry Pi 5 and be
controlled from a web UI on the local network. First supported devices: TP-Link Tapo
L535 color bulbs.

## Roadmap

1. **Stack decision** – done, see [docs/adr/0001-stack-decision.md](docs/adr/0001-stack-decision.md).
   Production service in Rust (`tapo` crate + `axum`), proof of concept in Python using
   the `tapo` PyPI package (same Rust core).
2. **Proof of concept** – `crates/poc` (Rust) and `poc/poc.py` (Python) control the two
   bulbs from the command line. The Rust one is the reference; the Python one is kept as
   a quick protocol check that needs no toolchain.
3. **Real implementation** – in progress. `crates/schonhouse` is the service: REST API,
   Server-Sent Events and an embedded web UI. See [docs/architecture.md](docs/architecture.md).
4. **Deployment** – single binary on the Pi 5 under `systemd` (next).

## Running the service

Needs the same prerequisites as the Rust POC below plus `.env` with your credentials.
Devices live in [schonhouse.toml](schonhouse.toml): each one needs an `id`, a `kind`
and an `ip`; the name shown in the UI is the one set in the Tapo app unless you add a
`name` override.

```bash
cargo run -p schonhouse
```

Then open `http://<this-machine-ip>:8080` from any device on the LAN. Logging level is
controlled with `RUST_LOG` (default `info`). Use `--config path.toml` or
`SCHONHOUSE_CONFIG` to point at another config file.

## Prerequisites

- A Tapo account (the email/password you use in the Tapo app). The credentials are only
  used for the local KLAP handshake; nothing is sent to TP-Link servers.
- The bulbs on the same LAN as the machine running the POC.
- Copy `.env.example` to `.env` and fill in `TAPO_USERNAME` and `TAPO_PASSWORD`.

## Rust proof of concept

Requirements: Rust 1.88+ (install with [rustup](https://rustup.rs)) plus a C toolchain
and CMake, because the `tapo` crate's TLS stack (`rustls` + `aws-lc-rs`) compiles a C
library:

- Windows: Visual Studio 2022 Build Tools with the "Desktop development with C++"
  workload, CMake, and NASM (all installable with `winget`).
- Raspberry Pi OS / Debian: `sudo apt install build-essential cmake`.

Windows note: if Smart App Control is on, Cargo fails with "An Application Control
policy has blocked this file" because it rejects every locally compiled, unsigned
executable (build scripts included). It has to be turned off in Windows Security, or the
build has to run inside WSL.

```bash
cargo run -p poc -- status
```

Other commands: `on`, `off`, `toggle`, `brightness 40`, `temp 2700`, `color 120 100`.
Add `--ip 192.168.0.2` to target a single bulb and `--json` to dump the raw device info.
`cargo build --release -p poc` produces a standalone binary in `target/release/`.

## Python proof of concept

Requirements: Python 3.11+.

```bash
python -m venv .venv
```

```bash
.venv\Scripts\pip install -r poc\requirements.txt
```

```bash
.venv\Scripts\python poc\poc.py status
```

Same commands and flags as the Rust POC. On Linux/macOS replace `.venv\Scripts\` with
`.venv/bin/`.

## Repository layout

```
Cargo.toml          Cargo workspace (release profile tuned for small binaries)
schonhouse.toml     Service configuration (devices, bind address)
crates/schonhouse/  The service: config, device actors, API, embedded web UI
crates/poc/         Rust proof of concept
docs/               Architecture notes and decision records
poc/                Python proof of concept
```
