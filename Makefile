# Fast paths for the inner loop. Everything is plain cargo underneath.
.PHONY: check test lint fmt ci run desktop desktop-test desktop-snap dist shim harness

AVD ?= $(shell cargo run -q -p emu -- -list-avds 2>/dev/null | head -1)

check:            ## type-check everything (no codegen, no link)
	cargo check --all-targets

test:             ## unit tests (no hypervisor needed)
	@command -v cargo-nextest >/dev/null && cargo nextest run || cargo test

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --all

ci: fmt lint test ## what CI runs, locally

run:              ## boot $(AVD) with the guest console on stdout
	cargo run -p emu -- -avd $(AVD) -show-kernel -verbose

desktop:
	cd desktop && cargo run

desktop-test:
	cd desktop && cargo test

desktop-snap:     ## render the desktop app headless
	cd desktop && cargo run -- --snap ../docs/desktop-demo.png --scene demo

dist:             ## smallest, fastest binary (thin LTO, 1 CGU), signed
	cargo build --profile dist -p emu
	codesign --sign - --force --entitlements scripts/hypervisor.entitlements target/dist/emu

shim:             ## SDK overlay whose emulator is emu-rs (for auto-mobile and other tools)
	scripts/sdk-shim.sh

harness:          ## run AutoMobile's tools against emu-rs: make harness DEVICE=emulator-5554 (or @avd)
	scripts/automobile-harness.sh $(DEVICE)
