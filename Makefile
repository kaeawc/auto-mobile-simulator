# Fast paths for the inner loop. Everything is plain cargo underneath.
.PHONY: check test lint fmt ci watch desktop desktop-test desktop-snap dist parity harness emu-run emu-dist shim harness-android

check:            ## type-check everything (no codegen, no link): the inner loop
	cargo check --all-targets

test:             ## run tests with nextest (falls back to cargo test)
	@command -v cargo-nextest >/dev/null && cargo nextest run || cargo test

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --all
	cd desktop && cargo fmt

ci: fmt lint test ## what CI runs, locally

watch:            ## re-check on save (bacon)
	bacon

desktop:
	cd desktop && cargo run --release

desktop-test:
	cd desktop && cargo test

desktop-snap:     ## render the desktop app headless to shot.png
	cd desktop && cargo run -- --snap ../shot.png --scene demo

dist:             ## smallest, fastest CLI binary
	cargo build --profile dist -p sim-rs

parity:           ## make parity UDID=<udid>
	cargo build -p sim-rs && scripts/parity.sh $(UDID)

harness:          ## make harness UDID=<udid>
	scripts/automobile-harness.sh $(UDID)

# Android (emu-rs): boot an AVD with the guest console on stdout. make emu-run AVD=<name>
AVD ?= $(shell cargo run -q -p emu -- -list-avds 2>/dev/null | head -1)
emu-run:
	cargo run -p emu -- -avd $(AVD) -show-kernel -verbose

emu-dist:         ## smallest, fastest emulator binary (thin LTO, 1 CGU), signed
	cargo build --profile dist -p emu
	codesign --sign - --force --entitlements scripts/hypervisor.entitlements target/dist/emu

shim:             ## SDK overlay whose emulator is emu-rs (for auto-mobile and other tools)
	scripts/sdk-shim.sh

harness-android:  ## AutoMobile's tools against emu-rs: make harness-android DEVICE=emulator-5554 (or @avd)
	scripts/automobile-harness-android.sh $(DEVICE)
