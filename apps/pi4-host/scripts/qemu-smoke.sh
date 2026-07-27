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

  # Avoid bind-mounting dist/ (flaky on external volumes). docker cp + libasound2
  # (binary is dynamically linked against ALSA).
  local name="vinyl-qemu-binary-$$"
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --platform linux/arm64 \
    -p "${HTTP_PORT}:8080" \
    debian:bookworm-slim \
    sleep 600 >/dev/null
  cleanup_bin() { docker rm -f "$name" >/dev/null 2>&1 || true; }
  trap cleanup_bin EXIT

  docker cp "$BIN" "${name}:/usr/local/bin/vinyl-bridge-pi"
  docker exec "$name" bash -c '
    set -euo pipefail
    chmod +x /usr/local/bin/vinyl-bridge-pi
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq libasound2 ca-certificates >/dev/null
  '
  docker exec -d "$name" /usr/local/bin/vinyl-bridge-pi --http-bind 0.0.0.0:8080 --log warn
  log "container $name (docker arm64 + libasound2)"

  local i ok=0
  for i in $(seq 1 60); do
    if curl -fsS --max-time 2 "http://127.0.0.1:${HTTP_PORT}/api/status" 2>/dev/null \
      | tee "${WORK}/status-binary.json" | grep -qE 'version|Vinyl|raspberry|macos|linux'; then
      ok=1
      break
    fi
    sleep 0.5
  done
  if [[ "$ok" -ne 1 ]]; then
    docker logs "$name" 2>&1 | tail -50 || true
    docker exec "$name" ps aux 2>/dev/null || true
    docker exec "$name" /usr/local/bin/vinyl-bridge-pi --version 2>&1 || true
    cleanup_bin
    trap - EXIT
    die "binary smoke: HTTP /api/status failed"
  fi
  log "PASS binary — /api/status"
  cat "${WORK}/status-binary.json"; echo
  cleanup_bin
  trap - EXIT
}

find_edk2_code() {
  for p in \
    /opt/homebrew/share/qemu/edk2-aarch64-code.fd \
    /usr/local/share/qemu/edk2-aarch64-code.fd \
    /usr/share/qemu/edk2-aarch64-code.fd \
    /usr/share/AAVMF/AAVMF_CODE.fd; do
    if [[ -f "$p" ]]; then
      echo "$p"
      return 0
    fi
  done
  return 1
}

# --- virt: Debian cloud + UEFI + cloud-init + systemd ---
smoke_virt() {
  log "LEVEL virt (-M virt, aarch64 + UEFI)"
  ensure_binary
  local qs efi
  qs="$(find_qemu_system)" || die "qemu-system-aarch64 not found — brew install qemu"
  efi="$(find_edk2_code)" || die "missing edk2-aarch64-code.fd (brew install qemu)"
  log "using $qs"
  log "UEFI $efi"
  machine_has "$qs" virt || die "QEMU has no -M virt"
  command -v qemu-img >/dev/null 2>&1 || die "qemu-img not found"

  local base="${WORK}/debian-12-generic-arm64.qcow2"
  local url="https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-arm64.qcow2"
  if [[ ! -f "$base" ]]; then
    log "downloading Debian 12 arm64 cloud image (one-time, ~400MB)"
    curl -fL --retry 3 -o "${base}.partial" "$url"
    mv "${base}.partial" "$base"
  fi

  local overlay="${WORK}/virt-overlay.qcow2"
  rm -f "$overlay"
  qemu-img create -f qcow2 -b "$base" -F qcow2 "$overlay" >/dev/null
  qemu-img resize "$overlay" +1G >/dev/null 2>&1 || true

  # Embed binary as gzip+base64 (avoids 9p mount races).
  local gz_b64="${WORK}/bin.gz.b64"
  gzip -c -n "$BIN" | base64 | tr -d '\n' >"$gz_b64"
  local stage="${WORK}/cidata"
  rm -rf "$stage"
  mkdir -p "$stage"
  cat >"${stage}/meta-data" <<EOF
instance-id: vinyl-qemu-$(date +%s)
local-hostname: vinyl-bridge-qemu
EOF
  # user-data: write b64 payload via separate file on seed to keep YAML small
  cp "$gz_b64" "${stage}/vinyl-bridge-pi.gz.b64"
  cat >"${stage}/user-data" <<'EOF'
#cloud-config
hostname: vinyl-bridge-qemu
package_update: true
packages:
  - libasound2
  - curl
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
runcmd:
  - |
    set -e
    mkdir -p /usr/local/bin /var/lib/vinyl-bridge/rec
    # NoCloud seed is mounted at /dev/disk/by-label/CIDATA or /cidata
    SRC=""
    for p in /cidata /media/cidata /mnt /run/cloud-init; do
      if [ -f "$p/vinyl-bridge-pi.gz.b64" ]; then SRC="$p/vinyl-bridge-pi.gz.b64"; break; fi
    done
    if [ -z "$SRC" ]; then
      # try blkid mount
      mkdir -p /mnt/cidata
      CIDEV=$(blkid -L CIDATA 2>/dev/null || blkid -L cidata 2>/dev/null || true)
      if [ -n "$CIDEV" ]; then mount -o ro "$CIDEV" /mnt/cidata || true; fi
      if [ -f /mnt/cidata/vinyl-bridge-pi.gz.b64 ]; then SRC=/mnt/cidata/vinyl-bridge-pi.gz.b64; fi
    fi
    if [ -z "$SRC" ] || [ ! -f "$SRC" ]; then
      echo "vinyl: seed binary not found" >&2
      ls -la /cidata /mnt/cidata 2>/dev/null || true
      exit 1
    fi
    base64 -d "$SRC" | gzip -d > /usr/local/bin/vinyl-bridge-pi
    chmod 755 /usr/local/bin/vinyl-bridge-pi
    systemctl daemon-reload
    systemctl enable --now vinyl-bridge.service
    sleep 2
    curl -fsS http://127.0.0.1/api/status || true
EOF

  # Prefer FAT NoCloud seed (reliable on aarch64 virt).
  # Build inside Docker when host cannot mkfs.vfat (macOS sandbox / no mtools).
  local seed="${WORK}/seed.img"
  rm -f "$seed"
  log "building NoCloud FAT seed.img"
  if command -v mkfs.vfat >/dev/null 2>&1 && command -v mcopy >/dev/null 2>&1; then
    dd if=/dev/zero of="$seed" bs=1M count=48 status=none
    mkfs.vfat -n CIDATA "$seed" >/dev/null
    mcopy -i "$seed" "${stage}/user-data" "${stage}/meta-data" "${stage}/vinyl-bridge-pi.gz.b64" ::
  elif command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    tar -C "$stage" -cf - user-data meta-data vinyl-bridge-pi.gz.b64 \
    | docker run --rm -i debian:bookworm-slim bash -c '
        set -euo pipefail
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq dosfstools mtools >/dev/null
        mkdir -p /cidata
        tar -C /cidata -xf -
        dd if=/dev/zero of=/seed.img bs=1M count=48 status=none
        mkfs.vfat -n CIDATA /seed.img >/dev/null
        mcopy -i /seed.img /cidata/user-data /cidata/meta-data /cidata/vinyl-bridge-pi.gz.b64 ::
        cat /seed.img
      ' >"$seed"
  else
    die "need mkfs.vfat+mcopy or Docker to build NoCloud seed"
  fi
  [[ -s "$seed" ]] || die "empty seed.img"

  # Writable UEFI vars store (required for some guests)
  local efi_vars="${WORK}/edk2-vars.fd"
  if [[ ! -f "$efi_vars" ]]; then
    # 64 MiB empty flash is the usual size for QEMU edk2 vars
    dd if=/dev/zero of="$efi_vars" bs=1m count=64 status=none 2>/dev/null \
      || dd if=/dev/zero of="$efi_vars" bs=1048576 count=64 status=none
  fi

  local serial_log="${WORK}/virt-serial.log"
  rm -f "$serial_log"
  local qemu_pid
  local virt_timeout="${TIMEOUT_S}"
  # First cloud-init boot + apt is slow
  if (( virt_timeout < 480 )); then virt_timeout=480; fi
  log "booting -M virt + UEFI (timeout ${virt_timeout}s); host :${HTTP_PORT} → guest :80"
  log "serial log: $serial_log"

  local accel_args=()
  if [[ "$(uname -s)" == "Darwin" ]] && "$qs" -accel help 2>/dev/null | grep -q hvf; then
    accel_args=(-accel hvf)
  fi

  "$qs" \
    -machine virt,gic-version=3 \
    "${accel_args[@]}" \
    -cpu max \
    -m 2048 \
    -smp 2 \
    -nographic \
    -drive "if=pflash,format=raw,readonly=on,file=${efi}" \
    -drive "if=pflash,format=raw,file=${efi_vars}" \
    -drive "if=virtio,format=qcow2,file=${overlay}" \
    -drive "if=virtio,format=raw,file=${seed}" \
    -netdev "user,id=net0,hostfwd=tcp::${HTTP_PORT}-:80" \
    -device virtio-net-device,netdev=net0 \
    >"$serial_log" 2>&1 &
  qemu_pid=$!

  cleanup() { kill "$qemu_pid" 2>/dev/null || true; wait "$qemu_pid" 2>/dev/null || true; }
  trap cleanup EXIT

  local deadline=$((SECONDS + virt_timeout))
  while (( SECONDS < deadline )); do
    if ! kill -0 "$qemu_pid" 2>/dev/null; then
      tail -120 "$serial_log" || true
      die "virt QEMU exited early"
    fi
    if curl -fsS --max-time 2 "http://127.0.0.1:${HTTP_PORT}/api/status" 2>/dev/null \
      | tee "${WORK}/status-virt.json" | grep -qE 'version|Vinyl|raspberry|linux'; then
      log "PASS virt — /api/status (boot + systemd + binary)"
      (python3 -m json.tool "${WORK}/status-virt.json" 2>/dev/null || cat "${WORK}/status-virt.json")
      echo
      cleanup
      trap - EXIT
      return 0
    fi
    sleep 5
  done
  log "FAIL virt — last serial lines:"
  tail -200 "$serial_log" || true
  die "virt smoke timed out after ${virt_timeout}s"
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
