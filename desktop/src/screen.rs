//! The device screen: frames and input over a running instance's `<avd>/emu-rs/display.sock`
//! (protocol in emu's `display_server.rs`). A reader thread feeds [`Pixels`], which wakes only
//! the window showing it; input goes back as text lines.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rune::Pixels;

pub struct Screen {
    pub pixels: Pixels,
    pub dir: PathBuf,
    link: Arc<Mutex<UnixStream>>,
    guest: Arc<(AtomicU32, AtomicU32)>,
    alive: Arc<AtomicBool>,
}

impl Screen {
    /// Connects and asks for frames at most `max_width` pixels wide.
    pub fn connect(dir: &Path, max_width: u32) -> Option<Screen> {
        let mut s = UnixStream::connect(dir.join("emu-rs/display.sock")).ok()?;
        s.write_all(format!("hello {max_width}\n").as_bytes()).ok()?;
        let reader = s.try_clone().ok()?;
        let (pixels, guest, alive) = (Pixels::new(), Arc::new((AtomicU32::new(0), AtomicU32::new(0))), Arc::new(AtomicBool::new(true)));
        {
            let (p, g, a) = (pixels.clone(), guest.clone(), alive.clone());
            std::thread::Builder::new()
                .name("emu-desktop-frames".into())
                .spawn(move || {
                    let _ = read_frames(reader, &p, &g);
                    a.store(false, Ordering::Relaxed);
                })
                .ok()?;
        }
        Some(Screen { pixels, dir: dir.to_path_buf(), link: Arc::new(Mutex::new(s)), guest, alive })
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The guest display size (input coordinates); zero until the first frame.
    pub fn guest_size(&self) -> (u32, u32) {
        (self.guest.0.load(Ordering::Relaxed), self.guest.1.load(Ordering::Relaxed))
    }

    pub fn send(&self, line: &str) {
        let mut l = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if l.write_all(line.as_bytes()).and_then(|()| l.write_all(b"\n")).is_err() {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    /// A touch at (`x`, `y`) in an element of `w` x `h` points showing the screen.
    pub fn touch(&self, x: f32, y: f32, w: f32, h: f32, phase: &str) {
        let (gw, gh) = self.guest_size();
        if gw == 0 || w <= 0.0 || h <= 0.0 {
            return;
        }
        let gx = (x / w * gw as f32).clamp(0.0, gw as f32 - 1.0);
        let gy = (y / h * gh as f32).clamp(0.0, gh as f32 - 1.0);
        self.send(&format!("touch {gx:.0} {gy:.0} {phase}"));
    }
}

fn read_frames(mut r: UnixStream, pixels: &Pixels, guest: &(AtomicU32, AtomicU32)) -> std::io::Result<()> {
    let mut hdr = [0u8; 32];
    loop {
        r.read_exact(&mut hdr)?;
        if &hdr[0..4] != b"EMUF" {
            return Err(std::io::Error::other("bad frame header"));
        }
        let u = |i: usize| u32::from_le_bytes([hdr[i], hdr[i + 1], hdr[i + 2], hdr[i + 3]]);
        let (w, h, gw, gh, len) = (u(4), u(8), u(12), u(16), u(28) as usize);
        if len != w as usize * h as usize * 4 {
            return Err(std::io::Error::other("frame size mismatch"));
        }
        let mut px = vec![0u8; len];
        r.read_exact(&mut px)?;
        guest.0.store(gw, Ordering::Relaxed);
        guest.1.store(gh, Ordering::Relaxed);
        pixels.set(w, h, px);
    }
}
