# Build ember (Rust) and, on macOS, ember-vz (Swift).
# Places both binaries side-by-side in target/{debug,release}/ so ember
# can find ember-vz at runtime.

UNAME := $(shell uname -s)

.PHONY: build release clean fmt check clippy test udeps emberd

build:
	cargo build
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift build
	# SEC-445: codesign the FINAL binary location after cp, not the source.
	# `cp` invalidates the embedded code signature (the kernel rejects the
	# first text page at runtime with "tainted:1"), causing every spawn to
	# SIGKILL via the Code Signing Monitor before reaching dyld's `start`.
	# Symptom: "ember-vz closed ready-fd without writing MAC address" with
	# an empty per-VM log (helper dies before any user code runs).
	cp ember-vz/.build/debug/ember-vz target/debug/
	codesign --force --sign - --entitlements ember-vz/entitlements.plist target/debug/ember-vz
endif

release:
	cargo build --release
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift build -c release
	# SEC-445: same fix as `build` — sign the final location, not the source.
	cp ember-vz/.build/release/ember-vz target/release/
	codesign --force --sign - --entitlements ember-vz/entitlements.plist target/release/ember-vz
endif

# Build emberd (in-VM daemon). Runs inside Linux VMs so the vsock listener
# only compiles on Linux, but UDS-only mode works on macOS for testing.
emberd:
	cargo build -p emberd

emberd-release:
	cargo build -p emberd --release

# Build emberd for Linux and stage at images/emberd for Dockerfile COPY.
# Uses Docker (via Colima on macOS) so no cross-compilation toolchain needed.
emberd-image:
ifeq ($(UNAME),Linux)
	cargo build -p emberd --release
	cp target/release/emberd images/emberd
else
	docker run --rm -v "$(CURDIR)":/src -w /src \
		-e CARGO_TARGET_DIR=/tmp/emberd-target \
		rust:latest \
		sh -c 'cargo build -p emberd --release && cp /tmp/emberd-target/release/emberd /src/images/emberd'
endif
	@echo "emberd binary staged at images/emberd"

clean:
	cargo clean
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift package clean
endif

fmt:
	cargo fmt --all

check:
	cargo check --workspace

clippy:
	cargo clippy --workspace -- -D warnings

test:
	cargo test --workspace

udeps:
	cargo machete
