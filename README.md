# emu-rs

An Android emulator written from scratch in Rust for Apple silicon. It boots the SDK's
unmodified arm64 system images on Hypervisor.framework and presents the same board, ports and
control surfaces as the official emulator, so existing tooling (`adb`, `adb emu`, test
harnesses such as [AutoMobile](https://github.com/kaeawc/auto-mobile)) can drive it unchanged.
The goals are to be able to run in the official emulator's place, to be measurably faster, and
to use fewer resources.

## Status

| Area | State |
| --- | --- |
| VM core | vCPUs, the framework's in-kernel GICv3, PSCI (SMP, power off and reset), arch timer, PL011 console, PL031 RTC |
| Board | Stock memory map and virtio-mmio slots, so the unmodified fstab and first-stage init find `vda`–`vde` and their by-name paths |
| Storage | virtio-blk with indirect descriptors and event-idx. System and vendor are copy-on-write in memory; userdata, cache and metadata persist per AVD |
| Boot | Kernel, first-stage init, dm-linear `super`, second-stage init, metadata encryption of `/data`, APEX activation, adbd |
| adb | virtio-vsock with a host bridge on `console port + 1`, announced to the adb server. `adb shell`/`root`/`push` work, and the device shows up about 8 s after launch on a warm boot |
| Console | `adb emu` / telnet console with token auth: `avd name`, `ping`, `kill` |
| Discovery | `-list-avds`, `pid_<pid>.ini` running-instance file, `emulator-<port>` serial on stdout |
| Graphics | **Next milestone.** SurfaceFlinger needs a host GLES/Vulkan renderer (gfxstream); until then the framework doesn't finish booting |
| Desktop app | Rune app (`desktop/`): AVD list, start/stop, live guest console |

## Quick start

```bash
cargo run -p emu -- -list-avds
cargo run -p emu -- -avd <name> -show-kernel -verbose   # first boot formats and encrypts /data
adb -s emulator-5554 shell                              # once adbd is up
adb -s emulator-5554 emu kill
make shim && export ANDROID_HOME=~/.emu-rs/sdk          # make tools launch emu-rs as "emulator"
cd desktop && cargo run                                 # desktop app
```

`cargo run` signs binaries with the `com.apple.security.hypervisor` entitlement
(`scripts/sign-run.sh` is the cargo runner); Hypervisor.framework refuses unsigned callers.

Flags: `-avd`/`@name`, `-port`, `-cores`, `-memory`, `-read-only` (all disk writes kept in
memory, several instances per AVD), `-wipe-data`, `-show-kernel`, `-verbose` (prints vCPU exit
counts at shutdown), `-no-boot-anim`. The rest of the SDK emulator's flags are accepted and
ignored for now.

## Layout

| Path | What |
| --- | --- |
| `crates/emu-hvf` | Safe wrappers over Hypervisor.framework: VM, vCPUs, GIC, guest RAM with zero-copy file I/O. **The only crate with `unsafe`.** |
| `crates/emu-vmm` | The machine: memory map (`layout.rs`), device tree (`board.rs`), vCPU loop and PSCI (`vcpu.rs`), MMIO bus, PL011/PL031, virtio-mmio transport, blk, rng, vsock, host pipe services (adb bridge, `qemud:boot-properties`) |
| `crates/emu-avd` | AVD discovery and config (`~/.android/avd`, `config.ini`, system image), bootconfig encoding |
| `crates/emu-fdt` | Flattened device tree writer |
| `apps/emu` | The `emu` CLI: SDK-emulator-compatible flags, per-AVD instance state, console server |
| `desktop/` | Rune desktop app, its own workspace |
| `docs/` | [Stock board reference](docs/reference/stock-board.md), [desktop and display design](docs/design/desktop-and-display.md), [roadmap](docs/roadmap.md) |
| `scripts/` | Cargo runner that signs with the hypervisor entitlement, SDK overlay (`make shim`), AutoMobile harness (`make harness DEVICE=emulator-5554`) |

## Benchmarks

`scripts/bench.sh` boots an AVD the same way under either emulator and measures it. Results
below are from one API 36 `google_apis` AVD (4 vCPU, 2 GB) on an M-series Mac, 2026-11-22. Stock
was cold-booted (`-no-snapshot-load`).

| | emu-rs | stock |
| --- | --- | --- |
| launch → `adb device` | **14.5 s** | 25.8 s |
| peak RSS (process tree) | **1.67 GB** | 5.09 GB |
| `adb shell echo` (median of 3) | **0.41 s** | 0.80 s |
| guest CPU: shell loop / 4× md5 | 0.74 s / 0.96 s | 0.72 s / 0.89 s |
| `sys.boot_completed` | not yet (needs graphics) | 51 s |

Guest CPU speed is equal, as expected: both run guest code natively. The differences are in
the VMM: no userspace GIC, fewer device threads, and RAM that is only committed when touched.

## Provisioning without Java

emu-rs needs only a system image and platform-tools (adb); the stock emulator package, Java,
sdkmanager and avdmanager aren't required.

```bash
scripts/provision.sh 36 google_apis small36    # resolve, verify (size + sha1), unpack, create the AVD
LCD=480x800@240 CORES=2 scripts/provision.sh 35 google_apis tiny35
cargo run -p emu -- -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a -lcd 720x1280@320
```

`scripts/sdk-resolve.py` reads Google's repository manifests directly. AVD defaults favour
cheap, fast boots: 720×1280 screen, 2 GB RAM, 4 vCPU, 4 GB data. Benchmarks of the stock
emulator found that smaller screens boot markedly faster and that RAM above 2 GB slows cold
boots down.

## Performance notes

What has mattered so far, measured with `sample` and the `-verbose` exit counters:

- **In-kernel GICv3.** The stock board emulates a GICv2 in userspace. Here interrupt
  acknowledge, EOI and the virtual timer never leave the guest, and an idle vCPU parks inside
  the framework (zero WFI exits in practice).
- **Kick on interrupt.** A vCPU idling inside the framework doesn't notice a newly raised SPI
  until its own timer fires. Kicking vCPUs out of `hv_vcpu_run` after raising one cut
  first-boot `/data` encryption from 22 s to 8 s.
- **Plain `fsync` for guest flushes.** `std`'s `sync_data` is `F_FULLFSYNC` on macOS, which
  drains the drive cache. It was 75% of the userdata disk thread's time, and APEX
  decompression on first boot took more than 270 s; with plain `fsync` it takes about 70 s.
  Writes already sit in the host page cache, so an emulator crash loses nothing either way.
- **Zero-copy disk I/O.** `pread`/`pwrite` go straight between the image and guest RAM, one
  worker thread per disk, so the vCPU that kicked a queue goes back to the guest at once.
- **Sparse, lazily committed memory.** Guest RAM is an untouched anonymous mapping and the
  userdata image is a sparse file.

## Development

```bash
cargo c && cargo lint && cargo t    # check, clippy -D warnings, tests (no hypervisor needed)
make run AVD=<name>                 # boot with the console on stdout
cd desktop && cargo test            # headless Rune view tests
```
