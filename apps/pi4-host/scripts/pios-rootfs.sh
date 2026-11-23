#!/usr/bin/env bash
set -euo pipefail
PATH="$PATH:/usr/sbin:/sbin"
DEST="${1:?usage: pios-rootfs.sh DEST [CACHE_DIR]}"
CACHE="${2:-${PIOS_CACHE:-$HOME/.cache/raspair-pios}}"
IMAGE=2026-10-06-raspios-trixie-arm64-lite
URL="https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/$IMAGE.img.xz"
SHA256=483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843

mkdir -p "$CACHE"
xz="$CACHE/$IMAGE.img.xz"
if ! echo "$SHA256  $xz" | sha256sum -c --status 2>/dev/null; then
  curl -fL --retry 3 -o "$xz.partial" "$URL"
  mv "$xz.partial" "$xz"
  echo "$SHA256  $xz" | sha256sum -c
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
xz -dc "$xz" >"$work/image.img"
read -r start sectors < <(partx -g -o START,SECTORS -n 2 "$work/image.img")
dd if="$work/image.img" of="$work/root.ext4" bs=4M iflag=skip_bytes,count_bytes \
  skip=$((start * 512)) count=$((sectors * 512)) status=none
rm -f "$work/image.img"
rm -rf "$DEST"
mkdir -p "$DEST"
debugfs -R "rdump / $DEST" "$work/root.ext4" 2>/dev/null
[[ -e "$DEST/etc/os-release" ]] || { echo "error: rootfs extraction failed" >&2; exit 1; }
echo "rootfs: $IMAGE ($(. "$DEST/etc/os-release" && echo "$PRETTY_NAME")) at $DEST"
