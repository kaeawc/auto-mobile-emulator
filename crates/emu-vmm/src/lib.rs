//! The emu-rs virtual machine: an arm64 board that boots unmodified Android emulator system
//! images on Hypervisor.framework.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

pub mod board;
pub mod boot;
pub mod bus;
pub mod devices;
pub mod disk;
pub mod irq;
pub mod layout;
pub mod pipe;
pub mod vcpu;
pub mod virtio;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use emu_hvf::{GuestRam, Vm};

use bus::Bus;
use irq::{GicSpi, IrqLine};
use layout::*;
pub use vcpu::StopReason;
use vcpu::{Cpus, Wakers};
use virtio::Interrupt;
use virtio::mmio::MmioTransport;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskMode {
    /// Guest writes go to the file.
    Persistent,
    /// Guest writes are kept in memory and discarded at exit.
    Ephemeral,
}

#[derive(Clone, Debug)]
pub struct DiskSpec {
    pub slot: u32,
    pub path: PathBuf,
    pub mode: DiskMode,
    pub serial: String,
}

/// Host GPU rendering through gfxstream.
pub struct GpuSpec {
    /// The SDK's `emulator/lib64` (holds `libgfxstream_backend.dylib`).
    pub lib64: PathBuf,
    pub width: u32,
    pub height: u32,
    pub verbose: bool,
    pub display: Arc<dyn virtio::gpu::DisplaySink>,
}

/// The renderer is process-wide: loaded and initialized once, reused across guest reboots.
fn renderer(spec: &GpuSpec) -> Result<Arc<emu_gfxstream::Renderer>> {
    static RENDERER: std::sync::OnceLock<Arc<emu_gfxstream::Renderer>> = std::sync::OnceLock::new();
    if let Some(r) = RENDERER.get() {
        return Ok(r.clone());
    }
    let r = emu_gfxstream::Renderer::load(&spec.lib64).context("loading the gfxstream renderer")?;
    let flags = emu_gfxstream::flags::USE_GLES | emu_gfxstream::flags::USE_VK;
    r.init(spec.width, spec.height, flags, spec.verbose, virtio::gpu::fence_handler()).context("initializing the gfxstream renderer")?;
    Ok(RENDERER.get_or_init(|| Arc::new(r)).clone())
}

pub struct MachineConfig {
    pub cpus: u32,
    pub ram_bytes: u64,
    pub kernel: PathBuf,
    /// Initrd bytes, bootconfig already appended.
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    pub disks: Vec<DiskSpec>,
    pub rng: bool,
    /// Host TCP port bridged to the guest's adbd (stock: console port + 1).
    pub adb_port: Option<u16>,
    pub gpu: Option<GpuSpec>,
    /// Served to the guest's `qemu-props` over the `qemud:boot-properties` pipe.
    pub boot_props: Vec<(String, String)>,
    /// Where guest console (PL011) output goes.
    pub console: Box<dyn Write + Send>,
}

pub struct Outcome {
    pub reason: StopReason,
    pub uptime: Duration,
    /// Per-vCPU exit counters: (name, count).
    pub exits: Vec<Vec<(&'static str, u64)>>,
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b
}

/// Boots the machine and blocks until it powers off, resets, faults, or `stop` becomes true.
pub fn run(cfg: MachineConfig, stop: Arc<AtomicBool>) -> Result<Outcome> {
    let started = Instant::now();
    if cfg.cpus == 0 || cfg.cpus > 16 {
        bail!("cpus must be in 1..=16 (got {})", cfg.cpus);
    }
    let vm = Vm::new().context("creating the VM (is the binary signed with com.apple.security.hypervisor?)")?;
    let mem = Arc::new(GuestRam::new(RAM_BASE, cfg.ram_bytes)?);
    vm.map_ram(&mem)?;
    let gic = vm.create_gic(GIC_DIST_BASE, GIC_REDIST_BASE)?;
    let redist_size = u64::from(cfg.cpus) * 0x2_0000;
    if gic.redist_base + redist_size > GIC_REDIST_LIMIT || redist_size > gic.redist_region_size {
        bail!("GIC redistributors for {} vCPUs don't fit the memory map", cfg.cpus);
    }
    let max_spi = VIRTIO_MMIO_SPI_BASE + VIRTIO_MMIO_SLOTS;
    if spi_intid(0) < gic.spi_base || spi_intid(max_spi) >= gic.spi_base + gic.spi_count {
        bail!("GIC SPI range {}+{} doesn't cover the board's interrupts", gic.spi_base, gic.spi_count);
    }

    // Kernel, initrd, device tree.
    let kernel = boot::parse_kernel(std::fs::read(&cfg.kernel).with_context(|| format!("reading {}", cfg.kernel.display()))?)?;
    let entry = KERNEL_ADDR + kernel.text_offset;
    if entry + kernel.image_size > INITRD_ADDR {
        bail!("kernel image ({} bytes) overlaps the initrd area", kernel.image_size);
    }
    mem.write(entry, &kernel.image)?;
    let initrd = match &cfg.initrd {
        Some(bytes) => {
            if INITRD_ADDR + bytes.len() as u64 > mem.end() {
                bail!("initrd doesn't fit in guest RAM");
            }
            mem.write(INITRD_ADDR, bytes)?;
            Some((INITRD_ADDR, INITRD_ADDR + bytes.len() as u64))
        }
        None => None,
    };
    let dtb = board::build(&board::BoardInfo {
        cpus: cfg.cpus,
        ram_bytes: cfg.ram_bytes,
        gic_dist: (gic.dist_base, gic.dist_size),
        gic_redist: (gic.redist_base, redist_size),
        cmdline: &cfg.cmdline,
        initrd,
        virtio_slots: VIRTIO_MMIO_SLOTS,
        rng_seed: random_bytes(),
        kaslr_seed: u64::from_le_bytes(random_bytes()),
    })?;
    if dtb.len() as u64 > DTB_MAX {
        bail!("device tree too large");
    }
    mem.write(DTB_ADDR, &dtb)?;

    // Devices.
    let wakers = Arc::new(Wakers::new(cfg.cpus as usize));
    let wake: Arc<dyn Fn() + Send + Sync> = {
        let w = wakers.clone();
        Arc::new(move || w.wake_all())
    };
    let spi = |n: u32| -> Arc<dyn IrqLine> { Arc::new(GicSpi::new(spi_intid(n), wake.clone())) };

    let mut bus = Bus::new();
    bus.insert(UART_BASE, PERIPH_SIZE, Arc::new(devices::pl011::Pl011::new(spi(UART_SPI), cfg.console)));
    bus.insert(RTC_BASE, PERIPH_SIZE, Arc::new(devices::pl031::Pl031::default()));
    let mut slots: Vec<Option<Box<dyn virtio::VirtioDevice>>> = (0..VIRTIO_MMIO_SLOTS).map(|_| None).collect();
    for d in &cfg.disks {
        let disk = match d.mode {
            DiskMode::Persistent => disk::Disk::persistent(&d.path),
            DiskMode::Ephemeral => disk::Disk::ephemeral(&d.path),
        }
        .with_context(|| format!("opening {}", d.path.display()))?;
        let slot = slots.get_mut(d.slot as usize).with_context(|| format!("virtio slot {} out of range", d.slot))?;
        *slot = Some(Box::new(virtio::blk::Blk::new(disk, d.serial.clone())));
    }
    if cfg.rng {
        slots[slots::RNG as usize] = Some(Box::new(virtio::rng::Rng::new()));
    }
    if let Some(spec) = &cfg.gpu {
        let gpu = virtio::gpu::Gpu::new(virtio::gpu::GpuConfig {
            width: spec.width,
            height: spec.height,
            renderer: renderer(spec)?,
            shm: (GPU_SHM_BASE, GPU_SHM_SIZE),
            display: spec.display.clone(),
        });
        slots[slots::GPU as usize] = Some(Box::new(gpu));
    }
    if let Some(port) = cfg.adb_port {
        let (events, rx) = virtio::vsock::event_channel();
        let svc = pipe::adb::AdbService::bind(port, events.clone()).with_context(|| format!("binding adb port {port}"))?;
        let mut registry = pipe::PipeRegistry::default();
        registry.register("qemud:adb", Box::new(svc));
        registry.register("qemud:boot-properties", Box::new(pipe::qemud::BootProperties { props: cfg.boot_props.clone() }));
        slots[slots::VSOCK as usize] = Some(Box::new(virtio::vsock::Vsock::new(3, registry, events, rx)));
        pipe::adb::AdbService::announce_port(port);
    }
    for (i, dev) in slots.into_iter().enumerate() {
        let n = i as u32;
        let irq = Arc::new(Interrupt::new(spi(VIRTIO_MMIO_SPI_BASE + n)));
        bus.insert(virtio_slot_addr(n), VIRTIO_MMIO_STRIDE, Arc::new(MmioTransport::new(dev, irq, mem.clone())));
    }

    // Guest counter starts near zero, identical on every vCPU.
    let cpus = Arc::new(Cpus::new(bus, wakers, emu_hvf::host_ticks()));
    let mut threads = Vec::new();
    for idx in 0..cpus.count() {
        let c = cpus.clone();
        let boot = (idx == 0).then_some((entry, DTB_ADDR));
        threads.push(std::thread::Builder::new().name(format!("vcpu{idx}")).spawn(move || c.thread_main(idx, boot))?);
    }

    let reason = loop {
        if let Some(r) = cpus.wait_stop(Duration::from_millis(100)) {
            break r;
        }
        if stop.load(Ordering::SeqCst) {
            cpus.request_stop(StopReason::Requested);
        }
    };
    cpus.request_stop(reason.clone());
    for t in threads {
        let _ = t.join();
    }
    let exits = cpus.stats.iter().map(|s| s.snapshot().to_vec()).collect();
    // Devices (and their worker threads) go first, then the VM, then guest RAM.
    drop(cpus);
    drop(vm);
    drop(mem);
    Ok(Outcome { reason, uptime: started.elapsed(), exits })
}
