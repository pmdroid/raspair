# pi4-host — live status (Kyoto / Telegram pings)

**Branch:** `feat/pi4-linux-host`  
**Tip (pushed):** `65eb5a2efa9b736ca0d9295f1e64ddfe2ca69b6f`  
**Last update (UTC):** 2026-07-27  
**Base OS (deploy image):** **Buildroot** (busybox init) — **not** Raspberry Pi OS

## Milestone results

| Gate | Result | Notes |
|---|---|---|
| `make build-pi` | **PASS** | ELF aarch64 + ALSA → `dist/vinyl-bridge-pi` |
| Buildroot recipe | **PASS** | `apps/pi4-host/buildroot/` br2-external |
| **Buildroot `sdcard.img`** | **PASS** | ~**288 MiB** raw · ~**29 MiB** xz |
| Legacy Pi OS Lite inject | **DEPRECATED** | multi‑GB; `make image-legacy-pios` only |

**Not claimed:** real ALSA line-in, Wi‑Fi RF, AirPlay→HomePod (stubs).

## Base OS

| | |
|---|---|
| **Appliance** | Buildroot **2024.02.9** + br2-external `apps/pi4-host/buildroot` |
| **Defconfig** | `vinyl_rpi4_64_defconfig` (Pi 4, aarch64, bcm2711) |
| **Init** | busybox SysV (`/etc/init.d/S99vinyl-bridge`) — **no systemd** |
| **Network** | Ethernet DHCP (`eth0`); Wi‑Fi deferred |
| **Audio** | ALSA (`alsa-lib` + `arecord`/`aplay`) |
| **SSH** | dropbear (small) |
| **App** | Prebuilt `vinyl-bridge-pi` → `/usr/bin/vinyl-bridge-pi` |
| **Size** | boot 32 MiB + root 256 MiB ≈ **288 MiB** raw card image |
| **Root login** | `root` / `vinyl` (change after first boot) |
| **Hostname** | `vinyl-bridge` |

## Flashable SD image (Buildroot)

| Artifact | Path | Size |
|---|---|---|
| Raw image | `~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img` | **288 MiB** (301990400 bytes) |
| Compressed | `…/vinyl-bridge-pi4.img.xz` | **~29 MiB** |
| Checksums | `…/SHA256SUMS` | — |
| Build cache (macOS) | Docker volume `vinyl-buildroot-cache` | case-sensitive |

**SHA256 (this workstation build):**

```
fb42a77c14c9790f5a1832cc5ae7f47a1d15e341aa7e193551a3f5a7a778d8d3  vinyl-bridge-pi4.img
2b1517ea693513fbf6bf52df2a28f65d2767b9d1f56c633798dffa471e3c9b97  vinyl-bridge-pi4.img.xz
```

`file(1)`: DOS/MBR boot sector; partition 1 FAT (boot), partition 2 Linux (rootfs).

### Rebuild

```bash
cd apps/pi4-host
make build-pi
make image    # macOS → deploy/buildroot/build.macos.sh (Docker volume)
```

**macOS note:** Buildroot `O=` must be on a **case-sensitive** FS (glibc
`stamp.os` vs `stamp.oS`). `build.macos.sh` uses Docker volume
`vinyl-buildroot-cache` — do not bind-mount APFS for the build tree.

### Flash (macOS)

```bash
# diskutil list → whole disk e.g. /dev/rdisk4
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**Raspberry Pi Imager:** OS → Use custom → `vinyl-bridge-pi4.img` → Write.

### First boot

```bash
# Ethernet; serial 115200 or HDMI. Login: root / vinyl
ps | grep vinyl
curl -s http://<pi-ip>/api/status
arecord -l
vi /etc/vinyl-bridge/config.toml
/etc/init.d/S99vinyl-bridge restart
```

## Workflow

```
make run-macos          # daily CoreAudio (separate from image)
make build-pi           # aarch64 ELF for appliance
make image              # Buildroot → ~/.cache/.../vinyl-bridge-pi4.img
# flash → first boot → curl /api/status
```

## Legacy

| Path | Status |
|---|---|
| `deploy/image/*` Pi OS Lite inject | **Deprecated** |
| Prior ~2.8 GiB Pi OS card images | Superseded (~288 MiB Buildroot) |

## Parity

Control plane + **Buildroot packaging** OK. AirPlay 2 / Chromecast still stubs.
