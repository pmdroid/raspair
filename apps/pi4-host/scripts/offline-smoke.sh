#!/usr/bin/env bash
set -euo pipefail
BIN="${1:?usage: offline-smoke.sh BINARY ROOTFS}"
ROOTFS="$(cd "${2:?usage: offline-smoke.sh BINARY ROOTFS}" && pwd)"
QEMU="${QEMU_USER:-$(command -v qemu-aarch64 || command -v qemu-aarch64-static)}"
PORT="${HTTP_PORT:-18080}"

if [[ "${RASPAIR_SMOKE_NETNS:-}" != 1 ]]; then
  install -D -m 0755 "$BIN" "$ROOTFS/usr/local/bin/vinyl-bridge-pi"
  RASPAIR_SMOKE_NETNS=1 QEMU_USER="$QEMU" exec unshare -rn "$0" "$ROOTFS/usr/local/bin/vinyl-bridge-pi" "$ROOTFS"
fi

ip link set lo up
[[ "$(ip -o link show | grep -cv ' lo:')" == 0 ]] || { echo "error: namespace has non-loopback links" >&2; exit 1; }
work="$(mktemp -d)"
mkdir -p "$work/rec"
printf 'device_name = "Offline smoke"\nhttp_bind = "127.0.0.1:%s"\nrecord_dir = "%s/rec"\n' "$PORT" "$work" >"$work/config.toml"
printf '{"music_id_enabled": true, "acoustid_api_key": "offline-smoke"}\n' >"$work/settings.json"
env -i PATH=/nonexistent HOME="$work" "$QEMU" -L "$ROOTFS" "$BIN" --config "$work/config.toml" --log warn \
  >"$work/daemon.log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true; rm -rf "$work"' EXIT

base="http://127.0.0.1:$PORT"
fail() { echo "FAIL: $*" >&2; tail -50 "$work/daemon.log" >&2; exit 1; }
for _ in $(seq 1 240); do
  curl -fsS -o /dev/null "$base/api/status" 2>/dev/null && break
  kill -0 "$pid" 2>/dev/null || fail "daemon exited"
  sleep 0.5
done
# req METHOD PATH [JSON]: body in $work/body; on curl error or non-2xx, fail with the
# request, status and response body (a bare `curl -f` hid which request returned 500).
req() {
  local method="$1" path="$2" data="${3-}" code
  local args=(-sS -X "$method" -o "$work/body" -w '%{http_code} %{content_type}')
  [[ -n "$data" ]] && args+=(-H 'content-type: application/json' -d "$data")
  code="$(curl "${args[@]}" "$base$path")" || fail "$method $path: curl error"
  resp_type="${code#* }"
  code="${code%% *}"
  [[ "$code" == 2?? ]] || fail "$method $path -> HTTP $code: $(head -c 2000 "$work/body")"
}
check() {
  local path="$1" type="$2"
  req GET "$path"
  [[ "$resp_type" == "$type"* ]] || fail "GET $path content-type $resp_type, expected $type"
  echo "ok: GET $path ($resp_type)"
}

check / text/html
grep -q 'id="root"' "$work/body" || fail "/ is not the React index"
mapfile -t assets < <(grep -o '\./assets/[^"]*\.\(js\|css\)' "$work/body" | sort -u)
[[ ${#assets[@]} -ge 2 ]] || fail "index references ${#assets[@]} assets"
fonts=()
for a in "${assets[@]}"; do
  case "$a" in
    *.js) check "/${a#./}" text/javascript ;;
    *.css)
      check "/${a#./}" text/css
      mapfile -t -O "${#fonts[@]}" fonts < <(grep -o '[^("]*\.woff2' "$work/body" | sort -u) ;;
  esac
done
[[ ${#fonts[@]} -ge 1 ]] || fail "stylesheet references no woff2 fonts"
for f in "${fonts[@]}"; do
  case "$f" in /*) check "$f" font/woff2 ;; *) check "/assets/${f#./}" font/woff2 ;; esac
done
check /licenses text/plain
check /ui text/html
check /api/status application/json

req POST /api/record/start '{}'
sleep 2
req POST /api/record/stop
# Stop spawns identification, which writes the WAV's sidecar (<stem>.json.tmp, then rename)
# while the listing below runs. PAS-2348: that race returned HTTP 500 from the listing.
for _ in $(seq 1 20); do req GET /api/record/files; done
name="$(grep -o '"name":"[^"]*\.wav"' "$work/body" | head -1 | cut -d'"' -f4)"
[[ -n "$name" && -s "$work/rec/$name" ]] || fail "no WAV recording in $(cat "$work/body")"
echo "ok: recorded $name ($(stat -c %s "$work/rec/$name") bytes)"
req POST /api/record/identify "{\"file\":\"$name\"}"
cp "$work/body" "$work/id.json"
grep -q '"status":"untagged"' "$work/id.json" && grep -q 'fpcalc unavailable' "$work/id.json" \
  || fail "identify without helpers: $(cat "$work/id.json")"
echo "ok: identification without fpcalc -> untagged"
echo "PASS: offline smoke ($(cat "$ROOTFS/etc/debian_version"), $("$QEMU" --version | head -1))"
