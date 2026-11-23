#!/usr/bin/env bash
# Produce dist/vinyl-bridge-pi (linux aarch64 + ALSA) for SD image / QEMU.
# macOS daily loop: use `make build-macos` / `just build-macos` instead.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKSPACE="$(cd "$ROOT/../.." && pwd)"
cd "$ROOT"
mkdir -p dist

log() { printf '==> %s\n' "$*"; }

TARGET=aarch64-unknown-linux-gnu
BUILDER_IMAGE="${BUILDER_IMAGE:-rust:1.98.1-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
NOTICES=dist/THIRD-PARTY-NOTICES.txt
VOLUMES="${DOCKER_VOLUME_PREFIX:-vinyl-pi}"
if [[ -f "$WORKSPACE/SOURCE_COMMIT" ]]; then
  COMMIT="$(cat "$WORKSPACE/SOURCE_COMMIT")"
  SOURCE_ARCHIVE="$(realpath "${RASPAIR_SOURCE_ARCHIVE:?building from a source package needs RASPAIR_SOURCE_ARCHIVE}")"
  [[ "$(tar -xOzf "$SOURCE_ARCHIVE" "raspair-$VERSION/SOURCE_COMMIT")" == "$COMMIT" ]] \
    || { echo "error: $SOURCE_ARCHIVE is not the source package of $COMMIT" >&2; exit 1; }
else
  COMMIT="$(git -C "$WORKSPACE" rev-parse HEAD)"
fi
MODE="${RASPAIR_BUILDER:-}"
if [[ -z "$MODE" ]]; then
  if [[ "${CARGO_BUILD_TARGET:-}" == "$TARGET" ]]; then MODE=cross
  elif [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "aarch64" ]]; then MODE=native
  elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then MODE=docker
  fi
fi
rm -f dist/vinyl-bridge-pi dist/vinyl-bridge-pi.* dist/SHA256SUMS dist/BUILDINFO dist/*-source.tar.gz

log "locked web build (apps/web)"
(cd "$WORKSPACE/apps/web" && bun install --frozen-lockfile && bun run build)

log "license notices and dependency manifest"
python3 -I scripts/notices.py "$WORKSPACE" "$WORKSPACE/apps/web" "$NOTICES" dist/vinyl-bridge-pi.manifest.json "$TARGET"
export RASPAIR_RELEASE=1 RASPAIR_WEB_DIST="$WORKSPACE/apps/web/dist" RASPAIR_LICENSES="$ROOT/$NOTICES"

if [[ "$MODE" == cross ]]; then
  log "cross build for $TARGET with host cargo (linker and sysroot from the environment)"
  BUILDER="host cargo cross $(rustc --version)"
  cargo build --locked -p vinyl-bridge-pi --release --no-default-features --features alsa
  cp -f "${CARGO_TARGET_DIR:-$WORKSPACE/target}/$TARGET/release/vinyl-bridge-pi" dist/vinyl-bridge-pi
elif [[ "$MODE" == native ]]; then
  log "native aarch64 Linux build (+alsa)"
  BUILDER="native $(rustc --version)"
  cargo build --locked -p vinyl-bridge-pi --release --no-default-features --features alsa
  cp -f "${CARGO_TARGET_DIR:-$WORKSPACE/target}"/release/vinyl-bridge-pi dist/vinyl-bridge-pi
  strip dist/vinyl-bridge-pi || true
elif [[ "$MODE" == docker ]]; then
  log "Docker linux/arm64 native build (+alsa) with airplay-core path dep"
  BUILDER="$BUILDER_IMAGE"
  # Stream monorepo slice via tar (avoids flaky bind mounts on external volumes).
  tar -C "$WORKSPACE" -cf - \
      Cargo.toml Cargo.lock apps/pi4-host/Cargo.toml apps/pi4-host/build.rs apps/pi4-host/LICENSES.dev.txt \
      apps/pi4-host/src apps/pi4-host/static "apps/pi4-host/$NOTICES" apps/web/dist \
      crates/airplay-core apps/airplay-rs/cli \
  | docker run --rm -i --platform linux/arm64 \
      -v "$VOLUMES-cargo-registry:/usr/local/cargo/registry" \
      -v "$VOLUMES-cargo-git:/usr/local/cargo/git" \
      -v "$VOLUMES-target:/target" \
      -e CARGO_TARGET_DIR=/target \
      -e RASPAIR_RELEASE=1 \
      -e RASPAIR_WEB_DIST=/build/apps/web/dist \
      -e "RASPAIR_LICENSES=/build/apps/pi4-host/$NOTICES" \
      -e CARGO_HOME=/usr/local/cargo \
      -e PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
      "$BUILDER_IMAGE" \
      bash -c 'set -euo pipefail
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libasound2-dev libssl-dev pkg-config >/dev/null
        mkdir -p /build
        tar -C /build -xf -
        cd /build
        cargo build --locked -p vinyl-bridge-pi --release --no-default-features --features alsa
        strip /target/release/vinyl-bridge-pi || true
        file /target/release/vinyl-bridge-pi
        ls -lh /target/release/vinyl-bridge-pi
      '

  log "copying binary out of docker volume (stdout pipe; avoids flaky bind mounts)"
  docker run --rm \
    -v "$VOLUMES-target:/target" \
    "$BUILDER_IMAGE" \
    cat /target/release/vinyl-bridge-pi >"${ROOT}/dist/vinyl-bridge-pi"
  chmod +x "${ROOT}/dist/vinyl-bridge-pi"
else
  echo "error: need aarch64 Linux cargo, or Docker with linux/arm64 (RASPAIR_BUILDER=${MODE:-unset})" >&2
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

log "ELF dependencies against the stock Raspberry Pi OS allowlist"
scripts/check-elf.sh dist/vinyl-bridge-pi ${PIOS_ROOTFS:+"$PIOS_ROOTFS"} | tee dist/vinyl-bridge-pi.elf.txt

log "source package and build record"
if [[ -n "${SOURCE_ARCHIVE:-}" ]]; then
  cp "$SOURCE_ARCHIVE" "dist/raspair-$VERSION-source.tar.gz"
else
  git -C "$WORKSPACE" diff --quiet HEAD || echo "warn: working tree differs from $COMMIT; source package is HEAD only" >&2
  git -C "$WORKSPACE" archive --format=tar.gz --prefix="raspair-$VERSION/" \
    --add-virtual-file="raspair-$VERSION/SOURCE_COMMIT:$COMMIT" -o "$ROOT/dist/raspair-$VERSION-source.tar.gz" HEAD
fi
{
  echo "version: $VERSION"
  echo "commit: $COMMIT"
  echo "target: $TARGET"
  echo "builder: $BUILDER"
  echo "bun: $(bun --version)"
  echo "build: RASPAIR_RELEASE=1 cargo build --locked -p vinyl-bridge-pi --release --no-default-features --features alsa"
} >dist/BUILDINFO
(cd dist && sha256sum vinyl-bridge-pi vinyl-bridge-pi.manifest.json vinyl-bridge-pi.elf.txt THIRD-PARTY-NOTICES.txt BUILDINFO "raspair-$VERSION-source.tar.gz" >SHA256SUMS)
cat dist/SHA256SUMS
echo "OK: dist/vinyl-bridge-pi (Pi deploy artifact)"
