# ADR 0001: Technology stack

- Status: accepted
- Date: 2026-11-17

## Context

Schonhouse is a self-built home assistant that must:

- Run on a Raspberry Pi 5 with the smallest possible footprint (RAM, CPU, startup time).
- Control TP-Link Tapo devices locally (no cloud round-trips). First targets: two Tapo
  L535 color bulbs at `192.168.0.2` and `192.168.0.3`.
- Expose a web UI reachable from the local network.
- Be easy to extend later (more device types, automations, schedules).

Constraints observed on the development machine (Windows 11): only Python 3.15 is
installed. No Rust, Go, Node, Docker or Git toolchains are present yet.

### Protocol facts

Tapo devices on current firmware use the **KLAP** protocol (older firmware used the
"passthrough" protocol). Authentication is local, but the handshake is derived from the
Tapo cloud account email and password. No traffic goes to TP-Link servers once the
session is established. Any library we choose must support KLAP.

## Options considered

### 1. Rust: `tapo` crate + `axum` + embedded static UI (chosen)

- `tapo` (mihai-dinculescu/tapo) is the most complete and actively maintained Tapo
  library. It supports KLAP and passthrough, and lists the L535 explicitly
  (on/off, brightness, color temperature, hue/saturation, device info; no light
  effects on this model). Its Python bindings (`tapo` on PyPI) wrap the same core.
- `axum` + `tokio` give an async HTTP server with Server-Sent Events for live state.
- Single static binary, no runtime to install on the Pi, roughly 5 to 15 MB RSS and
  sub-second startup. Memory figures are estimates and will be measured on the Pi.
- Frontend: plain HTML/CSS/JS embedded in the binary (`rust-embed`), no Node build step.
- Persistence: SQLite through `rusqlite` (bundled) when automations/history are needed.
- Costs: compile times, a toolchain to install (rustup + MSVC Build Tools on Windows),
  and either building on the Pi itself or cross-compiling (`cargo-zigbuild`).

### 2. Go

- Trivial cross-compilation from Windows to `linux/arm64`, static binary, ~10-20 MB RSS.
- Tapo libraries in Go exist but are less mature than the Rust crate; KLAP support and
  device coverage vary by project. Higher risk of protocol breakage after firmware
  updates.

### 3. Python: `tapo` (PyPI) or `python-kasa` + a small ASGI server

- Fastest to develop; `tapo` on PyPI has prebuilt wheels for CPython 3.15 on Windows
  and for `manylinux aarch64` (Pi 5). `python-kasa` also supports the L535 (listed as
  L535E).
- Realistic footprint is 40-80 MB RSS plus interpreter startup. Fine for a Pi 5, but
  not "as light as possible".

### 4. Node.js

- Larger runtime, weaker Tapo library ecosystem for KLAP. Not considered further.

## Decision

- **Production service: Rust** (`tapo` crate, `axum`, `tokio`, embedded vanilla JS UI,
  SQLite later). This is the lightest option that also has the best Tapo protocol
  support.
- **Proof of concept: Rust** (`crates/poc`), using the same crate and handler
  (`ApiClient::l535(ip)`) the production service will use. A Python version
  (`poc/poc.py`, `tapo` PyPI package, same Rust core) was written first because it ran
  before any toolchain was installed; it is kept as a zero-setup protocol check.

## Consequences

- The Rust toolchain (rustup, stable channel) and the Visual Studio 2022 Build Tools
  with the C++ workload were installed on the Windows development machine on
  2026-11-17.
- Builds for the Pi 5 will initially be done on the Pi itself (`cargo build --release`
  with rustup installed on the Pi); cross-compilation can be added later.
- The Pi deployment will be a single binary managed by `systemd`.
- All documentation and code comments are written in English.
