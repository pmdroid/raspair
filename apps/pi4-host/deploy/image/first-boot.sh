#!/bin/bash
# DEPRECATED with deploy/image/* (Pi OS Lite path).
# Buildroot appliances use busybox /etc/init.d/S99vinyl-bridge instead.
#
# First-boot helper for a hand-flashed Pi OS Lite card that already has
# vinyl-bridge-pi installed under /usr/local (see build-sd-image.sh).
# Safe to re-run.
set -euo pipefail

/usr/local/bin/vinyl-bridge-pi install

echo "vinyl-bridge: service enabled. Check: systemctl status vinyl-bridge"
echo "API: http://$(hostname -I | awk '{print $1}'):80/api/status"
