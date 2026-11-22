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
    pub inputs: Arc<std::sync::Mutex<Option<emu_vmm::Inputs>>>,
    pub sensors: Arc<emu_vmm::pipe::sensors::Sensors>,
    pub clipboard: Arc<emu_vmm::pipe::clipboard::Clipboard>,
    pub fingerprint: Arc<emu_vmm::pipe::fingerprint::Fingerprint>,
}

/// `sensor set <name> <v[:v...]>` / `sensor get <name>` (stock syntax).
fn sensor(info: &ConsoleInfo, words: &[&str]) -> String {
    match words {
        ["get", name] => match info.sensors.get(name) {
            Some(v) => format!("{name} = {}\r\nOK", v.iter().map(|x| format!("{x}")).collect::<Vec<_>>().join(":")),
            None => format!("KO: unknown sensor '{name}'"),
        },
        ["set", name, values] => {
            let parsed: Result<Vec<f32>, _> = values.split(':').map(str::parse).collect();
            match parsed {
                Ok(v) => {
                    if info.sensors.set(name, v) {
                        "OK".into()
                    } else {
                        format!("KO: bad sensor '{name}' or value count")
                    }
                }
                Err(_) => format!("KO: bad values '{values}'"),
            }
        }
        ["status"] => {
            let names: Vec<&str> =
                emu_vmm::pipe::sensors::NAMES.iter().copied().filter(|n| !n.is_empty() && info.sensors.get(n).is_some()).collect();
            format!("{}\r\nOK", names.iter().map(|n| format!("{n}: enabled.")).collect::<Vec<_>>().join("\r\n"))
        }
        _ => "KO: usage: sensor set <name> <v:v:v> | sensor get <name> | sensor status".into(),
    }
}

/// `event send <type>:<code>:<value> ...` (stock syntax, e.g. `EV_KEY:KEY_POWER:1`,
/// `EV_REL:REL_WHEEL:-1`) and `event text <string>`.
fn event(info: &ConsoleInfo, words: &[&str]) -> String {
    use emu_vmm::virtio::input::{EV_ABS, EV_KEY, EV_REL, EV_SYN, InputEvent, keycode};
    let guard = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
    let Some(inputs) = guard.as_ref() else { return "KO: no input devices".into() };
    match words.first().copied() {
        Some("send") => {
            let mut batches: Vec<(u16, InputEvent)> = Vec::new();
            for spec in &words[1..] {
                let parts: Vec<&str> = spec.split(':').collect();
                let [t, c, v] = parts.as_slice() else { return format!("KO: bad event '{spec}'") };
                let typ = match *t {
                    "EV_SYN" => EV_SYN,
                    "EV_KEY" => EV_KEY,
                    "EV_REL" => EV_REL,
                    "EV_ABS" => EV_ABS,
                    other => match other.parse() {
                        Ok(n) => n,
                        Err(_) => return format!("KO: bad event type '{other}'"),
                    },
                };
                let code = match (typ, *c) {
                    (EV_REL, "REL_WHEEL") => emu_vmm::virtio::input::REL_WHEEL,
                    (EV_KEY, name) => match keycode(name).or_else(|| name.parse().ok()) {
                        Some(k) => k,
                        None => return format!("KO: unknown key '{name}'"),
                    },
                    (_, n) => match n.parse() {
                        Ok(k) => k,
                        Err(_) => return format!("KO: bad code '{n}'"),
                    },
                };
                let Ok(value) = v.parse::<i32>() else { return format!("KO: bad value '{v}'") };
                batches.push((typ, InputEvent::new(typ, code, value)));
            }
            for (typ, ev) in batches {
                let dev = match typ {
                    EV_REL => &inputs.rotary,
                    EV_ABS => &inputs.touch,
                    _ => &inputs.keyboard,
                };
                dev.send(vec![ev, InputEvent::syn()]);
            }
            "OK".into()
        }
        Some("text") => {
            for ch in words[1..].join(" ").chars() {
                if let Some(k) = keycode(&ch.to_string()).or_else(|| (ch == ' ').then_some(57)) {
                    inputs.keyboard.tap_key(k);
                }
            }
            "OK".into()
        }
        _ => "KO: usage: event send <type>:<code>:<value> ... | event text <string>".into(),
    }
}

/// `screenshot [path]`: the latest scanout frame as PNG.
fn screenshot(info: &ConsoleInfo, path: Option<&str>) -> String {
    let Some(display) = &info.display else { return "KO: no display (started with -gpu off?)".into() };
    let f = display.snapshot();
    if f.frames == 0 {
        return "KO: no frame presented yet".into();
    }
    // The renderer's own capture of the last posted frame; the post-callback copy is the fallback.
    let (width, height) = (f.width, f.height);
    let rgba = emu_vmm::renderer_screenshot(width, height)
        .unwrap_or_else(|| crate::png::to_rgba(f.width, f.height, f.stride, f.format, &f.pixels));
    let path = path.map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("emu-rs-{}.png", std::process::id())));
    match crate::png::encode(width, height, &rgba).and_then(|png| std::fs::write(&path, png)) {
        Ok(()) => format!("{} ({}x{}, frame {})\r\nOK", path.display(), width, height, f.frames),
        Err(e) => format!("KO: {e}"),
    }
}

/// Commands this console implements (stock names first, then emu-rs extensions).
const HELP: &[(&str, &str)] = &[
    ("help|h|?", "print a list of commands"),
    ("kill", "kill the emulator instance"),
    ("quit|exit", "quit control session"),
    ("event", "simulate hardware events"),
    ("avd", "control virtual device execution"),
    ("finger", "manage emulator fingerprint"),
    ("ping", "check if the emulator is alive"),
    ("sensor", "manage emulator sensors"),
    ("screenshot", "take a screenshot of the display"),
    ("touch", "touch the screen at x y (emu-rs)"),
    ("clipboard", "get or set the guest clipboard (emu-rs)"),
    ("frames", "number of frames presented (emu-rs)"),
];

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
            // Stock closes without a reply; `adb emu <cmd>` sends `quit` after the command and
            // prints everything it reads, so an extra OK leaks into answers like `avd name`.
            ("quit" | "exit", _) => return Ok(()),
            (_, false) => "KO: unknown command, try 'help'".to_string(),
            // Lists what emu-rs implements, in stock's format (tools probe it, e.g. for "finger").
            ("help" | "h" | "?", true) => {
                let mut s = String::from("Android console commands:\r\n\r\n");
                for (name, what) in HELP {
                    s.push_str(&format!("    {name:<16} {what}\r\n"));
                }
                s.push_str("\r\ntry 'help <command>' for command-specific help\r\nOK");
                s
            }
            ("ping", true) => "I am alive!\r\nOK".to_string(),
            ("screenshot", true) => screenshot(info, words.get(1).copied()),
            ("event", true) => event(info, &words[1..]),
            ("sensor", true) => sensor(info, &words[1..]),
            ("finger", true) => match (words.get(1).copied(), words.get(2).and_then(|v| v.parse::<u32>().ok())) {
                (Some("touch"), Some(id)) if id > 0 => {
                    info.fingerprint.touch(id);
                    "OK".into()
                }
                (Some("remove"), _) => {
                    info.fingerprint.remove();
                    "OK".into()
                }
                _ => "KO: usage: finger touch <fingerid> | finger remove".into(),
            },
            // emu-rs extension: `clipboard get` / `clipboard set <text>`.
            ("clipboard", true) => match words.get(1).copied() {
                Some("get") => format!("{}\r\nOK", String::from_utf8_lossy(&info.clipboard.get())),
                Some("set") => {
                    info.clipboard.set(words[2..].join(" ").as_bytes());
                    "OK".into()
                }
                _ => "KO: usage: clipboard get|set <text>".into(),
            },
            ("touch", true) => {
                // emu-rs extension: `touch <x> <y> [down|up|tap]` in device pixels.
                let g = info.inputs.lock().unwrap_or_else(|e| e.into_inner());
                match (g.as_ref(), words.get(1).and_then(|v| v.parse().ok()), words.get(2).and_then(|v| v.parse().ok())) {
                    (Some(i), Some(x), Some(y)) => {
                        match words.get(3).copied().unwrap_or("tap") {
                            "down" => i.touch.touch(x, y, true),
                            "up" => i.touch.touch(x, y, false),
                            _ => {
                                i.touch.touch(x, y, true);
                                i.touch.touch(x, y, false);
                            }
                        }
                        "OK".to_string()
                    }
                    _ => "KO: usage: touch <x> <y> [down|up|tap]".to_string(),
                }
            }
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
