################################################################################
#
# vinyl-bridge-pi — prebuilt aarch64 host daemon
#
# The Rust binary is cross-built outside Buildroot (apps/pi4-host
# scripts/build-release.sh → dist/vinyl-bridge-pi). Buildroot only
# installs it into the rootfs so we keep macOS/Docker cargo as the
# compile path and Buildroot as the appliance rootfs path.
#
# deploy/buildroot/build.sh stages dist/vinyl-bridge-pi into
# package/vinyl-bridge-pi/bin/ before invoking make.
#
################################################################################

VINYL_BRIDGE_PI_VERSION = 0.1.0
VINYL_BRIDGE_PI_LICENSE = MIT
VINYL_BRIDGE_PI_DEPENDENCIES = alsa-lib

# Local tree containing the staged ELF (gitignored binary + .gitkeep).
VINYL_BRIDGE_PI_SITE = $(BR2_EXTERNAL_VINYL_PATH)/package/vinyl-bridge-pi/bin
VINYL_BRIDGE_PI_SITE_METHOD = local

define VINYL_BRIDGE_PI_INSTALL_TARGET_CMDS
	@if [ ! -f "$(@D)/vinyl-bridge-pi" ]; then \
		echo "ERROR: missing staged binary in package bin/ (vinyl-bridge-pi)" >&2; \
		echo "  From apps/pi4-host:  make build-pi && make image" >&2; \
		exit 1; \
	fi
	$(INSTALL) -D -m 0755 $(@D)/vinyl-bridge-pi \
		$(TARGET_DIR)/usr/bin/vinyl-bridge-pi
endef

$(eval $(generic-package))
