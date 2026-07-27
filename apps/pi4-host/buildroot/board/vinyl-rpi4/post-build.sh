#!/bin/sh
# Vinyl Bridge post-build: ensure overlay init scripts are executable and
# drop a tiny MOTD. Runs after the stock board/raspberrypi/post-build.sh
# (configured first in BR2_ROOTFS_POST_BUILD_SCRIPT).
set -eu

TARGET="${TARGET_DIR:?TARGET_DIR unset}"

# Busybox SysV-style init scripts
for s in S99vinyl-bridge; do
	if [ -f "${TARGET}/etc/init.d/${s}" ]; then
		chmod 0755 "${TARGET}/etc/init.d/${s}"
	fi
done

# Ensure data dirs exist even if overlay is incomplete
mkdir -p "${TARGET}/var/lib/vinyl-bridge/rec" \
	"${TARGET}/etc/vinyl-bridge"

# Prefer root login over serial/HDMI for bring-up; password set via defconfig
if [ -e "${TARGET}/etc/inittab" ]; then
	# Allow root login on console (already typical for busybox)
	:
fi

printf '%s\n' \
	'Vinyl Bridge appliance (Buildroot)' \
	'  API:  http://<ip>/api/status' \
	'  SSH:  root@<ip>  (password: see BR2_TARGET_GENERIC_ROOT_PASSWD / docs)' \
	'  Config: /etc/vinyl-bridge/config.toml' \
	> "${TARGET}/etc/issue"

# Drop a version stamp for field diagnostics
{
	echo "vinyl-bridge-appliance"
	echo "built_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
} > "${TARGET}/etc/vinyl-bridge/image-id"
