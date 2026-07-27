#!/usr/bin/env bash
# QEMU / aarch64 smoke tests for vinyl-bridge-pi.
#
# Levels:
#   binary  — run aarch64 ELF (Docker linux/arm64 preferred) + curl /api/status
#   virt    — boot Debian arm64 cloud on -M virt, install binary via 9p, enable
#             systemd, curl host-forwarded HTTP. Proves boot+systemd on aarch64.
#   image   — boot dist/vinyl-bridge-pi4.img with -M raspi4b if available, else
#             document fallback. Full SD path when the image exists.
#   all     — binary + virt + image (image skipped if missing)
#   default — binary then virt
#
# Usage:
#   ./scripts/qemu-smoke.sh
#   ./scripts/qemu-smoke.sh binary|virt|image|all
#
# Env: HTTP_PORT (18080), TIMEOUT_S (240), QEMU_SYSTEM, QEMU_USER
#
# Audio line-in / Wi‑Fi RF are NOT validated in QEMU.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="${ROOT}/dist"
BIN="${DIST}/vinyl-bridge-pi"
LEVEL="${1:-default}"
HTTP_PORT="${HTTP_PORT:-18080}"
TIMEOUT_S="${TIMEOUT_S:-240}"
WORK="${DIST}/qemu-work"
mkdir -p "$DIST" "$WORK"

log() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

find_qemu_system() {
  if [[ -n "${QEMU_SYSTEM:-}" && -x "${QEMU_SYSTEM}" ]]; then
    echo "$QEMU_SYSTEM"; return
  fi
  for c in qemu-system-aarch64 /opt/homebrew/bin/qemu-system-aarch64 \
           /usr/local/bin/qemu-system-aarch64 /usr/bin/qemu-system-aarch64; do
    if [[ -x "$c" ]] || command -v "$c" >/dev/null 2>&1; then
      command -v "$c" 2>/dev/null || echo "$c"
      return
    fi
  done
  return 1
}

machine_has() {
  "$1" -machine help 2>/dev/null | awk '{print $1}' | grep -qx "$2"
}

ensure_binary() {
  if [[ -f "$BIN" ]]; then
    file "$BIN" 2>/dev/null || ls -la "$BIN"
    return
  fi
  log "building aarch64 binary via scripts/build-release.sh"
  command -v docker >/dev/null 2>&1 || die "need Docker to cross-build"
  docker info >/dev/null 2>&1 || die "Docker daemon not running (start OrbStack)"
  "${ROOT}/scripts/build-release.sh"
  [[ -f "$BIN" ]] || die "missing $BIN after build"
}

# --- binary: Docker arm64 (best on Apple Silicon / multiarch) ---
smoke_binary() {
  log "LEVEL binary"
  ensure_binary
  command -v docker >/dev/null 2>&1 || die "Docker required for binary smoke"
  docker info >/dev/null 2>&1 || die "Docker daemon not running"

  local cid
  cid="$(docker run -d --rm --platform linux/arm64 \
    -p "${HTTP_PORT}:8080" \
    -v "${BIN}:/vinyl-bridge-pi:ro" \
    debian:bookworm-slim \
    /vinyl-bridge-pi --http-bind 0.0.0.0:8080 --log warn)"
  log "container $cid"
  local i ok=0
  for i in $(seq 1 40); do
    if curl -fsS --max-time 2 "http://127.0.0.1:${HTTP_PORT}/api/status" 2>/dev/null \
      | tee "${WORK}/status-binary.json" | grep -qE 'version|Vinyl|raspberry'; then
      ok=1
      break
    fi
    sleep 0.5
  done
  if [[ "$ok" -ne 1 ]]; then
    docker logs "$cid" 2>&1 | tail -50 || true
    docker stop "$cid" >/dev/null 2>&1 || true
    die "binary smoke: HTTP /api/status failed"
  fi
  log "PASS binary — /api/status"
  cat "${WORK}/status-binary.json"; echo
  docker stop "$cid" >/dev/null 2>&1 || true
}

# --- virt: Debian cloud + 9p binary + systemd ---
smoke_virt() {
  log "LEVEL virt (-M virt, aarch64)"
  ensure_binary
  local qs
  qs="$(find_qemu_system)" || die "qemu-system-aarch64 not found — brew install qemu"
  log "using $qs"
  machine_has "$qs" virt || die "QEMU has no -M virt"
  command -v qemu-img >/dev/null 2>&1 || die "qemu-img not found"

  local base="${WORK}/debian-12-generic-arm64.qcow2"
  local url="https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-arm64.qcow2"
  if [[ ! -f "$base" ]]; then
    log "downloading Debian 12 arm64 cloud image (one-time, ~300MB)"
    curl -fL --retry 3 -o "${base}.partial" "$url"
    mv "${base}.partial" "$base"
  fi

  local overlay="${WORK}/virt-overlay.qcow2"
  rm -f "$overlay"
  qemu-img create -f qcow2 -b "$base" -F qcow2 "$overlay" >/dev/null
  # grow for packages/logs
  qemu-img resize "$overlay" +512M >/dev/null 2>&1 || true

  local stage="${WORK}/cidata"
  rm -rf "$stage"
  mkdir -p "$stage"
  cat >"${stage}/meta-data" <<EOF
instance-id: vinyl-qemu-$(date +%s)
local-hostname: vinyl-bridge-qemu
EOF
  cat >"${stage}/user-data" <<'EOF'
#cloud-config
hostname: vinyl-bridge-qemu
package_update: false
ssh_pwauth: false
write_files:
  - path: /etc/vinyl-bridge/config.toml
    permissions: '0644'
    content: |
      device_name = "Vinyl Bridge QEMU"
      http_bind = "0.0.0.0:80"
      mdns_hostname = "vinyl-bridge"
      record_dir = "/var/lib/vinyl-bridge/rec"
      volume_db = -30.0
  - path: /etc/systemd/system/vinyl-bridge.service
    permissions: '0644'
    content: |
      [Unit]
      Description=Vinyl Audio Bridge (Pi host)
      After=network-online.target
      Wants=network-online.target
      [Service]
      Type=simple
      ExecStart=/usr/local/bin/vinyl-bridge-pi --config /etc/vinyl-bridge/config.toml
      Restart=on-failure
      RestartSec=1
      Environment=RUST_LOG=warn
      [Install]
      WantedBy=multi-user.target
  - path: /usr/local/sbin/vinyl-install-from-9p.sh
    permissions: '0755'
    content: |
      #!/bin/bash
      set -e
      mkdir -p /mnt/host /usr/local/bin /var/lib/vinyl-bridge/rec
      for i in $(seq 1 90); do
        if mount -t 9p -o trans=virtio,version=9p2000.L host /mnt/host 2>/dev/null; then
          break
        fi
        # some kernels use different opts
        if mount -t 9p -o trans=virtio host /mnt/host 2>/dev/null; then
          break
        fi
        sleep 1
      done
      if [[ ! -x /mnt/host/vinyl-bridge-pi ]]; then
        echo "vinyl-install: binary not visible on 9p" >&2
        ls -la /mnt/host || true
        exit 1
      fi
      install -m 0755 /mnt/host/vinyl-bridge-pi /usr/local/bin/vinyl-bridge-pi
      systemctl daemon-reload
      systemctl enable --now vinyl-bridge.service
      sleep 1
      curl -fsS http://127.0.0.1/api/status || true
runcmd:
  - /usr/local/sbin/vinyl-install-from-9p.sh
EOF

  local seed="${WORK}/seed.iso"
  rm -f "$seed"
  if command -v mkisofs >/dev/null 2>&1; then
    mkisofs -output "$seed" -volid cidata -joliet -rock \
      -graft-points "user-data=${stage}/user-data" "meta-data=${stage}/meta-data" \
      >/dev/null 2>&1
  elif command -v genisoimage >/dev/null 2>&1; then
    genisoimage -output "$seed" -volid cidata -joliet -rock \
      -graft-points "user-data=${stage}/user-data" "meta-data=${stage}/meta-data" \
      >/dev/null 2>&1
  elif command -v hdiutil >/dev/null 2>&1; then
    # Volume name CIDATA (cloud-init accepts cidata/CIDATA)
    hdiutil makehybrid -o "${WORK}/seed" -iso -joliet -default-volume-name CIDATA "$stage" >/dev/null
    if [[ -f "${WORK}/seed.iso" ]]; then
      mv -f "${WORK}/seed.iso" "$seed"
    elif [[ -f "${WORK}/seed.cdr" ]]; then
      mv -f "${WORK}/seed.cdr" "$seed"
    else
      die "hdiutil did not produce seed ISO"
    fi
  else
    die "need mkisofs, genisoimage, or hdiutil to build cloud-init seed"
  fi

  local serial_log="${WORK}/virt-serial.log"
  rm -f "$serial_log"
  local qemu_pid
  log "booting -M virt (timeout ${TIMEOUT_S}s); host :${HTTP_PORT} → guest :80"
  log "serial log: $serial_log"

  # fsdev + device form is portable across QEMU versions
  "$qs" \
    -machine virt \
    -cpu max \
    -m 1024 \
    -smp 2 \
    -nographic \
    -drive "if=virtio,format=qcow2,file=${overlay}" \
    -drive "if=virtio,format=raw,file=${seed}" \
    -fsdev "local,id=fsdev0,path=${DIST},security_model=mapped" \
    -device "virtio-9p-device,fsdev=fsdev0,mount_tag=host" \
    -netdev "user,id=net0,hostfwd=tcp::${HTTP_PORT}-:80" \
    -device virtio-net-device,netdev=net0 \
    >"$serial_log" 2>&1 &
  qemu_pid=$!

  cleanup() { kill "$qemu_pid" 2>/dev/null || true; wait "$qemu_pid" 2>/dev/null || true; }
  trap cleanup EXIT

  local deadline=$((SECONDS + TIMEOUT_S))
  while (( SECONDS < deadline )); do
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
      tail -100 "$serial_log" || true
      die "virt QEMU exited early"
    fi
    if curl -fsS --max-time 2 "http://127.0.0.1:${HTTP_PORT}/api/status" 2>/dev/null \
      | tee "${WORK}/status-virt.json" | grep -qE 'version|Vinyl|raspberry'; then
      log "PASS virt — /api/status (boot + systemd + binary)"
      (python3 -m json.tool "${WORK}/status-virt.json" 2>/dev/null || cat "${WORK}/status-virt.json")
      echo
      cleanup
      trap - EXIT
      return 0
    fi
    sleep 3
  done
  log "FAIL virt — last serial lines:"
  tail -150 "$serial_log" || true
  die "virt smoke timed out after ${TIMEOUT_S}s"
}

# --- image: flashable SD img ---
smoke_image() {
  log "LEVEL image (SD card)"
  local img="${DIST}/vinyl-bridge-pi4.img"
  if [[ ! -f "$img" ]]; then
    if [[ -f "${img}.xz" ]]; then
      log "decompress ${img}.xz"
      xz -T0 -dkf "${img}.xz"
    else
      die "missing $img — assemble with deploy/image/build-sd-image.sh first"
    fi
  fi

  local qs
  qs="$(find_qemu_system)" || die "qemu-system-aarch64 not found"
  local serial_log="${WORK}/image-serial.log"
  rm -f "$serial_log"
  local qemu_pid

  if machine_has "$qs" raspi4b; then
    log "machine raspi4b available — extracting kernel/dtb from image boot partition"
    local boot="${WORK}/sd-boot"
    rm -rf "$boot"
    mkdir -p "$boot"
    if ! extract_boot_fat "$img" "$boot"; then
      die "cannot extract boot FAT from SD image (install mtools or run on Linux)"
    fi
    local kernel dtb
    kernel="$(find "$boot" -name 'kernel8.img' | head -1)"
    dtb="$(find "$boot" -name 'bcm2711-rpi-4-b.dtb' | head -1)"
    [[ -f "$kernel" && -f "$dtb" ]] || die "kernel8.img or bcm2711-rpi-4-b.dtb missing in boot partition"

    log "booting -M raspi4b (USB net; audio N/A)"
    "$qs" \
      -machine raspi4b \
      -cpu cortex-a72 \
      -m 2048 \
      -nographic \
      -kernel "$kernel" \
      -dtb "$dtb" \
      -drive "file=${img},if=sd,format=raw" \
      -append "console=ttyAMA0,115200 root=/dev/mmcblk0p2 rootfstype=ext4 rootwait rw" \
      -device usb-net,netdev=net0 \
      -netdev "user,id=net0,hostfwd=tcp::${HTTP_PORT}-:80" \
      >"$serial_log" 2>&1 &
    qemu_pid=$!
  else
    log "WARN: this QEMU has no -M raspi4b — cannot claim full Pi4 SoC boot"
    log "Run virt smoke for systemd path; upgrade QEMU for raspi4b, or test on hardware."
    die "raspi4b machine model not available in $($qs --version | head -1)"
  fi

  cleanup() { kill "$qemu_pid" 2>/dev/null || true; wait "$qemu_pid" 2>/dev/null || true; }
  trap cleanup EXIT

  local deadline=$((SECONDS + TIMEOUT_S + 120))
  while (( SECONDS < deadline )); do
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
      tail -120 "$serial_log" || true
      die "image QEMU exited early"
    fi
    if curl -fsS --max-time 2 "http://127.0.0.1:${HTTP_PORT}/api/status" 2>/dev/null \
      | tee "${WORK}/status-image.json" | grep -qE 'version|Vinyl|raspberry'; then
      log "PASS image (raspi4b) — /api/status"
      cat "${WORK}/status-image.json"; echo
      cleanup
      trap - EXIT
      return 0
    fi
    sleep 5
  done
  tail -150 "$serial_log" || true
  die "image smoke timed out (see $serial_log)"
}

extract_boot_fat() {
  local img="$1" dest="$2"
  # Partition 1 usually starts at sector 8192 on Pi OS.
  local start=8192
  if command -v fdisk >/dev/null 2>&1; then
    local parsed
    parsed="$(fdisk -l "$img" 2>/dev/null | awk '/FAT|W95|EFI/{print $2; exit}')"
    if [[ -n "$parsed" && "$parsed" =~ ^[0-9]+$ ]]; then
      start="$parsed"
    fi
  fi
  local offset=$((start * 512))
  if command -v mdir >/dev/null 2>&1; then
    export MTOOLS_SKIP_CHECK=1
    mdir -i "${img}@@${offset}" :: >/dev/null 2>&1 || return 1
    mcopy -i "${img}@@${offset}" ::kernel8.img ::bcm2711-rpi-4-b.dtb "$dest/" 2>/dev/null || \
      mcopy -s -i "${img}@@${offset}" :: "$dest/" 2>/dev/null || return 1
    return 0
  fi
  if [[ "$(uname -s)" == "Linux" ]]; then
    local loop
    loop="$(losetup -fP --show "$img")"
    mkdir -p "${dest}.mnt"
    mount -o ro "${loop}p1" "${dest}.mnt"
    cp -a "${dest}.mnt/." "$dest/"
    umount "${dest}.mnt"
    losetup -d "$loop"
    return 0
  fi
  return 1
}

case "$LEVEL" in
  binary) smoke_binary ;;
  virt)   smoke_virt ;;
  image)  smoke_image ;;
  all)
    smoke_binary
    smoke_virt
    if [[ -f "${DIST}/vinyl-bridge-pi4.img" || -f "${DIST}/vinyl-bridge-pi4.img.xz" ]]; then
      smoke_image
    else
      log "skip image (no dist/vinyl-bridge-pi4.img)"
    fi
    ;;
  default)
    smoke_binary
    smoke_virt
    ;;
  *) die "usage: $0 [binary|virt|image|all|default]" ;;
esac

log "qemu-smoke finished OK"
echo "Limits: no real ALSA line-in, no Wi‑Fi RF, no HomePod path in QEMU."
