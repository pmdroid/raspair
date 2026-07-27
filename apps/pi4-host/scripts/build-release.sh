#!/usr/bin/env bash
# Produce dist/vinyl-bridge-pi (linux aarch64 + ALSA) for SD image / QEMU.
# macOS daily loop: use `make build-macos` / `just build-macos` instead.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
mkdir -p dist

log() { printf '==> %s\n' "$*"; }

if [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "aarch64" ]]; then
  log "native aarch64 Linux build (+alsa)"
  cargo build --release --no-default-features --features alsa
  cp -f target/release/vinyl-bridge-pi dist/vinyl-bridge-pi
  strip dist/vinyl-bridge-pi || true
elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  log "Docker linux/arm64 native build (+alsa) with volume caches"
  # Prefer platform arm64 (OrbStack on Apple Silicon) over slow bind-mount cross.
  docker run --rm --platform linux/arm64 \
    -v "${ROOT}/Cargo.toml:/src/Cargo.toml:ro" \
    -v "${ROOT}/Cargo.lock:/src/Cargo.lock:ro" \
    -v "${ROOT}/src:/src/src:ro" \
    -v vinyl-pi-cargo-registry:/usr/local/cargo/registry \
    -v vinyl-pi-cargo-git:/usr/local/cargo/git \
    -v vinyl-pi-target:/target \
    -e CARGO_TARGET_DIR=/target \
    -w /src \
    rust:1-bookworm \
    bash -c 'set -euo pipefail
      apt-get update -qq
      DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libasound2-dev pkg-config >/dev/null
      # Cargo.toml lives at /src; create a minimal package dir
      mkdir -p /build/src
      cp /src/Cargo.toml /src/Cargo.lock /build/
      cp -a /src/src/. /build/src/
      cd /build
      cargo build --release --no-default-features --features alsa
      strip /target/release/vinyl-bridge-pi || true
      file /target/release/vinyl-bridge-pi
      ls -lh /target/release/vinyl-bridge-pi
    '
  docker run --rm \
    -v vinyl-pi-target:/target \
    -v "${ROOT}/dist":/out \
    alpine \
    cp /target/release/vinyl-bridge-pi /out/vinyl-bridge-pi
else
  echo "error: need aarch64 Linux cargo, or Docker with linux/arm64" >&2
  exit 1
fi

file dist/vinyl-bridge-pi || true
ls -lh dist/vinyl-bridge-pi
echo "OK: dist/vinyl-bridge-pi (Pi deploy artifact)"
