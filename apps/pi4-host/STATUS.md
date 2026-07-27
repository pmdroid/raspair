# pi4-host — live status (Kyoto / Telegram pings)

**Branch:** `feat/pi4-linux-host`  
**Last update (UTC):** 2026-07-26  
**Base OS (deploy image):** **Buildroot** (busybox init) — **not** Raspberry Pi OS

## Milestone results

| Gate | Result | Notes |
|---|---|---|
| `make build-pi` | **PASS** (prior) | ELF aarch64 + ALSA → `dist/vinyl-bridge-pi` (~2.9 MiB stripped) |
| `qemu-smoke.sh binary` | **PASS** (prior) | Docker linux/arm64 + libasound2 → `/api/status` |
| `qemu-smoke.sh virt` | **PASS** (prior) | Debian cloud guest + unit — validation only |
| **Buildroot recipe** | **IN TREE** | `buildroot/` br2-external + `deploy/buildroot/build*.sh` |
| **Buildroot `sdcard.img`** | **see rebuild** | `make image` → `~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img` |
| Legacy Pi OS Lite inject | **DEPRECATED** | `deploy/image/*` / `make image-legacy-pios` only |

**Not claimed:** real ALSA line-in, Wi‑Fi RF, AirPlay→HomePod (stubs).

## Base OS

| | |
|---|---|
| **Appliance** | Buildroot **2024.02.9** + br2-external `apps/pi4-host/buildroot` |
| **Defconfig** | `vinyl_rpi4_64_defconfig` (Pi 4, aarch64, bcm2711) |
| **Init** | busybox SysV (`/etc/init.d/S99vinyl-bridge`) — **no systemd** |
| **Network** | Ethernet DHCP (`eth0`); Wi‑Fi deferred |
| **Audio** | ALSA (`alsa-lib` + `arecord`) |
| **App** | Prebuilt `vinyl-bridge-pi` installed to `/usr/bin` |
| **Size target** | ~boot 32 MiB + root **256 MiB** ≈ **~300 MiB** raw card image |
| **Root login** | `root` / `vinyl` (change after first boot) |
| **Hostname** | `vinyl-bridge` |

**Not used for deploy:** Raspberry Pi OS Lite / Debian inject (legacy path only).

## Flashable SD image

Images are **not** committed. Default paths:

| Artifact | Path |
|---|---|
| Raw image | `~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img` |
| Compressed | `~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz` |
| Buildroot `O=` | `~/.cache/vinyl-bridge-buildroot/output-vinyl-rpi4-64/` |
| Package `dl/` | `~/.cache/vinyl-bridge-buildroot/dl/` |

Repo convenience symlinks (gitignored `dist/`):

- `apps/pi4-host/dist/vinyl-bridge-pi4.img`  
- `apps/pi4-host/dist/vinyl-bridge-pi4.img.xz`

### Rebuild image

```bash
cd apps/pi4-host
make build-pi
make image    # macOS → deploy/buildroot/build.macos.sh (Docker)
              # Linux → deploy/buildroot/build.sh
```

Docs: [`buildroot/README.md`](buildroot/README.md).

### Flash one-liner (macOS)

```bash
# diskutil list  → pick whole disk e.g. /dev/rdisk4 (NOT a partition)
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**Raspberry Pi Imager:** OS → **Use custom** → `vinyl-bridge-pi4.img` → Write.

### First boot

```bash
# Ethernet preferred; serial 115200 or HDMI
# login: root / vinyl
ps | grep vinyl
curl -s http://<pi-ip>/api/status
# set capture_device from arecord -l in /etc/vinyl-bridge/config.toml
/etc/init.d/S99vinyl-bridge restart
```

## Binary artifact (app only)

**Path:** `dist/vinyl-bridge-pi`  
**Build:** `make build-pi` (Docker linux/arm64 + ALSA)

## Workflow

```
make run-macos          # daily CoreAudio loop (separate from image)
make build-pi           # aarch64 ELF for appliance
make image              # Buildroot → ~/.cache/.../vinyl-bridge-pi4.img
# flash card → first boot → curl /api/status
```

## Legacy (do not use for product)

| Path | Status |
|---|---|
| `deploy/image/build-sd-image*.sh` | **Deprecated** — Pi OS Lite inject |
| `make image-legacy-pios` | Emergency only |
| Prior multi‑GB Pi OS card images | Superseded by Buildroot |

## Parity

Control plane OK on aarch64 Linux. Appliance packaging is **Buildroot**.  
AirPlay 2 / Chromecast still stubs.
