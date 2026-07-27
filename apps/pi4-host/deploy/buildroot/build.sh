#!/usr/bin/env bash
# Build the Vinyl Bridge Buildroot appliance image for Raspberry Pi 4 (64-bit).
#
# Produces flashable sdcard.img (copied to IMAGE_OUT as vinyl-bridge-pi4.img).
# Does NOT use Raspberry Pi OS — pure Buildroot + busybox init.
#
# Requirements (Linux host, or macOS via build.macos.sh / Docker):
#   - make, gcc, g++, rsync, cpio, unzip, bc, wget/curl, git, python3
#   - prebuilt aarch64 binary: apps/pi4-host/dist/vinyl-bridge-pi
#     (make build-pi)
#
# Usage:
#   ./deploy/buildroot/build.sh
#   IMAGE_OUT=~/images JOBS=8 ./deploy/buildroot/build.sh
#   BUILDROOT_VERSION=2024.02.9 ./deploy/buildroot/build.sh
#
# Caches (never commit):
#   $BUILDROOT_CACHE/buildroot-$VER   source tree
#   $BUILDROOT_CACHE/dl               package downloads
#   $BUILDROOT_CACHE/output           build output (O=)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EXTERNAL="${ROOT}/buildroot"
DIST="${ROOT}/dist"
BIN="${DIST}/vinyl-bridge-pi"
STAGE_BIN="${EXTERNAL}/package/vinyl-bridge-pi/bin/vinyl-bridge-pi"

BUILDROOT_VERSION="${BUILDROOT_VERSION:-2024.02.9}"
BUILDROOT_CACHE="${BUILDROOT_CACHE:-${HOME}/.cache/vinyl-bridge-buildroot}"
IMAGE_OUT="${IMAGE_OUT:-${HOME}/.cache/vinyl-bridge-pi-images}"
JOBS="${JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"

BR_DIR="${BUILDROOT_CACHE}/buildroot-${BUILDROOT_VERSION}"
DL_DIR="${BUILDROOT_CACHE}/dl"
OUTPUT_DIR="${BUILDROOT_CACHE}/output-vinyl-rpi4-64"
BR_URL="https://buildroot.org/downloads/buildroot-${BUILDROOT_VERSION}.tar.xz"
# GitHub mirror fallback (release tags)
BR_URL_GH="https://github.com/buildroot/buildroot/archive/refs/tags/${BUILDROOT_VERSION}.tar.gz"

log() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

if [[ "$(uname -s)" != "Linux" ]]; then
  die "Buildroot must run on Linux. On macOS use: ./deploy/buildroot/build.macos.sh"
fi

[[ -d "$EXTERNAL" ]] || die "missing br2-external tree at $EXTERNAL"
[[ -f "$EXTERNAL/external.desc" ]] || die "missing $EXTERNAL/external.desc"
[[ -f "$BIN" ]] || die "missing $BIN — run: make build-pi  (from apps/pi4-host)"

# Stage binary for the Buildroot package (gitignored path under bin/)
mkdir -p "$(dirname "$STAGE_BIN")"
cp -f "$BIN" "$STAGE_BIN"
chmod 0755 "$STAGE_BIN"
log "staged prebuilt binary → $STAGE_BIN ($(wc -c <"$STAGE_BIN") bytes)"

mkdir -p "$BUILDROOT_CACHE" "$DL_DIR" "$IMAGE_OUT" "$DIST"

# --- fetch Buildroot source (pinned release) ---
if [[ ! -f "${BR_DIR}/Makefile" ]]; then
  log "fetching Buildroot ${BUILDROOT_VERSION}"
  tmp="${BUILDROOT_CACHE}/br-fetch-$$"
  mkdir -p "$tmp"
  if curl -fL --retry 3 -o "${tmp}/br.tar.xz" "$BR_URL"; then
    tar -C "$tmp" -xf "${tmp}/br.tar.xz"
    # tarball extracts as buildroot-VERSION/
    src="$(find "$tmp" -maxdepth 1 -type d -name 'buildroot-*' | head -1)"
  else
    log "official tarball failed; trying GitHub tag archive"
    curl -fL --retry 3 -o "${tmp}/br.tar.gz" "$BR_URL_GH"
    tar -C "$tmp" -xzf "${tmp}/br.tar.gz"
    src="$(find "$tmp" -maxdepth 1 -type d -name 'buildroot-*' | head -1)"
  fi
  [[ -n "${src:-}" && -f "${src}/Makefile" ]] || die "failed to extract Buildroot"
  rm -rf "$BR_DIR"
  mv "$src" "$BR_DIR"
  rm -rf "$tmp"
fi
log "Buildroot tree: $BR_DIR"

# Ensure post-build is executable
chmod +x "${EXTERNAL}/board/vinyl-rpi4/post-build.sh" \
  "${EXTERNAL}/board/vinyl-rpi4/rootfs_overlay/etc/init.d/S99vinyl-bridge"

# --- configure ---
log "defconfig vinyl_rpi4_64 (BR2_EXTERNAL=$EXTERNAL)"
make -C "$BR_DIR" \
  O="$OUTPUT_DIR" \
  BR2_EXTERNAL="$EXTERNAL" \
  BR2_DL_DIR="$DL_DIR" \
  vinyl_rpi4_64_defconfig

# --- build ---
log "building (jobs=$JOBS) — first run downloads toolchain/kernel and can take 30–90+ min"
make -C "$BR_DIR" \
  O="$OUTPUT_DIR" \
  BR2_EXTERNAL="$EXTERNAL" \
  BR2_DL_DIR="$DL_DIR" \
  -j"$JOBS" \
  all

SDCARD="${OUTPUT_DIR}/images/sdcard.img"
[[ -f "$SDCARD" ]] || die "expected $SDCARD after build — check Buildroot log"

OUT_IMG="${IMAGE_OUT}/vinyl-bridge-pi4.img"
OUT_XZ="${OUT_IMG}.xz"
log "copying image → $OUT_IMG"
cp -f "$SDCARD" "$OUT_IMG"

log "compressing → $OUT_XZ"
# Prefer multi-thread xz when available
if xz -T0 -f -k -c "$OUT_IMG" >"${OUT_XZ}.tmp" 2>/dev/null; then
  mv -f "${OUT_XZ}.tmp" "$OUT_XZ"
else
  xz -f -k -c "$OUT_IMG" >"$OUT_XZ"
fi

(
  cd "$IMAGE_OUT"
  sha256sum "vinyl-bridge-pi4.img" "vinyl-bridge-pi4.img.xz" >SHA256SUMS 2>/dev/null \
    || shasum -a 256 "vinyl-bridge-pi4.img" "vinyl-bridge-pi4.img.xz" >SHA256SUMS
)

# Convenience symlinks under dist/ (gitignored)
ln -sfn "$OUT_IMG" "${DIST}/vinyl-bridge-pi4.img"
ln -sfn "$OUT_XZ" "${DIST}/vinyl-bridge-pi4.img.xz"

IMG_BYTES=$(wc -c <"$OUT_IMG" | tr -d ' ')
XZ_BYTES=$(wc -c <"$OUT_XZ" | tr -d ' ')
log "PASS Buildroot appliance image"
echo "  raw:  $OUT_IMG  (${IMG_BYTES} bytes)"
echo "  xz:   $OUT_XZ   (${XZ_BYTES} bytes)"
echo "  also: ${OUTPUT_DIR}/images/sdcard.img"
echo "  flash: see apps/pi4-host/buildroot/README.md"
