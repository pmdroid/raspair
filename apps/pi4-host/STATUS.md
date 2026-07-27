# pi4-host — live status (Kyoto / Telegram pings)

**Branch:** `feat/pi4-linux-host`  
**Tip (pushed):** `e0f97bacb4df9158947ab7dac07c60a713cb5cde`
**Last update (UTC):** 2026-07-27  
**Pre-Pi origin SHA:** `d1dfdaecacade5ffaeef2725a5db23469951db4f`

## Milestone results (this session)

| Gate | Result | Notes |
|---|---|---|
| `make build-pi` | **PASS** | ELF aarch64 + ALSA → `dist/vinyl-bridge-pi` (~2.9 MiB stripped) |
| `qemu-smoke.sh binary` | **PASS** | Docker linux/arm64 + libasound2 → `/api/status` |
| `qemu-smoke.sh virt` | **PASS** | `-M virt` + EDK2 UEFI + cloud-init + systemd → `/api/status` |
| Full SD `*.img` assemble | **PASS** | `deploy/image/build-sd-image.sh` via privileged Docker + kpartx |
| Image integrity verify | **PASS** | MBR + xz test + mount: binary, unit enabled, SSH flag, hostname |

**Not claimed:** real ALSA line-in, Wi‑Fi RF, AirPlay→HomePod (still stubs). Full `-M raspi4b` QEMU boot of the card image not required for flash-ready.

## Flashable SD image (assembled)

Images are **not** committed (multi‑GB). Absolute paths on this machine:

| Artifact | Absolute path | Size |
|---|---|---|
| Raw image | `/Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img` | **2.8 GiB** (3024093184 bytes) |
| Compressed | `/Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz` | **483 MiB** |
| Checksums | `/Users/pascal/.cache/vinyl-bridge-pi-images/SHA256SUMS` | — |

Repo convenience symlinks (gitignored `dist/`):

- `apps/pi4-host/dist/vinyl-bridge-pi4.img` → cache path above  
- `apps/pi4-host/dist/vinyl-bridge-pi4.img.xz` → cache path above  

**SHA256:**

```
b91381ba28519a53c590d9afa3f13e859a9273980d1821687a1eec60c22143f7  vinyl-bridge-pi4.img
8758768130ab52c28c82542ce8d41998d834d3f1ffe1f47933a25e225103d95a  vinyl-bridge-pi4.img.xz
```

**Base OS:** Raspberry Pi OS Lite arm64 Bookworm  
`2024-11-19-raspios-bookworm-arm64-lite`  
**Built UTC:** `2026-07-27T01:54:29Z`  
**Payload BuildID:** `10e4e51c35b46dd631873af9166ed16effd6ff15`  
**Hostname:** `vinyl-bridge` · **SSH:** enabled (`/boot/firmware/ssh`) · **unit:** `vinyl-bridge.service` enabled

### Flash one-liner (macOS)

```bash
# diskutil list  → pick whole disk e.g. /dev/rdisk4 (NOT a partition)
xzcat /Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**Raspberry Pi Imager:** OS → **Use custom** →  
`/Users/pascal/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img`  
(set Wi‑Fi + username/password in Imager OS customisation) → Write.

### First boot

```bash
# Ethernet or Imager Wi‑Fi; user from Imager / userconf
ssh <user>@vinyl-bridge.local
sudo systemctl status vinyl-bridge
curl -s http://vinyl-bridge.local/api/status
# set capture_device from `arecord -l` in /etc/vinyl-bridge/config.toml
sudo systemctl restart vinyl-bridge
```

### Rebuild image

```bash
cd apps/pi4-host
make build-pi
make image    # macOS → deploy/image/build-sd-image.macos.sh
# Linux host: ./deploy/image/build-sd-image.sh
```

Default `IMAGE_OUT` = `~/.cache/vinyl-bridge-pi-images`. Override with `IMAGE_OUT=...`.

## Binary artifact

**Path:** `dist/vinyl-bridge-pi`  
**file(1):** `ELF 64-bit LSB pie executable, ARM aarch64 … stripped`  
**sha256:** `85bc799000e3900c7ce3e025affe6e9af3d894e83c3331e8d4f24ce223f729be`  
**BuildID:** `10e4e51c35b46dd631873af9166ed16effd6ff15`

## Workflow

```
make run-macos          # daily (CoreAudio)
make build-pi           # PASS
make qemu-smoke         # binary (+ virt) PASS
make image              # PASS → ~/.cache/vinyl-bridge-pi-images/*.img{,.xz}
# flash card → first boot → curl /api/status
```

## Parity

Control plane OK on aarch64 Linux guest + packaged on Pi OS Lite card.  
AirPlay 2 / Chromecast still stubs.
