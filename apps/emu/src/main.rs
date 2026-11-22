//! `emu`: boots an Android Virtual Device with the emu-rs VMM.
//!
//! Accepts the SDK `emulator` command line (single-dash flags, `-avd NAME` or `@NAME`) so
//! existing tooling can launch it unchanged; flags that don't apply yet are accepted and
//! reported once with `-verbose`.

mod args;
mod console;
mod instance;
mod png;
mod provision;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

#[allow(unsafe_code)]
fn install_signal_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("emu: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `-create-avd`: writes the AVD with the requested hardware, warning if its image is missing.
fn create_avd(env: &emu_avd::Env, name: &str, sysdir: &str, opts: &args::Opts) -> Result<()> {
    let mut spec = emu_avd::NewAvd::new(name, sysdir);
    if let Some(lcd) = opts.lcd {
        spec.lcd = lcd;
    }
    if let Some(c) = opts.cores {
        spec.cores = c;
    }
    if let Some(m) = opts.memory_mb {
        spec.ram_mb = m;
    }
    let dir = env.create_avd(&spec)?;
    // Fail now, not at first boot, if the image isn't installed.
    if let Err(e) = env.load(name) {
        eprintln!("emu: warning: {e} (install it with emu -provision <api>)");
    }
    println!("created {name} at {}", dir.display());
    Ok(())
}

fn real_main() -> Result<ExitCode> {
    let opts = args::parse(std::env::args().skip(1))?;
    let env = emu_avd::Env::from_env();

    if opts.help {
        print!("{}", args::USAGE);
        return Ok(ExitCode::SUCCESS);
    }
    if opts.version {
        println!("emu-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    if opts.list_avds {
        for name in env.list_avds() {
            println!("{name}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if opts.list_images {
        provision::list_images(&env, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(api) = &opts.provision {
        provision::provision(&env, api, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &opts.create_avd {
        let sysdir = opts.sysdir.clone().context("-create-avd needs -sysdir system-images/<target>/<tag>/<abi>")?;
        create_avd(&env, name, &sysdir, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if opts.accel_check {
        println!("accel:\n0\nHypervisor.framework (arm64) is usable\naccel");
        return Ok(ExitCode::SUCCESS);
    }
    let name = opts.avd.clone().context("no AVD given (use -avd NAME, or -list-avds)")?;
    if opts.verbose && !opts.ignored.is_empty() {
        eprintln!("emu: accepted but not implemented yet: {}", opts.ignored.join(" "));
    }

    install_signal_handlers();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            loop {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    stop.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
    }

    let avd = env.load(&name)?;
    let inst = instance::Instance::prepare(&avd, &opts)?;
    console::serve(
        inst.console_port,
        console::ConsoleInfo { avd_name: avd.name.clone(), stop: stop.clone(), display: inst.display.clone() },
    )?;
    let _advert = console::Advertisement::write(&avd.name, &avd.dir, inst.console_port);
    // On stdout: launchers watch for the first line and the `emulator-<port>` serial.
    println!("emu: {} on emulator-{} (adb port {})", avd.name, inst.console_port, inst.console_port + 1);
    loop {
        let cfg = inst.machine_config(&opts)?;
        let out = emu_vmm::run(cfg, stop.clone())?;
        if opts.verbose {
            eprintln!("emu: stopped after {:.1}s: {:?}", out.uptime.as_secs_f64(), out.reason);
            for (i, exits) in out.exits.iter().enumerate() {
                let line: Vec<String> = exits.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{k}={n}")).collect();
                eprintln!("emu:   vcpu{i}: {}", line.join(" "));
            }
        }
        match out.reason {
            emu_vmm::StopReason::Reset if !stop.load(Ordering::SeqCst) => continue,
            emu_vmm::StopReason::Fault(msg) => {
                eprintln!("emu: guest fault: {msg}");
                return Ok(ExitCode::FAILURE);
            }
            _ => return Ok(ExitCode::SUCCESS),
        }
    }
}
