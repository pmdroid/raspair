# pi4-host — live status (Kyoto / Telegram pings)

**Branch:** `feat/pi4-linux-host`  
**Last update (UTC):** 2026-07-27  
**Pre-Pi origin SHA:** `d1dfdaecacade5ffaeef2725a5db23469951db4f`

## Now

| Item | State |
|---|---|
| **macOS native binary** | **done** — `make build-macos` → Mach-O arm64; `make test-macos` PASS |
| Capture on macOS | **cpal/CoreAudio** (default feature); falls back to silence if mic permission denied |
| Makefile / justfile | **done** — `build-macos`, `run-macos`, `build-pi`, `qemu-smoke` |
| Rust host scaffold | **done** — HTTP/mDNS/session/record; AirPlay+CC stubs |
| systemd + SD recipe | **done** (scripts) |
| aarch64 Pi binary | **pending** — `make build-pi` (Docker arm64 + ALSA) |
| QEMU smoke | **scripts+docs ready**; not green yet → **not flash-ready** |

## Workflow (Pascal)

```
make run-macos          # daily API/gate/record (CoreAudio)
make build-pi           # dist/vinyl-bridge-pi aarch64
make qemu-smoke         # deploy gate before flash
```

## Flash-ready

**no** — wait for QEMU (or hardware) green.

## Parity

Control plane OK; AirPlay 2 / Chromecast still stubs.
