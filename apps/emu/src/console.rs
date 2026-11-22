//! The emulator console: a line-oriented TCP control channel on the console port, as used by
//! `adb emu <cmd>` and test harnesses. Same framing and replies as the stock console
//! (`OK` / `KO: <reason>`, CRLF line endings, token auth from `~/.emulator_console_auth_token`).
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

pub struct ConsoleInfo {
    pub avd_name: String,
    pub stop: Arc<AtomicBool>,
    pub display: Option<Arc<emu_vmm::virtio::gpu::FrameStore>>,
}

/// `screenshot [path]`: the latest scanout frame as PNG.
fn screenshot(info: &ConsoleInfo, path: Option<&str>) -> String {
    let Some(display) = &info.display else { return "KO: no display (started with -gpu off?)".into() };
    let f = display.snapshot();
    if f.frames == 0 {
        return "KO: no frame presented yet".into();
    }
    let rgba = crate::png::to_rgba(f.width, f.height, f.stride, f.format, &f.pixels);
    let path = path.map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("emu-rs-{}.png", std::process::id())));
    match crate::png::encode(f.width, f.height, &rgba).and_then(|png| std::fs::write(&path, png)) {
        Ok(()) => format!("{} ({}x{}, frame {})\r\nOK", path.display(), f.width, f.height, f.frames),
        Err(e) => format!("KO: {e}"),
    }
}

fn token_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".emulator_console_auth_token"))
}

/// The auth token, creating one like the stock emulator does when the file is missing.
/// An empty file disables authentication.
fn auth_token() -> Option<String> {
    let path = token_path()?;
    match std::fs::read_to_string(&path) {
        Ok(t) => Some(t.trim().to_string()).filter(|t| !t.is_empty()),
        Err(_) => {
            let mut raw = [0u8; 12];
            use std::io::Read;
            std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut raw).ok()?;
            let token: String = raw.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string();
            std::fs::write(&path, &token).ok()?;
            Some(token)
        }
    }
}

pub fn serve(port: u16, info: ConsoleInfo) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| format!("binding console port {port}"))?;
    let info = Arc::new(info);
    std::thread::Builder::new().name("console".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let info = info.clone();
            std::thread::spawn(move || {
                let _ = session(stream, &info);
            });
        }
    })?;
    Ok(())
}

fn session(stream: TcpStream, info: &ConsoleInfo) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let token = auth_token();
    let mut authed = token.is_none();
    if authed {
        out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n")?;
    } else {
        let path = token_path().map(|p| p.display().to_string()).unwrap_or_default();
        out.write_all(
            format!(
                "Android Console: Authentication required\r\nAndroid Console: type 'auth <auth_token>' to authenticate\r\n\
                 Android Console: you can find your <auth_token> in \r\n'{path}'\r\nOK\r\n"
            )
            .as_bytes(),
        )?;
    }
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = words.first() else { continue };
        let reply = match (cmd, authed) {
            ("auth", _) => {
                if words.get(1).copied() == token.as_deref() || token.is_none() {
                    authed = true;
                    "Android Console: type 'help' for a list of commands\r\nOK".to_string()
                } else {
                    "KO: authentication token does not match ~/.emulator_console_auth_token".to_string()
                }
            }
            ("quit" | "exit", _) => {
                out.write_all(b"OK\r\n")?;
                return Ok(());
            }
            (_, false) => "KO: unknown command, try 'help'".to_string(),
            ("help" | "h" | "?", true) => "Android console commands:\r\n\r\n    help|h|?         print a list of commands\r\n    \
                 avd              control virtual device execution\r\n    kill             kill the emulator instance\r\n    \
                 ping             check if the emulator is alive\r\n    quit|exit        quit control session\r\n\r\nOK"
                .to_string(),
            ("ping", true) => "I am alive!\r\nOK".to_string(),
            ("screenshot", true) => screenshot(info, words.get(1).copied()),
            ("frames", true) => match &info.display {
                Some(d) => format!("{}\r\nOK", d.frames()),
                None => "KO: no display".to_string(),
            },
            ("kill", true) => {
                out.write_all(b"OK: killing emulator, bye bye\r\n")?;
                info.stop.store(true, Ordering::SeqCst);
                return Ok(());
            }
            ("avd", true) => match words.get(1).copied() {
                Some("name") => format!("{}\r\nOK", info.avd_name),
                Some("status") => "virtual device is running\r\nOK".to_string(),
                Some("id") => format!("{}\r\nOK", info.avd_name),
                _ => "KO: unsupported 'avd' subcommand in emu-rs".to_string(),
            },
            (other, true) => format!("KO: '{other}' is not implemented in emu-rs yet"),
        };
        out.write_all(reply.as_bytes())?;
        out.write_all(b"\r\n")?;
    }
    Ok(())
}

/// `<discovery dir>/avd/running/pid_<pid>.ini`, which tools read to find running instances.
pub struct Advertisement {
    path: PathBuf,
}

impl Advertisement {
    pub fn write(avd_name: &str, avd_dir: &std::path::Path, console_port: u16) -> Option<Advertisement> {
        let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/TemporaryItems"))?;
        let dir = base.join("avd/running");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("pid_{}.ini", std::process::id()));
        let cmdline: Vec<String> = std::env::args().collect();
        let body = format!(
            "port.serial={console_port}\nport.adb={}\navd.name={avd_name}\navd.id={avd_name}\navd.dir={}\ncmdline=\"{}\"\nemulator=emu-rs\n",
            console_port + 1,
            avd_dir.display(),
            cmdline.join("\" \"")
        );
        std::fs::write(&path, body).ok()?;
        Some(Advertisement { path })
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
