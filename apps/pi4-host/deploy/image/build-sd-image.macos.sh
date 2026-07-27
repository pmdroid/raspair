#!/usr/bin/env bash
# =============================================================================
# DEPRECATED — Raspberry Pi OS Lite inject via Docker.
# Prefer: ./deploy/buildroot/build.macos.sh  (or `make image`)
# =============================================================================
#
# macOS entrypoint: assemble vinyl-bridge-pi4.img via privileged Docker.
#
# Why not plain bind-mount of apps/pi4-host?
#   OrbStack + external volumes (e.g. /Volumes/Data) often return EINTR on
#   exec and omit loopNp* nodes. We tar sources into the container FS, build
#   there, then copy artifacts out to IMAGE_OUT (default: ~/.cache/...).
#
# Prerequisites: Docker running, dist/vinyl-bridge-pi already built
#   (make build-pi).
#
# Usage:
#   ./deploy/image/build-sd-image.macos.sh
#   IMAGE_OUT=/path/to/dir ./deploy/image/build-sd-image.macos.sh
set -euo pipefail

echo "WARNING: deploy/image/* is DEPRECATED. Prefer Buildroot: make image" >&2

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIST="${ROOT}/dist"
BIN="${DIST}/vinyl-bridge-pi"
IMAGE_OUT="${IMAGE_OUT:-${HOME}/.cache/vinyl-bridge-pi-images}"
CTR="vinyl-sd-build-$$"

if [[ ! -f "$BIN" ]]; then
  echo "error: missing $BIN — run: make build-pi" >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker not found" >&2
  exit 1
fi

mkdir -p "$IMAGE_OUT"

cleanup_ctr() {
  docker rm -f "$CTR" >/dev/null 2>&1 || true
}
trap cleanup_ctr EXIT

echo "==> starting privileged build container ($CTR)"
docker run --privileged --name "$CTR" -d \
  -v "$IMAGE_OUT":/out \
  debian:bookworm-slim \
  sleep 7200 >/dev/null

echo "==> streaming sources into container"
tar -C "$ROOT" -cf - \
  deploy/image/build-sd-image.sh \
  deploy/image/first-boot.sh \
  deploy/systemd/vinyl-bridge.service \
  config.example.toml \
  dist/vinyl-bridge-pi \
  | docker exec -i "$CTR" tar -xf - -C /tmp

docker exec "$CTR" bash -c 'set -euo pipefail
  mkdir -p /src/apps/pi4-host/dist
  rm -rf /src/apps/pi4-host/deploy
  mv /tmp/deploy /src/apps/pi4-host/
  mv /tmp/config.example.toml /src/apps/pi4-host/
  mv /tmp/dist/vinyl-bridge-pi /src/apps/pi4-host/dist/
  chmod +x /src/apps/pi4-host/deploy/image/build-sd-image.sh \
           /src/apps/pi4-host/deploy/image/first-boot.sh \
           /src/apps/pi4-host/dist/vinyl-bridge-pi
'

echo "==> apt + build-sd-image.sh (download + customize + xz)"
docker exec "$CTR" bash -c 'set -euo pipefail
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq \
    curl xz-utils mount util-linux e2fsprogs ca-certificates \
    kpartx cloud-guest-utils parted fdisk
  cd /src/apps/pi4-host
  ./deploy/image/build-sd-image.sh
  ls -lh dist/vinyl-bridge-pi4.img dist/vinyl-bridge-pi4.img.xz
'

# docker cp is more reliable than bind-mount cp to macOS (avoids "error deallocating").
echo "==> copying artifacts out via docker cp → ${IMAGE_OUT}"
docker cp "$CTR:/src/apps/pi4-host/dist/vinyl-bridge-pi4.img.xz" \
  "${IMAGE_OUT}/vinyl-bridge-pi4.img.xz"
docker cp "$CTR:/src/apps/pi4-host/dist/vinyl-bridge-pi4.img" \
  "${IMAGE_OUT}/vinyl-bridge-pi4.img"
(
  cd "$IMAGE_OUT"
  shasum -a 256 vinyl-bridge-pi4.img vinyl-bridge-pi4.img.xz > SHA256SUMS
  ls -lh vinyl-bridge-pi4.img vinyl-bridge-pi4.img.xz SHA256SUMS
)

# Symlink into dist/ for local convenience (artifacts stay gitignored).
mkdir -p "$DIST"
ln -sfn "${IMAGE_OUT}/vinyl-bridge-pi4.img" "${DIST}/vinyl-bridge-pi4.img"
ln -sfn "${IMAGE_OUT}/vinyl-bridge-pi4.img.xz" "${DIST}/vinyl-bridge-pi4.img.xz"
ln -sfn "${IMAGE_OUT}/SHA256SUMS" "${DIST}/SHA256SUMS" 2>/dev/null || true

echo
echo "Artifacts (absolute):"
echo "  ${IMAGE_OUT}/vinyl-bridge-pi4.img"
echo "  ${IMAGE_OUT}/vinyl-bridge-pi4.img.xz"
echo "  ${IMAGE_OUT}/SHA256SUMS"
echo "Symlinks: ${DIST}/vinyl-bridge-pi4.img{,.xz}"
echo
echo "Flash (macOS, replace rdiskN after diskutil list):"
echo "  xzcat ${IMAGE_OUT}/vinyl-bridge-pi4.img.xz | sudo dd of=/dev/rdiskN bs=4m status=progress"
echo "  # or Raspberry Pi Imager → Use custom → ${IMAGE_OUT}/vinyl-bridge-pi4.img"
