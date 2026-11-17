"""Proof of concept: control two Tapo L535 bulbs over the local network.

Usage (from the repository root):

    python poc/poc.py status
    python poc/poc.py on
    python poc/poc.py off
    python poc/poc.py toggle
    python poc/poc.py brightness 40          # 1..100
    python poc/poc.py temp 2700              # color temperature in Kelvin (2500..6500)
    python poc/poc.py color 120 100          # hue 0..360, saturation 1..100
    python poc/poc.py on --ip 192.168.0.2    # act on a single bulb

Credentials come from the environment or from a `.env` file in the repository root:

    TAPO_USERNAME=you@example.com
    TAPO_PASSWORD=your-tapo-password
    BULB_IPS=192.168.0.2,192.168.0.3        # optional, this is the default

The `tapo` package is the Python binding of the Rust `tapo` crate, which is the
library the production service will use, so this script exercises the same code path.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
from pathlib import Path

from tapo import ApiClient

DEFAULT_IPS = "192.168.0.2,192.168.0.3"
REPO_ROOT = Path(__file__).resolve().parent.parent


def load_dotenv(path: Path) -> None:
    """Minimal .env loader: KEY=VALUE lines, '#' comments, no overriding of real env."""
    if not path.is_file():
        return
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        value = value.strip().strip('"').strip("'")
        os.environ.setdefault(key, value)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Tapo L535 proof of concept")
    parser.add_argument(
        "--ip",
        action="append",
        help="Bulb IP address (repeatable). Defaults to BULB_IPS env or "
        + DEFAULT_IPS,
    )
    parser.add_argument(
        "--json", action="store_true", help="Print raw device info as JSON"
    )
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("status", help="Print device info")
    sub.add_parser("on", help="Turn on")
    sub.add_parser("off", help="Turn off")
    sub.add_parser("toggle", help="Invert the current on/off state")
    p = sub.add_parser("brightness", help="Set brightness 1..100")
    p.add_argument("value", type=int)
    p = sub.add_parser("temp", help="Set color temperature in Kelvin (2500..6500)")
    p.add_argument("kelvin", type=int)
    p = sub.add_parser("color", help="Set hue (0..360) and saturation (1..100)")
    p.add_argument("hue", type=int)
    p.add_argument("saturation", type=int)
    return parser.parse_args(argv)


def summarize(ip: str, info: dict) -> str:
    state = "ON " if info.get("device_on") else "OFF"
    parts = [
        f"{ip:<14} {state}",
        f"nick={info.get('nickname')!r}",
        f"model={info.get('model')}",
        f"fw={info.get('fw_ver')}",
        f"brightness={info.get('brightness')}",
    ]
    if info.get("color_temp"):
        parts.append(f"color_temp={info.get('color_temp')}K")
    else:
        parts.append(f"hue={info.get('hue')} sat={info.get('saturation')}")
    parts.append(f"rssi={info.get('rssi')}")
    return "  ".join(parts)


async def run_one(client: ApiClient, ip: str, args: argparse.Namespace) -> None:
    started = time.perf_counter()
    # Dedicated L535 handler (same color-light API surface as the L530).
    device = await client.l535(ip)
    connect_ms = (time.perf_counter() - started) * 1000

    cmd = args.command
    if cmd == "on":
        await device.on()
    elif cmd == "off":
        await device.off()
    elif cmd == "toggle":
        info = await device.get_device_info()
        if info.device_on:
            await device.off()
        else:
            await device.on()
    elif cmd == "brightness":
        await device.set_brightness(args.value)
    elif cmd == "temp":
        await device.set_color_temperature(args.kelvin)
    elif cmd == "color":
        await device.set_hue_saturation(args.hue, args.saturation)

    info = (await device.get_device_info()).to_dict()
    total_ms = (time.perf_counter() - started) * 1000
    if args.json:
        print(json.dumps({"ip": ip, "info": info}, indent=2))
    else:
        print(summarize(ip, info))
    print(f"{'':<14} handshake {connect_ms:.0f} ms, total {total_ms:.0f} ms")


async def main(argv: list[str]) -> int:
    load_dotenv(REPO_ROOT / ".env")
    args = parse_args(argv)

    username = os.environ.get("TAPO_USERNAME")
    password = os.environ.get("TAPO_PASSWORD")
    if not username or not password:
        print(
            "Missing TAPO_USERNAME / TAPO_PASSWORD. Copy .env.example to .env and "
            "fill in your Tapo account credentials.",
            file=sys.stderr,
        )
        return 2

    ips = args.ip or os.environ.get("BULB_IPS", DEFAULT_IPS).split(",")
    ips = [ip.strip() for ip in ips if ip.strip()]

    client = ApiClient(username, password)
    results = await asyncio.gather(
        *(run_one(client, ip, args) for ip in ips), return_exceptions=True
    )

    failures = 0
    for ip, result in zip(ips, results):
        if isinstance(result, BaseException):
            failures += 1
            print(f"{ip:<14} ERROR {type(result).__name__}: {result}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main(sys.argv[1:])))
