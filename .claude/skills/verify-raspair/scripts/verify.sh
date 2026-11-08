#!/usr/bin/env bash
set -euo pipefail

SCRIPTS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPTS/../../../.." && pwd)"
VERIFY_DIR="$REPO/.verify"
RECEIVER_IMAGE="mikebrady/shairport-sync:latest"
RUST_IMAGE="rust:1-bookworm"
TARGET_VOLUME="raspair-verify-target"
CARGO_VOLUME="raspair-verify-cargo"

die() { echo "verify: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: verify.sh <command> [args]

  up [--mode realtime|buffered]   build, start receiver + bridge on a private network
  doctor                          read-only health check of the current run
  api METHOD PATH [curl args]     call the bridge HTTP API (JSON body via -d)
  receiver-audio SECONDS          capture what the receiver plays for SECONDS and check it
  stream-tap SECONDS              record /capture.wav from the stream port and check it
  restart-bridge                  restart this run's bridge container, refresh its ports
  receiver-ptp                    does the receiver's nqptp have a PTP master (buffered mode)
  check FILE                      run check_audio.py on a WAV file
  logs bridge|receiver            print container logs
  down                            save logs, remove containers + network, keep evidence
  runs                            list runs and their evidence directories

The current run is read from $RASPAIR_RUN, else .verify/current.
EOF
}

run_dir() { echo "$VERIFY_DIR/runs/$1"; }

load_env() {
  RUN="${RASPAIR_RUN:-$(cat "$VERIFY_DIR/current" 2>/dev/null || true)}"
  [ -n "$RUN" ] || die "no current run; start one with: verify.sh up"
  DIR="$(run_dir "$RUN")"
  [ -f "$DIR/env" ] || die "run $RUN has no env file at $DIR/env"
  source "$DIR/env"
}

base_url() { echo "http://127.0.0.1:$HTTP_PORT"; }

wait_for() {
  local what="$1" tries="$2"
  shift 2
  for _ in $(seq "$tries"); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  die "timed out waiting for $what"
}

receiver_ready() { docker exec "$RECEIVER" netstat -ltn 2>/dev/null | grep -q ':7000 '; }

bridge_ready() { curl -fsS "$(base_url)/api/status" | jq -e '.parity.capture == "tone"'; }

cmd_build() {
  docker volume create "$TARGET_VOLUME" >/dev/null
  docker volume create "$CARGO_VOLUME" >/dev/null
  docker run --rm \
    -v "$REPO:/src:ro" \
    -v "$TARGET_VOLUME:/target" \
    -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/target \
    -w /src/apps/pi4-host \
    "$RUST_IMAGE" cargo build --no-default-features --locked
}

write_receiver_conf() {
  cat > "$1" <<'EOF'
general = {
  name = "raspair-verify";
  output_backend = "pipe";
  ignore_volume_control = "yes";
};
pipe = {
  name = "/out/audio.raw";
  output_rate = 48000;
  output_format = "S16_LE";
  output_channels = 2;
};
EOF
}

write_bridge_conf() {
  cat > "$1" <<EOF
device_name = "raspair-verify-bridge"
http_bind = "0.0.0.0:8080"
mdns_hostname = "raspair-verify-bridge"
capture_device = "tone"
sample_rate = 48000
channels = 2
bits = 24
stream_port = 8088
airplay_mode = "$2"
record_dir = "/verify/rec"
volume_db = -30.0
EOF
}

cmd_up() {
  local mode=realtime
  while [ $# -gt 0 ]; do
    case "$1" in
      --mode) mode="$2"; shift 2 ;;
      *) die "unknown flag $1" ;;
    esac
  done
  case "$mode" in realtime|buffered) ;; *) die "--mode must be realtime or buffered" ;; esac

  RUN="$(date +%m%d-%H%M%S)-$RANDOM"
  DIR="$(run_dir "$RUN")"
  NET="raspair-verify-$RUN"
  RECEIVER="$NET-receiver"
  BRIDGE="$NET-bridge"
  mkdir -p "$DIR/receiver" "$DIR/rec" "$DIR/evidence"
  printf 'RUN=%s\nMODE=%s\nNET=%s\nRECEIVER=%s\nBRIDGE=%s\nHTTP_PORT=\nSTREAM_PORT=\n' \
    "$RUN" "$mode" "$NET" "$RECEIVER" "$BRIDGE" > "$DIR/env"
  echo "$RUN" > "$VERIFY_DIR/current"

  echo "verify: building vinyl-bridge-pi in $RUST_IMAGE (first build takes minutes), log: $DIR/build.log" >&2
  cmd_build > "$DIR/build.log" 2>&1 || { tail -40 "$DIR/build.log" >&2; die "build failed, see $DIR/build.log"; }
  docker run --rm -v "$TARGET_VOLUME:/target:ro" -v "$DIR:/verify" "$RUST_IMAGE" \
    cp /target/debug/vinyl-bridge-pi /verify/vinyl-bridge-pi

  docker network create --label "raspair-verify=$RUN" "$NET" >/dev/null
  write_receiver_conf "$DIR/receiver.conf"
  : > "$DIR/receiver/audio.raw"
  docker run -d --name "$RECEIVER" --label "raspair-verify=$RUN" --network "$NET" \
    -v "$DIR/receiver.conf:/etc/shairport-sync.conf:ro" \
    -v "$DIR/receiver:/out" \
    "$RECEIVER_IMAGE" -v --statistics >/dev/null
  wait_for "receiver listening on :7000" 30 receiver_ready

  write_bridge_conf "$DIR/bridge.toml" "$mode"
  docker run -d --name "$BRIDGE" --label "raspair-verify=$RUN" --network "$NET" \
    -p 127.0.0.1::8080 -p 127.0.0.1::8088 \
    -v "$DIR:/verify" -e RUST_LOG=info \
    "$RUST_IMAGE" /verify/vinyl-bridge-pi --config /verify/bridge.toml >/dev/null

  HTTP_PORT="$(docker port "$BRIDGE" 8080/tcp | head -1 | sed 's/.*://')"
  STREAM_PORT="$(docker port "$BRIDGE" 8088/tcp | head -1 | sed 's/.*://')"
  RECEIVER_IP="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$RECEIVER")"
  cat > "$DIR/env" <<EOF
RUN=$RUN
MODE=$mode
NET=$NET
RECEIVER=$RECEIVER
BRIDGE=$BRIDGE
RECEIVER_IP=$RECEIVER_IP
HTTP_PORT=$HTTP_PORT
STREAM_PORT=$STREAM_PORT
GIT_REV=$(git -C "$REPO" rev-parse --short HEAD)$(git -C "$REPO" diff --quiet HEAD -- apps || echo -dirty)
BINARY_SHA=$(sha256sum "$DIR/vinyl-bridge-pi" | cut -c1-16)
EOF
  wait_for "bridge HTTP API with tone capture" 30 bridge_ready
  cat "$DIR/env"
  echo "API=$(base_url)"
  echo "EVIDENCE=$DIR/evidence"
}

cmd_doctor() {
  load_env
  local ok=0
  for c in "$BRIDGE" "$RECEIVER"; do
    local state label
    state="$(docker inspect -f '{{.State.Status}}' "$c" 2>/dev/null || echo missing)"
    label="$(docker inspect -f '{{index .Config.Labels "raspair-verify"}}' "$c" 2>/dev/null || true)"
    echo "container $c: $state (label raspair-verify=$label)"
    [ "$state" = running ] && [ "$label" = "$RUN" ] || ok=1
  done
  local status
  if status="$(curl -fsS --max-time 3 "$(base_url)/api/status")"; then
    echo "$status" | jq -c '{name, version, capture: .parity.capture, airplay2: .parity.airplay2, streaming, rms_dbfs: (.rms_dbfs | floor), targets: [.targets[].name], airplay: [.airplay[] | {name, active, note}]}'
    echo "$status" | jq -e '.name == "raspair-verify-bridge" and .parity.capture == "tone"' >/dev/null || ok=1
  else
    echo "bridge API $(base_url): not answering"
    ok=1
  fi
  local running_sha
  running_sha="$(sha256sum "$DIR/vinyl-bridge-pi" 2>/dev/null | cut -c1-16 || true)"
  echo "build: mode=$MODE git=$GIT_REV binary=$BINARY_SHA on-disk=$running_sha checkout-now=$(git -C "$REPO" rev-parse --short HEAD)$(git -C "$REPO" diff --quiet HEAD -- apps || echo -dirty)"
  [ "$running_sha" = "$BINARY_SHA" ] || ok=1
  if docker exec "$RECEIVER" netstat -ltn 2>/dev/null | grep -q ':7000 '; then
    echo "receiver $RECEIVER_IP:7000 listening"
  else
    echo "receiver $RECEIVER_IP:7000 not listening"
    ok=1
  fi
  echo "receiver bytes so far: $(stat -c %s "$DIR/receiver/audio.raw" 2>/dev/null || echo 0)"
  [ "$ok" = 0 ] && echo "doctor: OK (run $RUN)" || { echo "doctor: NOT OK (run $RUN)"; return 1; }
}

cmd_api() {
  load_env
  [ $# -ge 2 ] || die "usage: verify.sh api METHOD PATH [curl args]"
  local method="$1" path="$2"
  shift 2
  curl -sS -X "$method" -H 'content-type: application/json' "$(base_url)$path" "$@"
  echo
}

cmd_receiver_audio() {
  load_env
  local seconds="${1:-5}" raw="$DIR/receiver/audio.raw" stamp out start end
  stamp="$(date +%H%M%S)"
  out="$DIR/evidence/receiver-$stamp.wav"
  start="$(stat -c %s "$raw")"
  sleep "$seconds"
  end="$(stat -c %s "$raw")"
  python3 "$SCRIPTS/check_audio.py" --wrap-raw "$raw" "$start" "$end" "$out"
  python3 "$SCRIPTS/check_audio.py" "$out" | tee "$DIR/evidence/receiver-$stamp.json"
}

cmd_stream_tap() {
  load_env
  local seconds="${1:-3}" stamp out
  stamp="$(date +%H%M%S)"
  out="$DIR/evidence/capture-tap-$stamp.wav"
  curl -sS --max-time "$seconds" "http://127.0.0.1:$STREAM_PORT/capture.wav" -o "$out" || true
  python3 "$SCRIPTS/check_audio.py" "$out" | tee "$DIR/evidence/capture-tap-$stamp.json"
}

cmd_restart_bridge() {
  load_env
  docker restart "$BRIDGE" >/dev/null
  HTTP_PORT="$(docker port "$BRIDGE" 8080/tcp | head -1 | sed 's/.*://')"
  STREAM_PORT="$(docker port "$BRIDGE" 8088/tcp | head -1 | sed 's/.*://')"
  sed -i "s/^HTTP_PORT=.*/HTTP_PORT=$HTTP_PORT/; s/^STREAM_PORT=.*/STREAM_PORT=$STREAM_PORT/" "$DIR/env"
  wait_for "bridge HTTP API with tone capture" 30 bridge_ready
  echo "API=$(base_url) STREAM_PORT=$STREAM_PORT"
}

cmd_receiver_ptp() {
  load_env
  local shm
  shm="$(docker exec "$RECEIVER" od -A n -t x1 -j 8 /dev/shm/nqptp 2>/dev/null | tr -d ' \n')"
  if [ -z "$shm" ]; then
    echo "receiver PTP: no nqptp shared memory (receiver not running?)"
    return 1
  elif echo "$shm" | grep -q '[1-9a-f]'; then
    echo "receiver PTP: nqptp has a master clock"
  else
    echo "receiver PTP: nqptp has NO master clock; the sender's PTP is not accepted, buffered audio will not play"
    return 1
  fi
}

cmd_check() {
  [ $# -ge 1 ] || die "usage: verify.sh check FILE.wav"
  python3 "$SCRIPTS/check_audio.py" "$@"
}

cmd_logs() {
  load_env
  case "${1:-}" in
    bridge) docker logs "$BRIDGE" 2>&1 ;;
    receiver) docker logs "$RECEIVER" 2>&1 ;;
    *) die "usage: verify.sh logs bridge|receiver" ;;
  esac
}

cmd_down() {
  load_env
  mkdir -p "$DIR/evidence"
  docker logs "$BRIDGE" > "$DIR/evidence/bridge.log" 2>&1 || true
  docker logs "$RECEIVER" > "$DIR/evidence/receiver.log" 2>&1 || true
  cp "$DIR/env" "$DIR/evidence/env" 2>/dev/null || true
  docker ps -aq --filter "label=raspair-verify=$RUN" | xargs -r docker rm -f >/dev/null
  docker network ls -q --filter "label=raspair-verify=$RUN" | xargs -r docker network rm >/dev/null
  rm -rf "$DIR/receiver" "$DIR/rec" "$DIR/vinyl-bridge-pi" "$DIR/settings.json"
  if [ "$(cat "$VERIFY_DIR/current" 2>/dev/null || true)" = "$RUN" ]; then rm -f "$VERIFY_DIR/current"; fi
  echo "down: run $RUN removed; evidence kept at $DIR/evidence"
  ls -1 "$DIR/evidence"
}

cmd_runs() {
  ls -1 "$VERIFY_DIR/runs" 2>/dev/null | while read -r r; do
    local live
    live="$(docker ps -q --filter "label=raspair-verify=$r" | wc -l)"
    echo "$r containers=$live evidence=$(run_dir "$r")/evidence"
  done
}

case "${1:-}" in
  up) shift; cmd_up "$@" ;;
  doctor) shift; cmd_doctor ;;
  api) shift; cmd_api "$@" ;;
  receiver-audio) shift; cmd_receiver_audio "$@" ;;
  stream-tap) shift; cmd_stream_tap "$@" ;;
  restart-bridge) cmd_restart_bridge ;;
  receiver-ptp) cmd_receiver_ptp ;;
  check) shift; cmd_check "$@" ;;
  logs) shift; cmd_logs "$@" ;;
  down) shift; cmd_down ;;
  runs) cmd_runs ;;
  *) usage; exit 2 ;;
esac
