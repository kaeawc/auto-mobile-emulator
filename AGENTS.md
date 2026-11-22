# emu-rs

Android emulator in Rust on Hypervisor.framework (Apple silicon). Boots unmodified SDK system
images and matches the official emulator's board, ports and control surfaces.

## Working rules

- `unsafe` lives only in `crates/emu-hvf` (FFI and guest RAM), each block with a `// SAFETY:`
  comment. Everything else is `#![deny(unsafe_code)]` via workspace lints; `apps/emu` allows it
  in exactly one place (signal handlers).
- Guest-visible layout follows the stock board (`crates/emu-vmm/src/layout.rs`,
  `docs/reference/stock-board.md`): images hard-code device addresses (fstab, boot_devices).
  Change it only with evidence from a running stock emulator.
- Measure before optimizing: `-verbose` prints per-vCPU exit counts; `sample <pid> 5` shows host
  time per thread. Record results that change a design decision in the README's performance notes.
- Never modify the stock emulator's files in an AVD directory; emu-rs state lives in
  `<avd>/emu-rs/`.
- New dependencies go in root `[workspace.dependencies]` with `default-features = false`.
- `desktop/` is a separate workspace (Rune git dependency); keep its Rune `rev` and
  `[patch.crates-io]` entries in sync.
- Running guests: start them with a recorded PID and stop them with `kill -INT <pid>` or
  `adb -s emulator-<port> emu kill`; don't kill by name (other emulators may be running).

## Build & validate

```bash
cargo c                      # check --all-targets
cargo lint                   # clippy --all-targets -D warnings
cargo t                      # unit tests (no hypervisor needed)
cargo run -p emu -- -avd <name> -show-kernel -verbose   # boot (signed by scripts/sign-run.sh)
cd desktop && cargo test     # headless Rune view tests
```

After any change: `cargo c && cargo lint && cargo t`; after VMM changes also boot an AVD and
check `adb -s emulator-5554 shell getprop sys.boot_completed`.

## Map

- `crates/emu-hvf` — `Vm` (GIC, RAM mapping, SPIs), `Vcpu` (run/regs/vtimer), `GuestRam`.
- `crates/emu-vmm` — `lib.rs` (`run`), `layout.rs`, `board.rs` (DT), `vcpu.rs` (exits, PSCI,
  wakers), `bus.rs`, `devices/` (PL011, PL031), `virtio/` (mmio transport, queue, blk, rng,
  vsock), `pipe/` (adb bridge, qemud services), `disk.rs`, `boot.rs`.
- `crates/emu-avd` — `Env` (SDK/AVD paths), `Avd`, `BootConfig`, ini parsing.
- `apps/emu` — `args.rs` (SDK emulator flags), `instance.rs` (disks, bootconfig, boot props),
  `console.rs` (console server, running-instance file).
- `desktop/` — Rune app: `backend.rs`, `view.rs`, `main.rs` (state, actions, poll, tests).
