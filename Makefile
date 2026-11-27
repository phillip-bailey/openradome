# Build on the Mac, run on the Pi 2 (armv7, 32-bit hard-float).
# Needs: rustup stable, zig, cargo-zigbuild. rustup is keg-only on Homebrew:
#   export PATH=/opt/homebrew/opt/rustup/bin:$$PATH
# Pi login. Directory is relative to its home. glibc: the Pi has 2.41, building against older is fine.
PI     ?= pi@raspberrypi.local
PI_DIR ?= openradome
TARGET ?= armv7-unknown-linux-gnueabihf
GLIBC  ?= 2.31

.PHONY: check test fuzz pi push
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

test: check
	cargo test

# A longer fuzz hunt than the one `cargo test` runs (crates/openradome-core/tests/fuzz.rs):
#   make fuzz FUZZ_ITERS=50000000 FUZZ_SEED=7
FUZZ_ITERS ?= 5000000
FUZZ_SEED  ?= $(shell date +%s)
fuzz:
	FUZZ_ITERS=$(FUZZ_ITERS) FUZZ_SEED=$(FUZZ_SEED) cargo test --release -p openradome-core --test fuzz

pi:
	cargo zigbuild --release --target $(TARGET).$(GLIBC)

push: pi
	ssh $(PI) 'mkdir -p $(PI_DIR)'
	rsync -avz --progress target/$(TARGET)/release/openradome $(PI):$(PI_DIR)/openradome
