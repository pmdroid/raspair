#!/usr/bin/env bash
# macOS entrypoint: run Buildroot inside a Linux container.
#
# Why Docker: Buildroot does not support macOS as a build host.
# Why Docker named volumes for /cache: macOS APFS is usually *case-insensitive*.
#   glibc creates both stamp.os and stamp.oS — they collide on APFS bind mounts
#   and the toolchain fails with "No rule to make target .../stdlib/stamp.os".
#   Linux Docker volumes are case-sensitive ext4 and avoid that.
# Why stream sources via tar: OrbStack + /Volumes/Data bind mounts often break.
#
# Prerequisites:
#   - Docker running
#   - dist/vinyl-bridge-pi already built (make build-pi)
#
# Usage:
#   ./deploy/buildroot/build.macos.sh
#   IMAGE_OUT=… JOBS=8 ./deploy/buildroot/build.macos.sh
#
# Cache volume: vinyl-buildroot-cache (docker volume)
# Final images are copied out to IMAGE_OUT on the host (case-insensitive OK).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIST="${ROOT}/dist"
BIN="${DIST}/vinyl-bridge-pi"
IMAGE_OUT="${IMAGE_OUT:-${HOME}/.cache/vinyl-bridge-pi-images}"
BUILDROOT_VERSION="${BUILDROOT_VERSION:-2024.02.9}"
JOBS="${JOBS:-$(sysctl -n hw.ncpu 2>/dev/null || echo 4)}"
CACHE_VOL="${BUILDROOT_CACHE_VOL:-vinyl-buildroot-cache}"
CTR="vinyl-br-build-$$"

if [[ ! -f "$BIN" ]]; then
  echo "error: missing $BIN — run: make build-pi" >&2
  exit 1
fi
if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker not found" >&2
  exit 1
fi

mkdir -p "$IMAGE_OUT"
docker volume create "$CACHE_VOL" >/dev/null

cleanup_ctr() {
  docker rm -f "$CTR" >/dev/null 2>&1 || true
}
trap cleanup_ctr EXIT

echo "==> starting Linux build container ($CTR)"
echo "    cache volume: $CACHE_VOL  (case-sensitive; do NOT bind-mount macOS APFS for O=)"
echo "    image out:    $IMAGE_OUT"
# IMAGE_OUT stays on the host (final .img only). Build tree stays in Docker volume.
docker run --name "$CTR" -d \
  -v "${CACHE_VOL}:/cache" \
  -v "${IMAGE_OUT}:/out" \
  debian:bookworm-slim \
  sleep 86400 >/dev/null

echo "==> installing Buildroot host dependencies"
docker exec "$CTR" bash -c 'set -euo pipefail
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends \
    build-essential bc bison flex gawk gettext git curl ca-certificates \
    wget rsync cpio unzip file python3 python3-setuptools \
    libncurses5-dev libssl-dev \
    xz-utils bzip2 gzip tar patch diffutils findutils \
    util-linux e2fsprogs dosfstools mtools \
    >/dev/null
'

echo "==> streaming br2-external + binary + build scripts into container"
tar -C "$ROOT" -cf - \
  buildroot \
  deploy/buildroot/build.sh \
  dist/vinyl-bridge-pi \
  | docker exec -i "$CTR" bash -c 'mkdir -p /work && tar -xf - -C /work'

echo "==> running Buildroot (this can take a long time on first build)"
docker exec -e BUILDROOT_VERSION="$BUILDROOT_VERSION" \
  -e BUILDROOT_CACHE=/cache \
  -e IMAGE_OUT=/out \
  -e JOBS="$JOBS" \
  -e HOME=/root \
  "$CTR" bash -c 'set -euo pipefail
    cd /work
    chmod +x deploy/buildroot/build.sh
    ./deploy/buildroot/build.sh
  '

echo "==> Buildroot image ready under $IMAGE_OUT"
ls -lh "$IMAGE_OUT"/vinyl-bridge-pi4.img* 2>/dev/null || true
ln -sfn "${IMAGE_OUT}/vinyl-bridge-pi4.img" "${DIST}/vinyl-bridge-pi4.img"
ln -sfn "${IMAGE_OUT}/vinyl-bridge-pi4.img.xz" "${DIST}/vinyl-bridge-pi4.img.xz" 2>/dev/null || true
echo "OK: ${IMAGE_OUT}/vinyl-bridge-pi4.img"
