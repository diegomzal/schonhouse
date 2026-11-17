# Security notes

Schonhouse is meant to run on a trusted home LAN. This page says what the code does to
stay safe, what it deliberately does not do, and how to deploy it responsibly. The
source is public; nothing in it is secret, and nothing in the repository must ever be.

## Secrets

- The only secret is the Tapo account email and password, used for the local KLAP
  handshake with each device. It is read from the environment or a `.env` file and is
  never written to the config file, the database, logs or API responses.
- `.env` is git-ignored. Only `.env.example` with placeholders is committed. The SQLite
  database (`*.db`) is git-ignored too because it contains device ids and addresses.
- On the Pi, keep the credentials in `/etc/schonhouse/env` with mode `600`, owned by
  the service user (see `deploy/schonhouse.service`).

## Network exposure

- The service listens on `0.0.0.0:8080` by default so phones on the LAN can reach it.
  It has **no authentication**: anyone who can reach the port can control the lights,
  add or remove devices, and read device details (MAC, firmware, Wi-Fi SSID).
- Do not expose it to the internet. No port forwarding, no public reverse proxy. If
  remote access is ever needed, put it behind a VPN (WireGuard, Tailscale) or an
  authenticating reverse proxy, and bind to `127.0.0.1` in `schonhouse.toml`.
- Discovery sends a UDP broadcast on port 20002 only when `/api/discover` is called.

## What the HTTP layer enforces

- Content-Security-Policy `default-src 'self'` (no inline scripts, no third-party
  origins), `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer`.
- The UI is built with DOM APIs only (`textContent`, `createElement`); device-supplied
  strings such as nicknames and error messages are never interpolated as HTML.
- Every JSON body uses `deny_unknown_fields` and explicit range validation
  (brightness 1-100, color temperature 2500-6500 K, hue 0-360, saturation 1-100, names
  up to 64 characters without control characters, ids restricted to `[A-Za-z0-9_-]`).
- All SQL goes through parameterized statements; names and ids are never concatenated
  into queries. Foreign keys are enforced, so deleting a device cleans up groups and
  scenes.
- Browsers enforce same-origin for `fetch`, and the server sends no CORS headers, so a
  malicious web page cannot call the API from another origin. Note that this does not
  protect against other programs on the LAN; only network isolation does.
- Device errors are passed through to the UI for debugging. They come from the device
  or the `tapo` crate and contain no credentials.

## Dependencies

- Rust with `#![forbid(unsafe_code)]` is not used because `std::env::set_var` (the
  `.env` loader) is `unsafe` in edition 2024; it is the only `unsafe` block, called at
  startup before any thread exists.
- `Cargo.lock` is committed so builds are reproducible. CI runs `cargo fmt`, `clippy
  -D warnings` and the test suite on every push.
- TLS and crypto come from `rustls` + `aws-lc-rs` via the `tapo` crate.

## Reporting

Open an issue on the repository. There is no bug bounty; this is a hobby project.
