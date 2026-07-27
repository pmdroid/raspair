# pi4-host — live status (Kyoto / Telegram pings)

**Branch:** `feat/pi4-linux-host`  
**Last update (UTC):** 2026-07-27  
**Pre-Pi origin SHA:** `d1dfdaecacade5ffaeef2725a5db23469951db4f`

## Milestone results (this session)

| Gate | Result | Notes |
|---|---|---|
| `make build-pi` | **PASS** | ELF aarch64 + ALSA → `dist/vinyl-bridge-pi` (~2.9 MiB stripped) |
| `qemu-smoke.sh binary` | **PASS** | Docker linux/arm64 + libasound2 → `/api/status` |
| `qemu-smoke.sh virt` | **PASS** | `-M virt` + EDK2 UEFI + cloud-init + systemd → `/api/status` |
| Full SD `*.img` assemble | not run | recipe ready; use `deploy/image/build-sd-image.sh` on Linux/privileged Docker |
| `qemu-smoke.sh image` (raspi4b) | skipped | needs assembled `dist/vinyl-bridge-pi4.img` |

**Artifact:** `dist/vinyl-bridge-pi`  
**file(1):** `ELF 64-bit LSB pie executable, ARM aarch64 … stripped`  
**sha256:** `85bc799000e3900c7ce3e025affe6e9af3d894e83c3331e8d4f24ce223f729be`  
**BuildID:** `10e4e51c35b46dd631873af9166ed16effd6ff15`

## Flash path status

- **Binary + systemd path:** validated in QEMU virt (install + enable unit + HTTP).  
- **Flashable Pi OS card:** follow README “Flash path (validated binary)” — assemble image with `build-sd-image.sh`, then Imager/`dd`.  
- **Not claimed:** real ALSA line-in, Wi‑Fi RF, AirPlay→HomePod (still stubs).

## Workflow

```
make run-macos          # daily (CoreAudio)
make build-pi           # PASS
make qemu-smoke         # binary (+ virt) PASS
# then assemble SD image when flashing hardware
```

## Parity

Control plane OK on aarch64 Linux guest. AirPlay 2 / Chromecast still stubs.
