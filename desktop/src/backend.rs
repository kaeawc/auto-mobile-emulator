//! Everything the desktop app does outside the UI thread: AVD discovery, launching `emu`,
//! talking to a running instance's console, reading its boot log.
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvdRow {
    pub name: String,
    pub api: Option<u32>,
    pub cpus: u32,
    pub ram_mb: u64,
    pub lcd: (u32, u32),
    pub dir: PathBuf,
    /// Console port of a running emu-rs instance.
    pub running: Option<u16>,
}

/// Running instances advertised under `~/Library/Caches/TemporaryItems/avd/running`
/// (emu-rs writes `emulator=emu-rs` there): AVD name → console port.
fn running_instances() -> Vec<(String, u16)> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let dir = PathBuf::from(home).join("Library/Caches/TemporaryItems/avd/running");
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            let kv = emu_avd::parse_ini(&text);
            Some((kv.get("avd.name")?.clone(), kv.get("port.serial")?.parse().ok()?))
        })
        .collect()
}

pub fn scan() -> Vec<AvdRow> {
    let env = emu_avd::Env::from_env();
    let running = running_instances();
    env.list_avds()
        .into_iter()
        .filter_map(|name| {
            let avd = env.load(&name).ok()?;
            let (w, h, _) = avd.lcd();
            Some(AvdRow {
                running: running.iter().find(|(n, _)| *n == name).map(|(_, p)| *p),
                api: avd.api_level(),
                cpus: avd.cpus(),
                ram_mb: avd.ram_bytes() >> 20,
                lcd: (w, h),
                dir: avd.dir.clone(),
                name,
            })
        })
        .collect()
}

/// The `emu` binary: `EMU_BIN`, else next to this app's target dir, else on PATH.
pub fn emu_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("EMU_BIN") {
        return PathBuf::from(p);
    }
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    for profile in ["release", "debug"] {
        let p = here.join("../target").join(profile).join("emu");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("emu")
}

/// Starts `emu -avd <name>` detached. The instance advertises itself once it's up.
pub fn start(name: &str) -> Result<(), String> {
    let bin = emu_binary();
    let signer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/sign-run.sh");
    let mut cmd = if signer.is_file() {
        let mut c = std::process::Command::new(signer);
        c.arg(&bin);
        c
    } else {
        std::process::Command::new(&bin)
    };
    cmd.args(["-avd", name, "-no-boot-anim"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("starting {}: {e}", bin.display()))
}

/// Sends one console command (authenticating if a token is configured); returns the reply lines.
pub fn console(port: u16, command: &str) -> Result<Vec<String>, String> {
    let mut s = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    let token = std::env::var_os("HOME")
        .and_then(|h| std::fs::read_to_string(PathBuf::from(h).join(".emulator_console_auth_token")).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let mut script = String::new();
    if let Some(t) = &token {
        script.push_str(&format!("auth {t}\r\n"));
    }
    script.push_str(&format!("{command}\r\nquit\r\n"));
    s.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    let lines: Vec<String> = BufReader::new(s).lines().map_while(Result::ok).collect();
    // Drop the banner and auth acknowledgements: keep what follows the last of them.
    let skip = lines.iter().rposition(|l| l.starts_with("Android Console: type 'help'")).map_or(0, |i| i + 2);
    Ok(lines.into_iter().skip(skip).filter(|l| l != "OK").collect())
}

/// The last `n` lines of an instance's guest console log.
pub fn log_tail(avd_dir: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(avd_dir.join("emu-rs/console.log")) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| l.to_string()).collect()
}
