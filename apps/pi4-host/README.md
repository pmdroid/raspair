# vinyl-bridge-pi — Linux (Pi 4) host + macOS dev loop

Standalone **Rust** host that mirrors the ESP32-P4 / C5 firmware control surface.

| Loop | Platform | Capture | When |
|---|---|---|---|
| **Daily (preferred)** | **macOS** (Pascal’s machine) | **cpal → CoreAudio** | API, gate, record, session logic |
| **Deploy appliance** | Raspberry Pi 4 (64-bit) | **ALSA** | **Buildroot** image + busybox init + real line-in |
| Validate binary | QEMU aarch64 / Docker | silence/ALSA in guest | Before calling flash-ready |

```
  macOS mic/line (CoreAudio/cpal) ──► vinyl-bridge-pi (dev)
  Pi line-in (ALSA / USB / I2S HAT) ──► vinyl-bridge-pi (Buildroot appliance)
                                         │ HTTP + mDNS _vinylbridge._tcp
                                         │ AirPlay 2 / Chromecast (scaffolded)
                                         ▼
                                      HomePod / Cast speakers
```

| Piece | Path |
|---|---|
| App (Rust) | `src/` |
| Live status | [`STATUS.md`](STATUS.md) |
| **Buildroot external tree** | [`buildroot/`](buildroot/) — **preferred SD image** |
| Buildroot how-to | [`buildroot/README.md`](buildroot/README.md) |
| Makefile / justfile | `make build-macos` · `make build-pi` · `make image` |
| Config example | `config.example.toml` |
| Pi binary (Docker) | `scripts/build-release.sh` |
| **Legacy** Pi OS inject | `deploy/image/*` (**deprecated**) |
| QEMU smoke | `scripts/qemu-smoke.sh` |
| SSH install (running Linux) | `scripts/install-on-running-pi.sh` |

## Features (Cargo)

| Feature | Default | Backend | Platform |
|---|---|---|---|
| `cpal` | **yes** | CoreAudio (macOS), WASAPI/Pulse elsewhere | macOS fast loop |
| `alsa` | no | ALSA `libasound` | Linux / Raspberry Pi only |

ALSA is **feature-gated** so macOS never links `libasound`. Pi release builds use
`--no-default-features --features alsa`.

## macOS fast loop (preferred for daily work)

```bash
cd apps/pi4-host

make build-macos          # or: just build-macos
make run-macos            # HTTP 127.0.0.1:8080
make test-macos           # short curl smoke

# Raw cargo
cargo build --release --features cpal
cargo run --features cpal -- --http-bind 127.0.0.1:8080

curl -s http://127.0.0.1:8080/api/status | jq .
```

**CoreAudio notes**

- Default input = System Settings → Sound → Input (or USB ADC).
- Override: `capture_device = "USB"` (substring) in config, or `"default"`.
- Grant **Microphone** permission to the terminal on first run.
- Prefer a local `record_dir` on macOS (e.g. `./rec`).

```bash
cp config.example.toml /tmp/vinyl.toml
# edit record_dir = "/tmp/vinyl-rec"
cargo run --features cpal -- --config /tmp/vinyl.toml --http-bind 127.0.0.1:8080
```

## Linux / Pi binary

```bash
# On the Pi (native aarch64)
cargo build --release --no-default-features --features alsa

# On macOS → aarch64 artifact for Buildroot / QEMU
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
| GET | `/api/ota` | unsupported — rebuild Buildroot image / SCP binary |

Android discovery: mDNS **`_vinylbridge._tcp`**, hostname **`vinyl-bridge.local`**.

---

## Flash path — Buildroot appliance (preferred)

**Not Raspberry Pi OS.** Custom **Buildroot** rootfs: busybox init, ALSA, dropbear
SSH, preinstalled `vinyl-bridge-pi`. Target size **~300 MiB** raw (vs multi‑GB
Pi OS Lite).

| Step | Command | Result |
|---|---|---|
| aarch64 + ALSA binary | `make build-pi` | `dist/vinyl-bridge-pi` |
| Binary smoke | `./scripts/qemu-smoke.sh binary` | `/api/status` on linux/arm64 |
| **Buildroot image** | `make image` | **PASS** ~288 MiB `.img` / ~29 MiB `.xz` |

Full board docs: **[`buildroot/README.md`](buildroot/README.md)**.

### End-to-end

```bash
cd apps/pi4-host

make build-pi
# optional: make qemu-smoke

make image
# macOS: deploy/buildroot/build.macos.sh  (Docker Linux + Buildroot)
# Linux:  deploy/buildroot/build.sh
#
# Default output (NOT in git):
#   ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img
#   ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz
# Build cache:
#   ~/.cache/vinyl-bridge-buildroot/{dl,output-vinyl-rpi4-64,buildroot-*}
```

First Buildroot build is slow (toolchain + kernel). Rebuilds are incremental.

### Flash the card

**Raspberry Pi Imager:** OS → **Use custom** → `vinyl-bridge-pi4.img` → Write.

**dd (macOS):**

```bash
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**dd (Linux):**

```bash
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
```

### First boot

| Item | Value |
|---|---|
| Login | `root` / `vinyl` (change ASAP) |
| Network | Ethernet DHCP (`eth0`); Wi‑Fi later |
| Daemon | `/etc/init.d/S99vinyl-bridge` (busybox) |
| Config | `/etc/vinyl-bridge/config.toml` |
| Binary | `/usr/bin/vinyl-bridge-pi` |
| API | `http://<ip>/api/status` |

```bash
# serial 115200 or HDMI+keyboard
arecord -l
vi /etc/vinyl-bridge/config.toml
/etc/init.d/S99vinyl-bridge restart
curl -s http://vinyl-bridge.local/api/status
```

### Update a running appliance (no full re-flash)

```bash
make build-pi
scp dist/vinyl-bridge-pi root@<pi-ip>:/usr/bin/vinyl-bridge-pi
ssh root@<pi-ip> '/etc/init.d/S99vinyl-bridge restart'
```

(`scripts/install-on-running-pi.sh` still targets systemd Pi OS layouts.)

---

## Legacy: Raspberry Pi OS Lite inject (**deprecated**)

`deploy/image/build-sd-image*.sh` and `make image-legacy-pios` inject the
binary into a multi‑GB **Raspberry Pi OS Lite** card. **Do not use for
product/deploy.** Kept only for emergency comparison.

```bash
make image-legacy-pios   # prints deprecation warning
```

---

## Parity gaps vs ESP `p4-bridge` / `firmware`

| Capability | ESP host | Pi host (this tree) |
|---|---|---|
| HTTP control API | yes | **yes** |
| mDNS `_vinylbridge._tcp` | yes | **yes** |
| Line-in capture | I2S DMA 24/48 | ALSA (feature) / silence fallback |
| Signal gate | yes | **yes** |
| WAV record | microSD | **yes** (`record_dir`) |
| AirPlay 2 | production | **stub** |
| Chromecast | production | **stub** |
| OTA TCP 5001 | yes | **n/a** — rebuild image / SCP |
| Base OS | FreeRTOS | **Buildroot appliance** (or macOS dev) |

**Bottom line:** control plane + **Buildroot packaging** are in place. Shipping
audio to HomePod still needs the AirPlay 2 sender port.

## Hardware notes (Pi 4)

- Prefer **Ethernet** for AirPlay TCP (Wi‑Fi not in minimal image yet)
- Line-in: USB audio ADC or I2S HAT → ALSA device in config
- Power: official 5 V / 3 A USB-C
- microSD: 1 GB+ is enough for the Buildroot image; recordings need free rootfs

## License / tree

Part of the vinyl-project monorepo. ESP firmware remains the shipping path for
the custom PCB; this tree is the **Linux host alternative** (macOS daily loop +
Buildroot Pi appliance).
