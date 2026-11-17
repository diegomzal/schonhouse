# Deployment (Raspberry Pi 5)

The service is one static-ish binary plus a config file, a credentials file and a
SQLite database. The target is Raspberry Pi OS (64-bit, Debian based) on a Pi 5.

## 1. Build the binary

Option A, build on the Pi (simplest, a few minutes the first time):

```bash
sudo apt install -y build-essential cmake git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
git clone https://github.com/diegomzal/schonhouse.git && cd schonhouse
cargo build --release -p schonhouse
```

Option B, cross-compile from a Linux machine or WSL (Ubuntu):

```bash
sudo apt install -y build-essential cmake gcc-aarch64-linux-gnu g++-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
cargo build --release -p schonhouse --target aarch64-unknown-linux-gnu
```

Option C: download the `schonhouse-aarch64-unknown-linux-gnu` artifact produced by the
GitHub Actions workflow for the commit you want.

The binary is `target/<target>/release/schonhouse` (or `target/release/schonhouse`
when built natively).

## 2. Install

```bash
sudo useradd --system --home /var/lib/schonhouse --create-home --shell /usr/sbin/nologin schonhouse
sudo install -m 755 schonhouse /usr/local/bin/schonhouse
sudo mkdir -p /etc/schonhouse
sudo install -m 644 schonhouse.toml /etc/schonhouse/schonhouse.toml
printf 'TAPO_USERNAME=you@example.com\nTAPO_PASSWORD=your-password\n' | sudo tee /etc/schonhouse/env >/dev/null
sudo chown schonhouse:schonhouse /etc/schonhouse/env && sudo chmod 600 /etc/schonhouse/env
sudo install -m 644 deploy/schonhouse.service /etc/systemd/system/schonhouse.service
sudo systemctl daemon-reload
sudo systemctl enable --now schonhouse
```

In `/etc/schonhouse/schonhouse.toml` set the database path to the service's home:

```toml
[storage]
path = "/var/lib/schonhouse/schonhouse.db"
```

Devices do not need to be listed in the config: add them from the UI (Add device →
Scan). Entries under `[[devices]]` are imported into the database on first start.

## 3. Operate

```bash
systemctl status schonhouse
journalctl -u schonhouse -f
```

The UI is at `http://<pi-address>:8080`. Give the Pi a fixed address or a DHCP
reservation, and do the same for the bulbs, because devices are identified by IP.

## Footprint

Reference numbers from a release build on x86_64 Linux (WSL2 Ubuntu, 2026-11-17), with
two bulbs configured:

| Metric | Value |
|--------|-------|
| Binary size (stripped, LTO) | 10.3 MB |
| Resident memory a few seconds after start | 10.4 MB |
| Release build time, cold cache | 1 min 41 s |

Pi 5 (aarch64) numbers will be added after the first deployment; expect the same order
of magnitude.
