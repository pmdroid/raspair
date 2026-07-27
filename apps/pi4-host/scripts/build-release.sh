#!/usr/bin/env bash
# Produce dist/vinyl-bridge-pi (linux aarch64) for SD image assembly.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
mkdir -p dist

if [[ "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]] && [[ "$(uname -s)" == "Linux" ]]; then
  echo "==> native aarch64 Linux build"
  cargo build --release --features alsa
  cp -f target/release/vinyl-bridge-pi dist/vinyl-bridge-pi
  strip dist/vinyl-bridge-pi || true
else
  if ! command -v docker >/dev/null 2>&1; then
    echo "error: Docker required to cross-compile aarch64 from $(uname -s)/$(uname -m)" >&2
    echo "Install Docker, or build on a Pi / aarch64 Linux host." >&2
    exit 1
  fi
  echo "==> Docker cross-compile aarch64-unknown-linux-gnu (+alsa)"
  docker build -f deploy/Dockerfile.cross -t vinyl-pi-cross .
  docker run --rm -v "$ROOT":/src -w /src vinyl-pi-cross
fi

./dist/vinyl-bridge-pi --version 2>/dev/null || file dist/vinyl-bridge-pi
ls -lh dist/vinyl-bridge-pi
echo "OK: dist/vinyl-bridge-pi"
