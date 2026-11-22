# Fast paths for the inner loop. Everything is plain cargo underneath.
.PHONY: check test lint fmt ci watch desktop desktop-test desktop-snap dist parity harness

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
