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
  log "Docker linux/arm64 native build (+alsa)"
  # Stream sources via tar (avoids flaky bind mounts on external volumes).
  # Build artifacts stay in named volumes for cache.
  tar -C "$ROOT" -cf - Cargo.toml Cargo.lock src static \
  | docker run --rm -i --platform linux/arm64 \
      -v vinyl-pi-cargo-registry:/usr/local/cargo/registry \
      -v vinyl-pi-cargo-git:/usr/local/cargo/git \
      -v vinyl-pi-target:/target \
      -e CARGO_TARGET_DIR=/target \
      -e CARGO_HOME=/usr/local/cargo \
      -e PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      rust:1-bookworm \
      bash -c 'set -euo pipefail
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libasound2-dev pkg-config >/dev/null
        mkdir -p /build
        tar -C /build -xf -
        cd /build
        cargo build --release --no-default-features --features alsa
        strip /target/release/vinyl-bridge-pi || true
        file /target/release/vinyl-bridge-pi
        ls -lh /target/release/vinyl-bridge-pi
      '

  log "copying binary out of docker volume (stdout pipe; avoids flaky bind mounts)"
  # Writing via host redirect is more reliable than mounting dist/ from /Volumes.
  docker run --rm \
    -v vinyl-pi-target:/target \
    alpine \
    cat /target/release/vinyl-bridge-pi >"${ROOT}/dist/vinyl-bridge-pi"
  chmod +x "${ROOT}/dist/vinyl-bridge-pi"
else
  echo "error: need aarch64 Linux cargo, or Docker with linux/arm64" >&2
  exit 1
fi

file dist/vinyl-bridge-pi || true
ls -lh dist/vinyl-bridge-pi
# sanity: must be ELF aarch64, not Mach-O
if file dist/vinyl-bridge-pi | grep -qi 'Mach-O'; then
  echo "error: dist binary is Mach-O (macOS), expected Linux aarch64 ELF" >&2
  exit 1
fi
if ! file dist/vinyl-bridge-pi | grep -qiE 'ELF.*(aarch64|ARM aarch64|ARM64)'; then
  echo "warn: unexpected file(1) type — check manually" >&2
  file dist/vinyl-bridge-pi
fi
echo "OK: dist/vinyl-bridge-pi (Pi deploy artifact)"
