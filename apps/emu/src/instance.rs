//! Per-AVD runtime state: disk images under `<avd>/emu-rs/`, the instance lock, boot config.
//!
//! The stock emulator's own files in the AVD directory (qcow2 overlays, snapshots) are never
//! touched, so the same AVD keeps working with both.
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use emu_avd::{Avd, BootConfig};
use emu_vmm::layout::slots;
use emu_vmm::{DiskMode, DiskSpec, MachineConfig};

use crate::args::Opts;

const CACHE_BYTES: u64 = 66 << 20;

/// Kernel arguments the stock board passes for ranchu images.
const BASE_CMDLINE: &str = "8250.nr_uarts=1 no_timer_check console=ttyAMA0,38400 keep_bootcon earlyprintk=ttyAMA0 \
                            ndns=2 loop.max_part=7 printk.devkmsg=on bootconfig mac80211_hwsim.radios=0";

pub struct Instance {
    avd: Avd,
    dir: PathBuf,
    read_only: bool,
    /// Console port (the `emulator-<port>` serial); adb listens on port + 1.
    pub console_port: u16,
    _lock: Option<File>,
}

impl Instance {
    pub fn prepare(avd: &Avd, opts: &Opts) -> Result<Instance> {
        let dir = avd.dir.join("emu-rs");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = if opts.read_only {
            None
        } else {
            let f = File::create(dir.join("instance.lock"))?;
            if f.try_lock().is_err() {
                bail!("AVD '{}' is already running (use -read-only to start another instance)", avd.name);
            }
            Some(f)
        };
        if opts.wipe_data {
            if opts.read_only {
                bail!("-wipe-data and -read-only can't be combined");
            }
            for f in ["userdata.img", "cache.img", "encryptionkey.img"] {
                let p = dir.join(f);
                if p.exists() {
                    std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                }
            }
        }
        // User data starts as an empty ext4 filesystem (like stock): the guest mounts it, then
        // vold sets up metadata encryption in place. The metadata partition starts blank and is
        // formatted by the guest (`formattable` in the fstab).
        let userdata = dir.join("userdata.img");
        if !userdata.exists() {
            emu_vmm::disk::ensure_sparse(&userdata, avd.data_partition_bytes())?;
            if let Err(e) = format_ext4(avd, &userdata) {
                let _ = std::fs::remove_file(&userdata);
                return Err(e);
            }
        }
        emu_vmm::disk::ensure_sparse(&dir.join("cache.img"), CACHE_BYTES)?;
        let key_src = avd.sysdir.join("encryptionkey.img");
        if key_src.is_file() {
            emu_vmm::disk::ensure_copy(&key_src, &dir.join("encryptionkey.img"))?;
        } else {
            emu_vmm::disk::ensure_sparse(&dir.join("encryptionkey.img"), 18 << 20)?;
        }
        let console_port = match opts.port {
            Some(p) => p,
            None => free_port_pair().context("no free console/adb port pair in 5554..5682")?,
        };
        Ok(Instance { avd: avd.clone(), dir, read_only: opts.read_only, console_port, _lock: lock })
    }

    fn bootconfig(&self, opts: &Opts) -> BootConfig {
        let (w, h, _density) = self.avd.lcd();
        let mut bc = BootConfig::new();
        bc.set("androidboot.hardware", "ranchu")
            .set("androidboot.boot_devices", "a003600.virtio_mmio")
            .set("androidboot.qemu", "1")
            .set("androidboot.qemu.avd_name", self.avd.name.as_str())
            .set("androidboot.serialno", "EMULATOR0EMURS0")
            .set("androidboot.veritymode", "enforcing")
            .set("androidboot.dalvik.vm.heapsize", "576m")
            .set("androidboot.logcat", "*:V")
            .set("androidboot.qemu.skin", format!("{w}x{h}"))
            .set("androidboot.qemu.vsync", "60")
            .set("androidboot.qemu.settings.system.screen_off_timeout", "2147483647")
            .set("androidboot.opengles.version", "196609");
        if opts.no_boot_anim {
            bc.set("androidboot.debug.sf.nobootanimation", "1");
        }
        for (k, v) in self.avd.vbmeta_params() {
            bc.set(k, v);
        }
        // Pre-authorize the host's adb key, like the stock emulator.
        if let Some(home) = std::env::var_os("HOME")
            && let Ok(key) = std::fs::read_to_string(PathBuf::from(home).join(".android/adbkey.pub"))
        {
            bc.set("androidboot.qemu.adb.pubkey", key.trim());
        }
        bc
    }

    /// What the stock host hands `qemu-props` at boot.
    fn boot_props(&self) -> Vec<(String, String)> {
        let (_, _, density) = self.avd.lcd();
        let mut props = vec![("qemu.sf.lcd_density".to_string(), density.to_string()), ("qemu.hw.mainkeys".to_string(), "0".to_string())];
        if let Ok(tz) = std::fs::read_link("/etc/localtime")
            && let Some(name) = tz.to_str().and_then(|s| s.split("zoneinfo/").nth(1))
        {
            props.push(("qemu.timezone".to_string(), name.to_string()));
        }
        props
    }

    pub fn machine_config(&self, opts: &Opts) -> Result<MachineConfig> {
        let mut initrd = std::fs::read(self.avd.ramdisk()).with_context(|| format!("reading {}", self.avd.ramdisk().display()))?;
        initrd.extend_from_slice(&self.bootconfig(opts).encode());

        let user_mode = if self.read_only { DiskMode::Ephemeral } else { DiskMode::Persistent };
        let disk = |slot, path: PathBuf, mode, serial: &str| DiskSpec { slot, path, mode, serial: serial.to_string() };
        let disks = vec![
            disk(slots::SYSTEM, self.avd.system_image(), DiskMode::Ephemeral, "system"),
            disk(slots::CACHE, self.dir.join("cache.img"), user_mode, "cache"),
            disk(slots::USERDATA, self.dir.join("userdata.img"), user_mode, "userdata"),
            disk(slots::METADATA, self.dir.join("encryptionkey.img"), user_mode, "metadata"),
            disk(slots::VENDOR, self.avd.vendor_image(), DiskMode::Ephemeral, "vendor"),
        ];
        for d in &disks {
            if !d.path.is_file() {
                bail!("missing disk image {}", d.path.display());
            }
        }

        let mut cmdline = String::from(BASE_CMDLINE);
        let extra = self.avd.image_cmdline();
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(&extra);
        }

        let log = File::create(self.dir.join("console.log"))?;
        let console: Box<dyn Write + Send> = if opts.show_kernel { Box::new(Tee(std::io::stdout(), log)) } else { Box::new(log) };

        Ok(MachineConfig {
            cpus: opts.cores.unwrap_or_else(|| self.avd.cpus()),
            ram_bytes: opts.memory_mb.map_or_else(|| self.avd.ram_bytes(), |mb| mb << 20),
            kernel: self.avd.kernel(),
            initrd: Some(initrd),
            cmdline,
            disks,
            rng: true,
            adb_port: Some(self.console_port + 1),
            boot_props: self.boot_props(),
            console,
        })
    }
}

/// First even console port whose console and adb ports are both free, like the stock emulator.
fn free_port_pair() -> Option<u16> {
    let free = |p: u16| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, p)).is_ok();
    (5554..=5682).step_by(2).find(|&p| free(p) && free(p + 1))
}

fn format_ext4(avd: &Avd, image: &std::path::Path) -> Result<()> {
    let mkfs = avd.mkfs_ext4().context("no mkfs.ext4 found (install the SDK emulator package or e2fsprogs) to create userdata")?;
    // Lazy init keeps the sparse file sparse: the guest kernel zeroes inode tables in the background.
    let out = std::process::Command::new(&mkfs)
        .args(["-F", "-q", "-t", "ext4", "-b", "4096", "-L", "userdata", "-E", "lazy_itable_init=1,lazy_journal_init=1"])
        .arg(image)
        .output()
        .with_context(|| format!("running {}", mkfs.display()))?;
    if !out.status.success() {
        bail!("{} failed: {}", mkfs.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

struct Tee<A, B>(A, B);

impl<A: Write, B: Write> Write for Tee<A, B> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(buf)?;
        self.1.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()?;
        self.1.flush()
    }
}
