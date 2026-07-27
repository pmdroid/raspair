#!/bin/bash
# First-boot helper for a hand-flashed Pi OS Lite card that already has
# vinyl-bridge-pi installed under /usr/local (see build-sd-image.sh).
# Safe to re-run.
set -euo pipefail

mkdir -p /var/lib/vinyl-bridge/rec /etc/vinyl-bridge
if [[ ! -f /etc/vinyl-bridge/config.toml ]]; then
  cp /usr/local/share/vinyl-bridge/config.example.toml /etc/vinyl-bridge/config.toml
fi

# Prefer eth0/wlan0 online before advertising mDNS.
systemctl enable vinyl-bridge.service
systemctl restart vinyl-bridge.service || true

echo "vinyl-bridge: service enabled. Check: systemctl status vinyl-bridge"
echo "API: http://$(hostname -I | awk '{print $1}'):80/api/status"
