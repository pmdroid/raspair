#!/usr/bin/env bash
# Copy the binary to a running Pi (SSH) and run its installer — alternative to full SD re-flash.
# Usage: ./scripts/install-on-running-pi.sh vinyl@192.168.1.42
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${1:?usage: $0 user@pi-host}"
BIN="${ROOT}/dist/vinyl-bridge-pi"

if [[ ! -f "$BIN" ]]; then
  echo "missing $BIN — run ./scripts/build-release.sh first" >&2
  exit 1
fi

scp "$BIN" "${HOST}:/tmp/vinyl-bridge-pi"
ssh -t "$HOST" 'sudo /tmp/vinyl-bridge-pi install; rc=$?; rm -f /tmp/vinyl-bridge-pi; systemctl status vinyl-bridge.service --no-pager || true; exit $rc'

echo "Installed on $HOST"
