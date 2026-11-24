# Build on the Mac, run on the Pi 2 (armv7, 32-bit hard-float).
# Needs: rustup stable, zig, cargo-zigbuild. rustup is keg-only on Homebrew:
#   export PATH=/opt/homebrew/opt/rustup/bin:$$PATH
# Pi login. Directory is relative to its home. glibc: the Pi has 2.41, building against older is fine.
PI     ?= pi@raspberrypi.local
PI_DIR ?= rustadsb
TARGET ?= armv7-unknown-linux-gnueabihf
GLIBC  ?= 2.31

.PHONY: check test pi push release-mac
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test: check
	cargo test

pi:
	cargo zigbuild --release --target $(TARGET).$(GLIBC)

push: pi
	ssh $(PI) 'mkdir -p $(PI_DIR)'
	rsync -avz --progress target/$(TARGET)/release/rustadsb $(PI):$(PI_DIR)/rustadsb

# Add the macOS archives to CI's release for TAG (CI cannot build macOS). Usage: make release-mac TAG=v0.1.0
release-mac:
	tools/release-mac.sh $(TAG)
