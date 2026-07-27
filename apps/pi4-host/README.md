# vinyl-bridge-pi — Linux (Pi 4) host + macOS dev loop

Standalone **Rust** host that mirrors the ESP32-P4 / C5 firmware control surface.

| Loop | Platform | Capture | When |
|---|---|---|---|
| **Daily (preferred)** | **macOS** (Pascal’s machine) | **cpal → CoreAudio** | API, gate, record, session logic |
| Deploy | Raspberry Pi 4 (64-bit) | **ALSA** | Image + systemd + real line-in |
| Validate deploy | QEMU aarch64 (`virt` / `raspi4b`) | silence/ALSA in guest | Before calling an image flash-ready |

```
  macOS mic/line (CoreAudio/cpal) ──► vinyl-bridge-pi (dev)
  Pi line-in (ALSA / USB / I2S HAT) ──► vinyl-bridge-pi (deploy)
                                         │ HTTP + mDNS _vinylbridge._tcp
                                         │ AirPlay 2 / Chromecast (scaffolded)
                                         ▼
                                      HomePod / Cast speakers
```

| Piece | Path |
|---|---|
| App (Rust) | `src/` |
| Live status (Kyoto pings) | [`STATUS.md`](STATUS.md) |
| **Makefile / justfile** | `make build-macos` · `make build-pi` |
| Config example | `config.example.toml` |
| systemd unit | `deploy/systemd/vinyl-bridge.service` |
| Pi binary (Docker) | `scripts/build-release.sh` |
| SD image recipe | `deploy/image/build-sd-image.sh` |
| QEMU smoke | `scripts/qemu-smoke.sh` (**gate before “flash-ready”**) |
| SSH install (running Pi) | `scripts/install-on-running-pi.sh` |

## Features (Cargo)

| Feature | Default | Backend | Platform |
|---|---|---|---|
| `cpal` | **yes** | CoreAudio (macOS), WASAPI/Pulse elsewhere | macOS fast loop |
| `alsa` | no | ALSA `libasound` | Linux / Raspberry Pi only |

ALSA is **feature-gated** so macOS never links `libasound`. Pi release builds use
`--no-default-features --features alsa`.

## macOS fast loop (preferred)

```bash
cd apps/pi4-host

# Make / just
make build-macos          # or: just build-macos
make run-macos            # HTTP 127.0.0.1:8080
make test-macos           # short curl smoke

# Raw cargo (same thing)
cargo build --release --features cpal
cargo run --features cpal -- --http-bind 127.0.0.1:8080

curl -s http://127.0.0.1:8080/api/status | jq .
# parity.capture should be "cpal" when an input device opens
```

**CoreAudio notes**

- Default input = System Settings → Sound → Input (or USB ADC).
- Override: `capture_device = "USB"` (substring match) in config, or leave `"default"`.
- Grant **Microphone** permission to the terminal/app on first run (macOS privacy).
- Recordings default to `/var/lib/vinyl-bridge/rec` — on macOS either run with a
  local config (`record_dir = "./rec"`) or `sudo mkdir -p` that path.

Example local config:

```bash
cp config.example.toml /tmp/vinyl.toml
# edit record_dir = "/tmp/vinyl-rec"
cargo run --features cpal -- --config /tmp/vinyl.toml --http-bind 127.0.0.1:8080
```

## Linux / Pi build

```bash
# On the Pi (native aarch64)
cargo build --release --no-default-features --features alsa

# On macOS → aarch64 artifact for SD/QEMU
make build-pi             # or: just build-pi
# → dist/vinyl-bridge-pi
```

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

**Do not call the image flash-ready until QEMU smoke is green** (or hardware
boot is confirmed). The recipe alone only *assembles* an image.

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

### 3) QEMU smoke (prefer before flashing)

Install QEMU 8+ (`brew install qemu` on macOS; `apt install qemu-system-arm` on Debian).

```bash
cd apps/pi4-host

# Fast: aarch64 binary under Docker linux/arm64 → curl /api/status
./scripts/qemu-smoke.sh binary

# Boot path: Debian arm64 cloud on -M virt + systemd unit + binary via 9p
./scripts/qemu-smoke.sh virt

# Full SD image (needs dist/vinyl-bridge-pi4.img + QEMU with raspi4b)
./scripts/qemu-smoke.sh image

# binary + virt (+ image if present)
./scripts/qemu-smoke.sh all
```

#### Manual QEMU commands (reference)

**A) Binary-only (Docker multiarch — no full OS boot):**

```bash
docker run --rm --platform linux/arm64 -p 18080:8080 \
  -v "$PWD/dist/vinyl-bridge-pi:/vinyl-bridge-pi:ro" \
  debian:bookworm-slim \
  /vinyl-bridge-pi --http-bind 0.0.0.0:8080
# other terminal:
curl -s http://127.0.0.1:18080/api/status
```

**B) System emulation — `-M virt` (widely available):**

```bash
# After qemu-smoke prepares overlay/seed, equivalent shape:
qemu-system-aarch64 \
  -machine virt -cpu max -m 1024 -smp 2 -nographic \
  -drive if=virtio,format=qcow2,file=dist/qemu-work/virt-overlay.qcow2 \
  -drive if=virtio,format=raw,file=dist/qemu-work/seed.iso \
  -fsdev local,id=fsdev0,path=$PWD/dist,security_model=mapped \
  -device virtio-9p-device,fsdev=fsdev0,mount_tag=host \
  -netdev user,id=net0,hostfwd=tcp::18080-:80 \
  -device virtio-net-device,netdev=net0
```

**C) Pi 4 machine model — `-M raspi4b` (when your QEMU lists it):**

```bash
qemu-system-aarch64 -machine help | grep raspi4b

# kernel8.img + bcm2711-rpi-4-b.dtb from the image boot partition:
qemu-system-aarch64 \
  -machine raspi4b -cpu cortex-a72 -m 2048 -nographic \
  -kernel kernel8.img -dtb bcm2711-rpi-4-b.dtb \
  -drive file=dist/vinyl-bridge-pi4.img,if=sd,format=raw \
  -append "console=ttyAMA0,115200 root=/dev/mmcblk0p2 rootwait rw" \
  -device usb-net,netdev=net0 \
  -netdev user,id=net0,hostfwd=tcp::18080-:80
```

| What QEMU proves | What it does **not** prove |
|---|---|
| aarch64 binary runs | Real WM8782/USB line-in quality |
| systemd unit starts (virt) | Wi‑Fi / mDNS on real RF |
| HTTP `/api/status` | AirPlay → HomePod (still stub) |
| raspi4b boots SD image (if machine exists) | Full USB audio / HAT passthrough |

If `raspi4b` is missing, treat **virt + binary** as the gate for “image recipe is sane”; still do a hardware flash before shipping cards.

### 4) Flash the card (after smoke)

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

### 5) First boot

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
