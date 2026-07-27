#!/usr/bin/env bash
# Build a flashable Raspberry Pi OS Lite (64-bit) image with vinyl-bridge-pi.
#
# Requirements (Linux host or Docker — see README):
#   - curl, xz, losetup, mount (root/sudo)
#   - prebuilt aarch64 binary at dist/vinyl-bridge-pi (see scripts/build-release.sh)
#
# Output: dist/vinyl-bridge-pi4.img.xz
#
# Usage:
#   ./deploy/image/build-sd-image.sh
#   IMAGE_URL=... ./deploy/image/build-sd-image.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIST="${ROOT}/dist"
BIN="${DIST}/vinyl-bridge-pi"
OUT_IMG="${DIST}/vinyl-bridge-pi4.img"
OUT_XZ="${OUT_IMG}.xz"

# Raspberry Pi OS Lite 64-bit (Bookworm). Override with IMAGE_URL if the
# official redirect moves. Last verified recipe shape: 2026-xx Pi OS Lite arm64.
IMAGE_URL="${IMAGE_URL:-https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2024-11-19/2024-11-19-raspios-bookworm-arm64-lite.img.xz}"
WORK="${DIST}/image-work"
IMG_XZ="${WORK}/base.img.xz"
IMG="${WORK}/base.img"
MNT="${WORK}/mnt"

if [[ ! -x "$BIN" && ! -f "$BIN" ]]; then
  echo "error: missing binary $BIN" >&2
  echo "Build first:" >&2
  echo "  ./scripts/build-release.sh   # Docker cross-compile aarch64" >&2
  exit 1
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: SD image assembly needs Linux (loop mount)." >&2
  echo "On macOS: prefer the wrapper (copies sources into the container — more reliable" >&2
  echo "than bind-mounting external volumes under OrbStack):" >&2
  echo "  ./deploy/image/build-sd-image.macos.sh" >&2
  echo "Or manual privileged Docker:" >&2
  echo "  docker run --privileged --rm -v \"\$PWD\":/src -w /src \\" >&2
  echo "    debian:bookworm-slim bash -c 'apt-get update && apt-get install -y curl xz-utils mount util-linux e2fsprogs ca-certificates kpartx cloud-guest-utils && ./deploy/image/build-sd-image.sh'" >&2
  exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
  echo "re-exec with sudo..."
  exec sudo -E "$0" "$@"
fi

mkdir -p "$WORK" "$DIST"
rm -rf "$MNT"
mkdir -p "$MNT"

if [[ ! -f "$IMG" ]]; then
  echo "==> downloading Pi OS Lite"
  curl -fL --retry 3 -o "$IMG_XZ" "$IMAGE_URL"
  echo "==> decompressing"
  xz -T0 -dkf "$IMG_XZ"
  # xz -d may leave base.img or name derived from URL
  if [[ ! -f "$IMG" ]]; then
    found="$(find "$WORK" -maxdepth 1 -name '*.img' ! -name 'vinyl-*' | head -1 || true)"
    if [[ -n "$found" ]]; then
      mv "$found" "$IMG"
    else
      echo "error: no .img after decompress" >&2
      exit 1
    fi
  fi
fi

# Grow image by 256 MiB so installs always fit (optional spare).
echo "==> ensuring free space in image"
# Only grow once per working copy (marker avoids re-grow on re-run).
if [[ ! -f "${WORK}/.grown" ]]; then
  truncate -s +256M "$IMG" || true
  touch "${WORK}/.grown"
fi

echo "==> loop-mounting"
# -P (partscan) is best-effort: Docker Desktop / OrbStack often omit loopNp*
# nodes. Fall back to kpartx (/dev/mapper/loopNp*) or offset-based losetup.
USE_KPARTX=0
BOOT_LOOP=""
ROOT_LOOP=""
LOOP="$(losetup -fP --show "$IMG" 2>/dev/null || losetup -f --show "$IMG")"

resolve_parts() {
  BOOT_PART=""
  ROOT_PART=""
  if [[ -b "${LOOP}p1" && -b "${LOOP}p2" ]]; then
    BOOT_PART="${LOOP}p1"
    ROOT_PART="${LOOP}p2"
    return 0
  fi
  if command -v kpartx >/dev/null 2>&1; then
    kpartx -avs "$LOOP" >/dev/null
    local base
    base="$(basename "$LOOP")"
    if [[ -b "/dev/mapper/${base}p1" && -b "/dev/mapper/${base}p2" ]]; then
      BOOT_PART="/dev/mapper/${base}p1"
      ROOT_PART="/dev/mapper/${base}p2"
      USE_KPARTX=1
      return 0
    fi
  fi
  # Offset loop devices via partition table (no kpartx / no partscan nodes).
  local boot_start boot_size root_start root_size
  boot_start="$(partx -g -o START -n 1 "$LOOP" | tr -d ' ')"
  boot_size="$(partx -g -o SECTORS -n 1 "$LOOP" | tr -d ' ')"
  root_start="$(partx -g -o START -n 2 "$LOOP" | tr -d ' ')"
  root_size="$(partx -g -o SECTORS -n 2 "$LOOP" | tr -d ' ')"
  if [[ -z "$boot_start" || -z "$root_start" ]]; then
    return 1
  fi
  BOOT_LOOP="$(losetup -f --show -o $((boot_start * 512)) --sizelimit $((boot_size * 512)) "$IMG")"
  ROOT_LOOP="$(losetup -f --show -o $((root_start * 512)) --sizelimit $((root_size * 512)) "$IMG")"
  BOOT_PART="$BOOT_LOOP"
  ROOT_PART="$ROOT_LOOP"
  return 0
}

cleanup() {
  set +e
  sync
  umount "${MNT}/boot/firmware" 2>/dev/null
  umount "${MNT}/boot" 2>/dev/null
  umount "$MNT" 2>/dev/null
  if [[ "${USE_KPARTX}" -eq 1 && -n "${LOOP:-}" ]]; then
    kpartx -d "$LOOP" 2>/dev/null
  fi
  [[ -n "${BOOT_LOOP:-}" ]] && losetup -d "$BOOT_LOOP" 2>/dev/null
  [[ -n "${ROOT_LOOP:-}" ]] && losetup -d "$ROOT_LOOP" 2>/dev/null
  [[ -n "${LOOP:-}" ]] && losetup -d "$LOOP" 2>/dev/null
}
trap cleanup EXIT

if ! resolve_parts; then
  echo "error: could not map boot/root partitions on ${LOOP}" >&2
  echo "hint: install kpartx (apt install kpartx) inside the build container" >&2
  ls -la "${LOOP}"* /dev/mapper 2>/dev/null || true
  fdisk -l "$IMG" 2>/dev/null || true
  exit 1
fi
echo "    boot=${BOOT_PART}  root=${ROOT_PART}"

# Grow root partition to fill image (best-effort; needs whole-disk LOOP).
if command -v growpart >/dev/null 2>&1 && [[ -n "$LOOP" ]]; then
  growpart "$LOOP" 2 || true
  # Remap after growpart so ROOT_PART sees the new size.
  if [[ "${USE_KPARTX}" -eq 1 ]]; then
    kpartx -d "$LOOP" 2>/dev/null || true
    USE_KPARTX=0
  fi
  [[ -n "${BOOT_LOOP:-}" ]] && losetup -d "$BOOT_LOOP" 2>/dev/null
  [[ -n "${ROOT_LOOP:-}" ]] && losetup -d "$ROOT_LOOP" 2>/dev/null
  BOOT_LOOP=""
  ROOT_LOOP=""
  # Refresh partition table on the loop device
  partx -u "$LOOP" 2>/dev/null || true
  if ! resolve_parts; then
    echo "error: re-resolve partitions after growpart failed" >&2
    exit 1
  fi
  echo "    after grow: boot=${BOOT_PART}  root=${ROOT_PART}"
fi
e2fsck -fy "$ROOT_PART" || true
resize2fs "$ROOT_PART" || true

mount "$ROOT_PART" "$MNT"
# Bookworm: boot firmware on /boot/firmware; older: /boot
if [[ -d "${MNT}/boot/firmware" ]]; then
  mount "$BOOT_PART" "${MNT}/boot/firmware"
  BOOT_MNT="${MNT}/boot/firmware"
else
  mount "$BOOT_PART" "${MNT}/boot"
  BOOT_MNT="${MNT}/boot"
fi

echo "==> installing vinyl-bridge-pi"
install -d "${MNT}/usr/local/bin"
install -d "${MNT}/usr/local/share/vinyl-bridge"
install -d "${MNT}/etc/vinyl-bridge"
install -d "${MNT}/var/lib/vinyl-bridge/rec"
install -d "${MNT}/etc/systemd/system"
install -m 0755 "$BIN" "${MNT}/usr/local/bin/vinyl-bridge-pi"
install -m 0644 "${ROOT}/config.example.toml" "${MNT}/usr/local/share/vinyl-bridge/config.example.toml"
install -m 0644 "${ROOT}/config.example.toml" "${MNT}/etc/vinyl-bridge/config.toml"
install -m 0644 "${ROOT}/deploy/systemd/vinyl-bridge.service" \
  "${MNT}/etc/systemd/system/vinyl-bridge.service"
install -m 0755 "${ROOT}/deploy/image/first-boot.sh" \
  "${MNT}/usr/local/share/vinyl-bridge/first-boot.sh"

# Enable service via systemd wants (works offline without systemctl).
mkdir -p "${MNT}/etc/systemd/system/multi-user.target.wants"
ln -sfn /etc/systemd/system/vinyl-bridge.service \
  "${MNT}/etc/systemd/system/multi-user.target.wants/vinyl-bridge.service"

# Hostname + mDNS friendly name
echo "vinyl-bridge" > "${MNT}/etc/hostname"
if [[ -f "${MNT}/etc/hosts" ]]; then
  if ! grep -qE '[[:space:]]vinyl-bridge([[:space:]]|$)' "${MNT}/etc/hosts"; then
    # Pi OS typically maps hostname via 127.0.1.1
    echo "127.0.1.1	vinyl-bridge" >> "${MNT}/etc/hosts" || true
  fi
fi

# Enable SSH for headless bring-up (empty file on boot partition).
touch "${BOOT_MNT}/ssh"

# Optional: userconf for first boot — set VINYL_PI_USERHASH to a passwd hash
# (openssl passwd -6) if you want a default user without raspi-config.
if [[ -n "${VINYL_PI_USERHASH:-}" ]]; then
  # userconf format: username:hash
  echo "vinyl:${VINYL_PI_USERHASH}" > "${BOOT_MNT}/userconf.txt"
fi

# Marker (binary --version needs guest libs; fall back to sha256 of installed ELF)
BIN_VER="$("${BIN}" --version 2>/dev/null || true)"
if [[ -z "$BIN_VER" ]]; then
  BIN_VER="sha256:$(sha256sum "$BIN" 2>/dev/null | awk '{print $1}' || echo unknown)"
fi
cat > "${MNT}/etc/vinyl-bridge/IMAGE_BUILD.txt" <<EOF
vinyl-bridge-pi SD image
built_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)
binary_version=${BIN_VER}
base_image_url=${IMAGE_URL}
EOF

sync
cleanup
trap - EXIT

echo "==> copying image to ${OUT_IMG}"
cp -f "$IMG" "$OUT_IMG"
echo "==> compressing ${OUT_XZ}"
xz -T0 -f -k "$OUT_IMG"
ls -lh "$OUT_IMG" "$OUT_XZ"
echo
echo "Flash with:"
echo "  # macOS / Linux"
echo "  xzcat ${OUT_XZ} | sudo dd of=/dev/rdiskN bs=4m status=progress"
echo "  # or Raspberry Pi Imager → Use custom → ${OUT_IMG}"
echo
echo "Done."
