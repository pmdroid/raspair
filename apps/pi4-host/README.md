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

## Flash path (validated binary + SD image)

**Validated 2026-07-27** (see [`STATUS.md`](STATUS.md) for absolute paths + SHA256):

| Step | Command | Result |
|---|---|---|
| aarch64 + ALSA binary | `make build-pi` | **PASS** — `dist/vinyl-bridge-pi` ELF aarch64 |
| Binary smoke | `./scripts/qemu-smoke.sh binary` | **PASS** — `/api/status` on linux/arm64 |
| Boot + systemd smoke | `./scripts/qemu-smoke.sh virt` | **PASS** — UEFI `-M virt` + unit + HTTP |
| SD image assemble | `make image` | **PASS** — Pi OS Lite arm64 + vinyl-bridge payload |

QEMU does **not** prove real line-in, Wi‑Fi RF, or AirPlay→HomePod.

### End-to-end: build → smoke → flash

```bash
cd apps/pi4-host

# 1) Pi deploy binary (Docker linux/arm64 + ALSA)
make build-pi
# → dist/vinyl-bridge-pi

# 2) QEMU gates (binary + virt)
make qemu-smoke

# 3) Assemble flashable Pi OS Lite image
make image
# macOS: deploy/image/build-sd-image.macos.sh (privileged Docker + kpartx)
# Linux:  deploy/image/build-sd-image.sh
#
# Default write location (NOT in git):
#   ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img
#   ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz
# Symlinks also appear under dist/ (gitignored).

# 4) Flash — see below
```

### 1) Build the aarch64 binary

```bash
make build-pi
# same as: ./scripts/build-release.sh
# → dist/vinyl-bridge-pi  (ELF aarch64, ALSA, stripped ~2.9 MiB)
```

Sources are streamed into Docker via **tar** (avoids flaky bind mounts on external disks).

### 2) Assemble the SD image

**macOS (recommended):**

```bash
make image
# or: ./deploy/image/build-sd-image.macos.sh
# optional: IMAGE_OUT=/path/to/dir ./deploy/image/build-sd-image.macos.sh
```

The macOS wrapper copies sources into a privileged Debian container (OrbStack/Docker
loop mounts often omit `loopNp*` nodes — the recipe uses **kpartx**), builds the
card, then `docker cp`s artifacts out. Multi‑GB images stay under
`~/.cache/vinyl-bridge-pi-images/` by default.

**Linux (native root / sudo):**

```bash
./deploy/image/build-sd-image.sh
# needs: curl xz-utils util-linux e2fsprogs kpartx cloud-guest-utils
```

Recipe: Pi OS Lite arm64 → install `vinyl-bridge-pi` + systemd unit + config →
enable SSH → grow rootfs → write `vinyl-bridge-pi4.img` + `.img.xz`.

### 3) QEMU smoke

```bash
# Install: brew install qemu   (needs edk2-aarch64-code.fd for virt)
./scripts/qemu-smoke.sh binary   # Docker arm64 + libasound2
./scripts/qemu-smoke.sh virt     # -M virt + UEFI + cloud-init + systemd
./scripts/qemu-smoke.sh image    # optional: full SD img + -M raspi4b if present
```

#### Manual QEMU reference

**Binary (prefer `qemu-smoke.sh binary` — uses docker cp + libasound2):**

```bash
# see scripts/qemu-smoke.sh smoke_binary()
```

**Virt + UEFI (shape used by the script):**

```bash
qemu-system-aarch64 \
  -machine virt,gic-version=3 -cpu max -m 2048 -smp 2 -nographic \
  -drive if=pflash,format=raw,readonly=on,file=/opt/homebrew/share/qemu/edk2-aarch64-code.fd \
  -drive if=pflash,format=raw,file=dist/qemu-work/edk2-vars.fd \
  -drive if=virtio,format=qcow2,file=dist/qemu-work/virt-overlay.qcow2 \
  -drive if=virtio,format=raw,file=dist/qemu-work/seed.img \
  -netdev user,id=net0,hostfwd=tcp::18080-:80 \
  -device virtio-net-device,netdev=net0
```

**Pi 4 model (`-M raspi4b`) — after assembling the SD image:**

```bash
qemu-system-aarch64 -machine help | grep raspi4b
# extract kernel8.img + bcm2711-rpi-4-b.dtb from boot partition, then:
qemu-system-aarch64 \
  -machine raspi4b -cpu cortex-a72 -m 2048 -nographic \
  -kernel kernel8.img -dtb bcm2711-rpi-4-b.dtb \
  -drive file=$HOME/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img,if=sd,format=raw \
  -append "console=ttyAMA0,115200 root=/dev/mmcblk0p2 rootwait rw" \
  -device usb-net,netdev=net0 \
  -netdev user,id=net0,hostfwd=tcp::18080-:80
```

| QEMU proves | QEMU does **not** prove |
|---|---|
| aarch64 binary runs | Real line-in quality |
| systemd unit starts (virt) | Wi‑Fi / mDNS on RF |
| HTTP `/api/status` | AirPlay → HomePod (stub) |

### 4) Flash the card

**Current absolute paths (this workstation):**

| File | Path | ~Size |
|---|---|---|
| `.img` | `/Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img` | 2.8 GiB |
| `.img.xz` | `/Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz` | 483 MiB |

**Raspberry Pi Imager:** OS → **Use custom** →  
`/Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img`  
→ set Wi‑Fi + username/password (Imager OS customisation) → Write.

**dd (macOS):**

```bash
# diskutil list  → raw disk e.g. /dev/rdisk4 (NOT a partition)
xzcat /Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**dd (Linux):**

```bash
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
```

### 5) First boot

Image defaults: hostname **`vinyl-bridge`**, SSH enabled, **`vinyl-bridge.service`**
enabled, config at `/etc/vinyl-bridge/config.toml`.

```bash
ssh <user>@vinyl-bridge.local   # Imager user / Ethernet IP
sudo systemctl status vinyl-bridge
curl -s http://vinyl-bridge.local/api/status
# set capture_device from `arecord -l` in /etc/vinyl-bridge/config.toml
sudo systemctl restart vinyl-bridge
```

### Alternative: update a running Pi (no re-flash)

```bash
make build-pi
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
