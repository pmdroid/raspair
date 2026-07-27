#!/usr/bin/env bash
# Copy binary + systemd unit to a running Pi (SSH) — alternative to full SD re-flash.
# Usage: ./scripts/install-on-running-pi.sh vinyl@192.168.1.42
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${1:?usage: $0 user@pi-host}"
BIN="${ROOT}/dist/vinyl-bridge-pi"

if [[ ! -f "$BIN" ]]; then
  echo "missing $BIN — run ./scripts/build-release.sh first" >&2
  exit 1
fi

scp "$BIN" \
  "${ROOT}/deploy/systemd/vinyl-bridge.service" \
  "${ROOT}/config.example.toml" \
  "${ROOT}/deploy/image/first-boot.sh" \
  "${HOST}:/tmp/"

ssh -t "$HOST" 'sudo bash -s' <<'EOS'
set -euo pipefail
install -d /usr/local/bin /etc/vinyl-bridge /var/lib/vinyl-bridge/rec /usr/local/share/vinyl-bridge
install -m 0755 /tmp/vinyl-bridge-pi /usr/local/bin/vinyl-bridge-pi
install -m 0644 /tmp/vinyl-bridge.service /etc/systemd/system/vinyl-bridge.service
install -m 0644 /tmp/config.example.toml /usr/local/share/vinyl-bridge/config.example.toml
if [[ ! -f /etc/vinyl-bridge/config.toml ]]; then
  install -m 0644 /tmp/config.example.toml /etc/vinyl-bridge/config.toml
fi
systemctl daemon-reload
systemctl enable --now vinyl-bridge.service
systemctl status vinyl-bridge.service --no-pager || true
EOS

echo "Installed on $HOST"
