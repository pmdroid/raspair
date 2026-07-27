# Buildroot external tree — Vinyl Bridge Pi 4 appliance

**Base OS: Buildroot (busybox init).** Not Raspberry Pi OS / Debian.

This directory is a [br2-external](https://buildroot.org/downloads/manual/manual.html#outside-br-custom)
tree that produces a **small, flashable `sdcard.img`** for Raspberry Pi 4 (64-bit)
with the `vinyl-bridge-pi` daemon preinstalled.

| Goal | Target |
|---|---|
| Size | As small as practical (~100–300 MiB raw vs multi‑GB Pi OS Lite) |
| Init | **busybox** SysV scripts (`S99vinyl-bridge`) — no systemd |
| Network | Ethernet + DHCP (`eth0`); Wi‑Fi deferred |
| Audio | ALSA (`alsa-lib` + `arecord`) |
| App | Prebuilt aarch64 ELF from `make build-pi` |

## Layout

```
buildroot/                          ← BR2_EXTERNAL root
├── external.desc                   name: VINYL
├── external.mk / Config.in
├── configs/vinyl_rpi4_64_defconfig
├── board/vinyl-rpi4/
│   ├── config.txt                  Pi 4 boot (64-bit, lean gpu_mem)
│   ├── post-build.sh
│   └── rootfs_overlay/             hostname, init.d, config.toml
└── package/vinyl-bridge-pi/        installs staged prebuilt binary
```

Upstream Buildroot (pinned release, default **2024.02.9**) is **downloaded
at build time** into `~/.cache/vinyl-bridge-buildroot/` — never committed.

## Quick build (preferred entrypoints)

From `apps/pi4-host`:

```bash
# 1) aarch64 + ALSA binary (Docker on macOS; native on Pi/Linux aarch64)
make build-pi

# 2) Buildroot image → flashable .img
make image
# same as:
#   macOS: ./deploy/buildroot/build.macos.sh
#   Linux: ./deploy/buildroot/build.sh
```

### Outputs (not in git)

| Artifact | Default path |
|---|---|
| Raw SD image | `~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img` |
| Compressed | `…/vinyl-bridge-pi4.img.xz` |
| Buildroot cache (Linux) | `$BUILDROOT_CACHE` (must be **case-sensitive**) |
| Buildroot cache (macOS) | Docker volume `vinyl-buildroot-cache` → `/cache` |
| Convenience links | `apps/pi4-host/dist/vinyl-bridge-pi4.img{,.xz}` (gitignored) |

### Case-sensitive filesystem requirement

glibc creates both `stamp.os` and `stamp.oS`. On **case-insensitive APFS**
(default macOS) those collide and the toolchain dies with:

```text
No rule to make target '.../stdlib/stamp.os'
```

**macOS:** always use `make image` / `build.macos.sh` (Docker volume is ext4).  
**Do not** point `BUILDROOT_CACHE` at `~/…` or `/Volumes/Data` for the build tree.

Override destinations:

```bash
IMAGE_OUT=/path/to/dir BUILDROOT_CACHE=/path/to/cache JOBS=8 make image
```

### Manual Buildroot (Linux)

```bash
# after make build-pi and staging (build.sh does staging for you)
export BR2_EXTERNAL=$PWD/buildroot
export BR_DIR=~/.cache/vinyl-bridge-buildroot/buildroot-2024.02.9
export O=~/.cache/vinyl-bridge-buildroot/output-vinyl-rpi4-64

make -C "$BR_DIR" O="$O" BR2_EXTERNAL="$BR2_EXTERNAL" vinyl_rpi4_64_defconfig
make -C "$BR_DIR" O="$O" BR2_EXTERNAL="$BR2_EXTERNAL" -j$(nproc)

ls -lh "$O/images/sdcard.img"
```

First build downloads a toolchain, kernel, and firmware — often **30–90+ minutes**
depending on CPU and network. Rebuilds are incremental.

`deploy/buildroot/build.sh` builds the **toolchain serially** first (avoids a
known glibc parallel `stamp.os` race under Docker/aarch64), then finishes the
rest with `JOBS` (default: nproc).

## Flash

**Raspberry Pi Imager:** OS → Use custom → `vinyl-bridge-pi4.img` → Write.  
(Imager “OS customisation” is Pi OS–oriented; this image uses fixed root password.)

**dd (macOS):**

```bash
# diskutil list  → whole disk e.g. /dev/rdisk4 (NOT a partition)
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/rdiskN bs=4m status=progress
diskutil eject /dev/diskN
```

**dd (Linux):**

```bash
xzcat ~/.cache/vinyl-bridge-pi-images/vinyl-bridge-pi4.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
```

## First boot

1. Prefer **Ethernet** (Wi‑Fi not in the minimal image yet).
2. Serial console: `115200 8N1` on the Pi UART, or HDMI + USB keyboard.
3. Login: **`root` / `vinyl`** (change immediately for anything beyond a bench).
4. Check service and API:

```sh
# on device
ps | grep vinyl
cat /etc/vinyl-bridge/image-id
# from LAN
curl -s http://<pi-ip>/api/status
# mDNS name when network is up: vinyl-bridge.local (app advertises _vinylbridge._tcp)
```

5. Set capture device after plugging a USB ADC:

```sh
arecord -l
vi /etc/vinyl-bridge/config.toml   # capture_device = "hw:1,0" etc.
/etc/init.d/S99vinyl-bridge restart
```

## macOS daily loop (separate)

Do **not** use Buildroot for day-to-day API work:

```bash
make build-macos
make run-macos          # CoreAudio / cpal on 127.0.0.1:8080
```

Buildroot is the **deploy appliance** path only.

## Size notes

| Image | Typical raw size |
|---|---|
| **Buildroot appliance (this tree)** | ~boot 32 MiB + root 256 MiB ≈ **~300 MiB** card image |
| Legacy Pi OS Lite inject (deprecated) | multi‑GB (~2.8 GiB previously) |

Rootfs size is `BR2_TARGET_ROOTFS_EXT2_SIZE` in `configs/vinyl_rpi4_64_defconfig`
(default **256M**). Shrink further only after confirming package set + recordings fit.

## Wi‑Fi (later)

Not enabled in the minimal defconfig. When needed, add in menuconfig / defconfig:

- Raspberry Pi Wi‑Fi firmware package(s)
- `wpa_supplicant` + a small `/etc/wpa_supplicant.conf` or first-boot provisioner

Prefer Ethernet for AirPlay TCP reliability in the lab.

## Legacy Raspberry Pi OS path

`deploy/image/build-sd-image*.sh` injects the binary into **Raspberry Pi OS Lite**.
That path is **deprecated** — kept only for emergency comparison. Prefer
`make image` → Buildroot.

## What is not committed

- Buildroot source tarball / git clone
- `dl/` package downloads
- `output/` / `O=` build trees
- `package/vinyl-bridge-pi/bin/vinyl-bridge-pi` (staged binary)
- Final `*.img` / `*.img.xz`

Rebuild anytime with `make build-pi && make image`.
