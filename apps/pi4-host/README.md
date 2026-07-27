# vinyl-bridge-pi — Raspberry Pi 4 Linux host

Standalone **Rust** host that mirrors the ESP32-P4 / C5 firmware control surface
on minimal Linux (Raspberry Pi OS Lite 64-bit).

```
  line-in (ALSA / USB or I2S HAT) ──► Pi 4 (vinyl-bridge-pi)
                                         │ HTTP :80 + mDNS _vinylbridge._tcp
                                         │ AirPlay 2 / Chromecast (scaffolded)
                                         ▼
                                      HomePod / Cast speakers
```

| Piece | Path |
|---|---|
| App (Rust) | `src/` |
| Config example | `config.example.toml` |
| systemd unit | `deploy/systemd/vinyl-bridge.service` |
| Cross-build (Docker) | `deploy/Dockerfile.cross` + `scripts/build-release.sh` |
| Flashable SD recipe | `deploy/image/build-sd-image.sh` |
| SSH install (running Pi) | `scripts/install-on-running-pi.sh` |

## Why Rust

- Low-latency capture + future crypto/ALAC path without a GC pause model
- Easy static-ish release binary for aarch64
- Existing experimental interest under `apps/airplay-rs/` (build tree only)

## Dev build (any host)

```bash
cd apps/pi4-host
cargo build
# non-root HTTP:
cargo run -- --http-bind 127.0.0.1:8080
curl -s localhost:8080/api/status | jq .
```

On Linux with ALSA:

```bash
cargo run --features alsa -- --http-bind 0.0.0.0:8080
```

Default capture without `--features alsa` is a **silence generator** (so macOS
and CI still compile). Signal gate, record, and HTTP still exercise the same
control paths as the firmware.

## HTTP API (parity with p4-bridge)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/status` | name, streaming, volume, targets, record, parity flags |
| GET | `/api/speakers` | mDNS browse cache (`_airplay` / `_googlecast`) |
| POST | `/api/select` | `{ "speakers": [ ... ] }` |
| POST | `/api/play` / `/api/stop` | session intent |
| POST | `/api/volume` | `{ "db": -30.0 }` |
| GET/POST | `/api/record*` | WAV under `record_dir` |
| GET | `/api/stats` | uptime + counters |
| GET | `/api/ota` | reports unsupported (use image rebuild / SSH) |

Android companion discovery uses mDNS service **`_vinylbridge._tcp`**, hostname
default `vinyl-bridge.local`.

---

## Flashable SD card path

### 1) Build the aarch64 binary

On macOS/x86_64 (Docker required):

```bash
cd apps/pi4-host
./scripts/build-release.sh
# → dist/vinyl-bridge-pi  (ELF aarch64, ALSA enabled)
```

On a Pi or aarch64 Linux host the same script builds natively.

### 2) Assemble the image (Linux or Docker)

```bash
# From a Linux machine with loop mounts, or Docker --privileged:
./deploy/image/build-sd-image.sh
# → dist/vinyl-bridge-pi4.img
# → dist/vinyl-bridge-pi4.img.xz
```

macOS example (privileged container):

```bash
docker run --privileged --rm \
  -v "$PWD":/src -w /src \
  debian:bookworm-slim \
  bash -c 'apt-get update && apt-get install -y curl xz-utils mount util-linux e2fsprogs ca-certificates \
    && ./deploy/image/build-sd-image.sh'
```

The recipe:

1. Downloads **Raspberry Pi OS Lite arm64**
2. Loop-mounts boot + root
3. Installs `vinyl-bridge-pi` → `/usr/local/bin/`
4. Installs config → `/etc/vinyl-bridge/config.toml`
5. Enables `vinyl-bridge.service`
6. Enables SSH (`/boot/firmware/ssh` or `/boot/ssh`)
7. Writes `dist/vinyl-bridge-pi4.img(.xz)`

### 3) Flash the card

**Raspberry Pi Imager (recommended)**

1. Choose OS → **Use custom** → select `dist/vinyl-bridge-pi4.img` (decompress `.xz` first if needed)
2. Optional: set Wi‑Fi, user, locale in Imager’s gear menu
3. Write to the microSD card
4. Insert into Pi 4, power on

**dd (macOS)**

```bash
# diskutil list   → note the raw disk, e.g. /dev/rdisk4  (NOT a partition)
xzcat dist/vinyl-bridge-pi4.img.xz | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**dd (Linux)**

```bash
xzcat dist/vinyl-bridge-pi4.img.xz | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
```

### 4) First boot

1. Ethernet (or preconfigured Wi‑Fi via Imager)
2. SSH: `ssh vinyl@vinyl-bridge.local` (or the user you set in Imager)
3. Check service:

```bash
sudo systemctl status vinyl-bridge
curl -s http://vinyl-bridge.local/api/status
```

4. Point line-in at the ALSA device in `/etc/vinyl-bridge/config.toml`
   (`arecord -l`), then `sudo systemctl restart vinyl-bridge`.

### Alternative: update a running Pi without re-flashing

```bash
./scripts/build-release.sh
./scripts/install-on-running-pi.sh vinyl@192.168.x.y
```

---

## Parity gaps vs ESP `p4-bridge` / `firmware`

| Capability | ESP host | Pi host (this scaffold) |
|---|---|---|
| HTTP control API | yes | **yes** (same routes) |
| mDNS advertise `_vinylbridge._tcp` | yes | **yes** |
| mDNS browse AirPlay / Cast | yes | **yes** (best-effort) |
| Line-in capture | I2S DMA 24/48 | ALSA (feature) / silence fallback |
| Signal gate auto play/stop | yes | **yes** |
| WAV session record + download | microSD | **yes** (`record_dir`) |
| AirPlay 2 (RTSP/HAP/ALAC/PTP/buffer) | production | **stub** — port `components/airplay2` |
| Chromecast sender | production | **stub** |
| Multi-room fail-soft | yes | not yet (depends on AirPlay port) |
| OTA TCP 5001 / dual P4+C5 | yes | **n/a** — rebuild image / SSH deploy |
| ESP-Hosted SPI / C5 radio | yes | **n/a** — Pi native Wi‑Fi/Ethernet |
| Telemetry → ClickHouse | optional | not wired |
| Status LED / button | planned | not wired |

**Bottom line:** control plane + packaging path are in place so Android can find
the device and drive select/play/stop/record. **Shipping audio to HomePod
requires finishing the AirPlay 2 sender** (reuse or FFI the C library under
`apps/firmware/components/airplay2/`).

## Hardware notes (Pi 4)

- Prefer **Ethernet** or solid 5 GHz Wi‑Fi for AirPlay TCP
- Line-in: USB audio ADC (class-compliant) or I2S HAT → ALSA device in config
- Power: official 5 V / 3 A USB-C supply
- microSD: 8 GB+ Class A1; recordings land on rootfs under `/var/lib/vinyl-bridge/rec`

## License / tree

Part of the vinyl-project monorepo. ESP firmware remains the shipping path for
the custom PCB; this tree is the **Linux host alternative** for bring-up,
bench testing, and a Pi-based product variant.
