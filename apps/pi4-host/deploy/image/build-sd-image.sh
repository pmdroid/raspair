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
  echo "On macOS: run this script inside Docker:" >&2
  echo "  docker run --privileged --rm -v \"$ROOT\":/src -w /src/apps/pi4-host \\" >&2
  echo "    debian:bookworm-slim bash -c 'apt-get update && apt-get install -y curl xz-utils mount util-linux ca-certificates && ./deploy/image/build-sd-image.sh'" >&2
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
truncate -s +256M "$IMG" || true

echo "==> loop-mounting"
LOOP="$(losetup -fP --show "$IMG")"
cleanup() {
  set +e
  sync
  umount "${MNT}/boot" 2>/dev/null || umount "${MNT}/boot/firmware" 2>/dev/null
  umount "$MNT" 2>/dev/null
  losetup -d "$LOOP" 2>/dev/null
}
trap cleanup EXIT

# Partition layout: p1 boot (FAT), p2 root (ext4) on modern Pi OS.
BOOT_PART="${LOOP}p1"
ROOT_PART="${LOOP}p2"
if [[ ! -b "$ROOT_PART" ]]; then
  # Some kernels use /dev/loop0p2 vs loop0p2 naming — already covered by -P.
  echo "error: expected ${ROOT_PART}" >&2
  ls -la "${LOOP}"* || true
  exit 1
fi

# Grow root partition to fill image (best-effort).
if command -v growpart >/dev/null 2>&1; then
  growpart "$LOOP" 2 || true
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
  if ! grep -q 'vinyl-bridge' "${MNT}/etc/hosts"; then
    echo "10.0.0.1 vinyl-bridge" >> "${MNT}/etc/hosts" || true
    sed -i 's/10.0.0.1/127.0.1.1/' "${MNT}/etc/hosts" 2>/dev/null || true
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

# Marker
cat > "${MNT}/etc/vinyl-bridge/IMAGE_BUILD.txt" <<EOF
vinyl-bridge-pi SD image
built_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)
binary_version=$("${BIN}" --version 2>/dev/null || echo unknown)
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
