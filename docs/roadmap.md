# Roadmap

Each milestone ends with a check against a real harness: first adb, then AutoMobile's tools.

## 1. Boot to adb — done

VM core, stock board layout, virtio-blk/rng/vsock, adb bridge, console basics, first-boot
userdata creation, running-instance advertisement. Java-free provisioning in the binary
(`emu -provision`, `-list-images`; `crates/emu-sdk`): manifest resolution, resumable verified
download, unzip and AVD creation, replacing the old shell and Python scripts.

## 2. Boot to home screen (graphics) — next

SurfaceFlinger needs a host renderer. Two routes, both backed by gfxstream:

- **virtio-gpu with gfxstream context types** (`androidboot.hardware.gltransport=virtio-gpu-asg`):
  - a virtio-gpu device with blob resources and a host-visible shared-memory region mapped
    into the guest with `hv_vm_map`;
  - fences;
  - the documented `stream_renderer_*` C API.

  This is the route crosvm and upstream QEMU use. Preferred.

  Confirmed feasible (2026-11-22):
  - The API 36 image ships `virtio-gpu.ko`, and its gfxstream guest libraries
    (`libOpenglSystemCommon`, `vulkan.ranchu`, `libEGL_emulation`) carry the virtio-gpu
    transport selected by `ro.boot.hardware.gltransport`.
  - The SDK's `emulator/lib64/libgfxstream_backend.dylib` exports the whole
    `stream_renderer_*` C API: init, context/resource/blob creation, `submit_cmd`, fences,
    `resource_map`, snapshot/restore. It can be `dlopen`ed from a small FFI crate with no
    other emulator code.
- **goldfish_pipe + goldfish address space** (stock's own transport): more devices, private
  interfaces.

The renderer on a Java-free install — done (2026-11-22): `emu -provision` (or
`-provision-renderer` alone) fetches the `emulator` package on demand and unpacks only its
`lib64` minus Qt (498 MB of the 1.32 GB package, from a 420.6 MB download) into
`<sdk>/emu-rs/renderer/<revision>/`. emu prefers the SDK's own `emulator/lib64` when present.
`Renderer::load` preloads the backend's `@rpath` dependencies leaves-first, so the location
needs no rpath; bare-name `dlopen`s (`libshadertranslator.dylib`) still go through the
executable's rpaths, hence the `lib64` link beside the binary and the
`emu-rs/renderer/current/lib64` rpath. Open: without `-gpu`, `ANDROID_EMU_VK_ICD` is unset and
gfxstream finds no Vulkan ICD ("Failed to create Vulkan instance", Vulkan disabled) with either
renderer location; `moltenvk` and `lavapipe` initialize cleanly, while `swiftshader` aborts
during init (both 37.2.13 and 37.3.10).

Then the display path: scanout frames to the Rune window, zero-copy via IOSurface/Metal where
possible, plus input. virtio-input (touch, keyboard, rotary) and the PL061 power key go in the
same milestone. Done when `sys.boot_completed=1` and `pm list packages` answers, which is
AutoMobile's readiness check.

## 3. AutoMobile harness parity

Point AutoMobile at emu-rs through the SDK overlay (`make shim`, `ANDROID_HOME=~/.emu-rs/sdk`)
and run its tools one by one, fixing whatever breaks:

- **Console commands:** `geo fix`, `sms send`, `gsm`, `sensor set|get`, `finger touch|remove`,
  `network speed|delay|status`, `power`, `rotate`/`fold`/`posture`, `avd snapshot save|load|del`,
  `event send`, `redir`.
- **Pipe services:** sensors, battery, camera, QemuMiscPipe (boot-complete signal).
- **virtio devices:** virtio-console (logcat on `hvc1`, modem vport), virtio-snd, virtio-net or
  wifi with a user-mode NAT.
- **gRPC `EmulatorController`:** audio injection and streaming.
- **Process and launch contract:** the launch-success markers and `qemu-system-*` process
  naming the harness's stop fallback expects.

## 4. Exceed

- Snapshots: save and restore guest RAM and device state. Copy-on-write RAM files make restore
  near-instant.
- virtio-balloon free-page reporting, so idle guests hand memory back to the host.
- ~~`DISCARD`/`WRITE_ZEROES` with `F_PUNCHHOLE`, keeping userdata sparse.~~ Done. `/data`
  sits on dm-default-key, which passes discard but not write-zeroes, so freed space returns
  via discard (online discard or periodic fstrim).
- ~~Native ext4 formatter, removing the dependency on the SDK's `mkfs.ext4`.~~ Done
  (`crates/emu-ext4`): first-boot userdata needs neither the emulator package nor e2fsprogs,
  so the Java-free path (`emu -provision`) is complete. Checked with e2fsck 1.42 (SDK) and
  1.46 (in the guest, at boot); vold encrypts it in place.
- qcow2 read/write, to share AVD state with the stock emulator.
- Multi-instance density: shared read-only image pages and a smaller per-instance footprint.
- Debugging tools in the desktop app: exit and IRQ counters, frame timing, guest console and
  logcat panes.
