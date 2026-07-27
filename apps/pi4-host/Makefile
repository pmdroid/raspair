# Vinyl Bridge host — macOS fast loop + Pi Buildroot appliance
#
#   make build-macos    # native macOS (cpal/CoreAudio) — daily dev
#   make run-macos      # listen on :8080
#   make build-pi       # aarch64 Linux + ALSA (Docker)
#   make image          # Buildroot sdcard.img (NOT Raspberry Pi OS)
#   make qemu-smoke     # binary (+ virt if QEMU present)
#   make test-macos     # curl smoke against a short-lived process

ROOT := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
CARGO ?= cargo
HTTP_BIND ?= 127.0.0.1:8080

.PHONY: help build-macos run-macos build-pi qemu-smoke qemu-binary test-macos image image-legacy-pios clean

help:
	@echo "Targets:"
	@echo "  build-macos        native macOS binary (default features = cpal/CoreAudio)"
	@echo "  run-macos          cargo run --features cpal on $(HTTP_BIND)"
	@echo "  build-pi           aarch64-linux + ALSA → dist/vinyl-bridge-pi (Docker)"
	@echo "  qemu-smoke         scripts/qemu-smoke.sh default"
	@echo "  qemu-binary        scripts/qemu-smoke.sh binary"
	@echo "  image              Buildroot appliance → flashable .img / .img.xz"
	@echo "  image-legacy-pios  DEPRECATED: inject into Raspberry Pi OS Lite"
	@echo "  test-macos         build + 2s HTTP smoke on $(HTTP_BIND)"
	@echo "  clean              cargo clean + dist/"

# --- macOS daily loop (preferred) ---

build-macos:
	$(CARGO) build --release --features cpal
	@echo "→ target/release/vinyl-bridge-pi  (macOS, cpal/CoreAudio)"
	@file target/release/vinyl-bridge-pi || true

run-macos:
	$(CARGO) run --features cpal -- --http-bind $(HTTP_BIND)

# Debug build is fine for iterate-on-API cycles
build-macos-debug:
	$(CARGO) build --features cpal

test-macos: build-macos-debug
	@pkill -x vinyl-bridge-pi 2>/dev/null || true
	@./target/debug/vinyl-bridge-pi --http-bind $(HTTP_BIND) --log warn & \
		pid=$$!; \
		sleep 1; \
		curl -fsS "http://$(HTTP_BIND)/api/status" | head -c 400; echo; \
		kill $$pid 2>/dev/null || true; \
		wait $$pid 2>/dev/null || true; \
		echo "PASS test-macos"

# --- Raspberry Pi aarch64 deploy artifact ---

build-pi:
	./scripts/build-release.sh
	@echo "→ dist/vinyl-bridge-pi  (linux aarch64, alsa)"
	@file dist/vinyl-bridge-pi || true

# --- QEMU / image validation (deploy gate, not daily loop) ---

qemu-smoke:
	./scripts/qemu-smoke.sh

qemu-binary:
	./scripts/qemu-smoke.sh binary

# --- Buildroot appliance (preferred flash path) ---

image:
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		./deploy/buildroot/build.macos.sh; \
	else \
		./deploy/buildroot/build.sh; \
	fi

# DEPRECATED: Raspberry Pi OS Lite inject (multi-GB Debian). Prefer `make image`.
image-legacy-pios:
	@echo "WARNING: image-legacy-pios is DEPRECATED — use Buildroot (\`make image\`)" >&2
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		./deploy/image/build-sd-image.macos.sh; \
	else \
		./deploy/image/build-sd-image.sh; \
	fi

clean:
	$(CARGO) clean
	rm -rf dist/target dist/qemu-work
