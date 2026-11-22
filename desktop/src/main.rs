//! emu-rs desktop: pick an AVD, start or stop it, watch its console.
//!
//! Handlers only queue [`Action`]s; `poll` performs them, so the headless tests below drive the
//! real view without launching anything. A running instance's screen streams into the detail
//! pane (`screen.rs`), with touch and the Home/Back/Power keys going back to it.
//!
//! `cargo run` opens the window; `cargo run -- --snap panel.png --scene demo` renders headless.

#![forbid(unsafe_code)]

mod backend;
mod screen;
mod view;

use backend::AvdRow;
use rune::{AppSpec, Latest, Tx, Waker};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Refresh,
    Select(usize),
    Start(String),
    Stop(u16),
}

pub struct State {
    pub avds: Vec<AvdRow>,
    pub selected: Option<usize>,
    pub log: Vec<String>,
    pub status: Option<String>,
    pub actions: Vec<Action>,
    /// No side effects (headless scenes and tests).
    pub offline: bool,
    scan: Latest<Vec<AvdRow>>,
    next_refresh: f64,
    /// The selected running instance's screen.
    pub screen: Option<screen::Screen>,
    next_connect: f64,
}

impl State {
    pub fn current(&self) -> Option<&AvdRow> {
        self.selected.and_then(|i| self.avds.get(i))
    }
}

fn init() -> State {
    State {
        avds: Vec::new(),
        selected: None,
        log: Vec::new(),
        status: None,
        actions: vec![Action::Refresh],
        offline: false,
        scan: Latest::default(),
        next_refresh: 0.0,
        screen: None,
        next_connect: 0.0,
    }
}

fn demo(s: &mut State) {
    s.offline = true;
    s.actions.clear();
    let row = |name: &str, api, running| AvdRow {
        name: name.into(),
        api: Some(api),
        cpus: 4,
        ram_mb: 2048,
        lcd: (1080, 2400),
        dir: PathBuf::from("/tmp"),
        running,
    };
    s.avds = vec![row("pixel_8_api36", 36, Some(5554)), row("pixel_6_api35", 35, None), row("wear_round_api34", 34, None)];
    s.selected = Some(0);
    s.log = vec![
        "[    0.000000] Booting Linux on physical CPU 0x0000000000".into(),
        "[    0.871529] init: init first stage started!".into(),
        "[    0.938122] init: Switching root to '/system'".into(),
        "[    8.431130] init: Encryption policy of /data/incremental set".into(),
        "[   22.104210] init: starting service 'adbd'...".into(),
    ];
}

fn setup(s: &mut State, waker: Waker) {
    s.scan.set_waker(waker.clone());
    if s.offline {
        return;
    }
    std::thread::Builder::new()
        .name("emu-desktop-tick".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                waker();
            }
        })
        .map(drop)
        .unwrap_or_else(|e| eprintln!("tick thread: {e}"));
}

fn poll(s: &mut State, t: f64, _tx: &mut Tx) -> bool {
    let mut changed = false;
    for action in std::mem::take(&mut s.actions) {
        changed = true;
        match action {
            Action::Select(i) => {
                s.selected = Some(i);
                s.log.clear();
            }
            _ if s.offline => {}
            Action::Refresh => s.next_refresh = t,
            Action::Start(name) => {
                s.status = Some(match backend::start(&name) {
                    Ok(()) => format!("starting {name}…"),
                    Err(e) => e,
                });
                s.next_refresh = t + 1.0;
            }
            Action::Stop(port) => {
                s.status = Some(match backend::console(port, "kill") {
                    Ok(lines) => lines.join(" "),
                    Err(e) => format!("console {port}: {e}"),
                });
                s.next_refresh = t + 1.0;
            }
        }
    }
    if s.offline {
        return changed;
    }
    if t >= s.next_refresh && !s.scan.is_pending() {
        s.next_refresh = t + 2.0;
        s.scan.request((), |()| backend::scan());
    }
    if let Some(avds) = s.scan.take() {
        if s.selected.is_none() && !avds.is_empty() {
            s.selected = Some(0);
        }
        if avds != s.avds {
            s.avds = avds;
            changed = true;
        }
    }
    // Keep a screen connection to the selected instance while it runs.
    let want = s.current().filter(|r| r.running.is_some()).map(|r| r.dir.clone());
    let have = s.screen.as_ref().filter(|sc| sc.alive()).map(|sc| sc.dir.clone());
    if want != have {
        if s.screen.take().is_some() {
            changed = true;
        }
        if let Some(dir) = want
            && t >= s.next_connect
        {
            s.next_connect = t + 1.0;
            s.screen = screen::Screen::connect(&dir, 720);
            changed |= s.screen.is_some();
        }
    }
    if let Some(row) = s.current() {
        let log = backend::log_tail(&row.dir, 200);
        if log != s.log {
            s.log = log;
            changed = true;
        }
    }
    changed
}

fn main() {
    rune::run(AppSpec::<State> {
        scenes: &[
            ("demo", demo),
            ("empty", |s| {
                s.offline = true;
                s.actions.clear();
            }),
        ],
        setup: Some(setup),
        poll: Some(poll),
        min_size: Some((720.0, 440.0)),
        ..AppSpec::new("emu-rs", (980.0, 620.0), init, view::view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};

    fn runtime(scene: fn(&mut State)) -> Runtime<State> {
        let mut s = init();
        scene(&mut s);
        let mut rt = Runtime::new(s, view::view, (980.0, 620.0), Env::new(true, 2.0));
        rt.poll = Some(poll);
        rt.frame(0.0);
        rt
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
    }

    #[test]
    fn demo_lists_avds_and_console() {
        let rt = runtime(demo);
        for key in ["avd-0", "avd-1", "avd-2", "detail", "log-0", "log-4", "stop-button"] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        assert!(rt.find("avd-3").is_none());
    }

    #[test]
    fn buttons_queue_actions() {
        let mut rt = runtime(demo);
        rt.poll = None;
        press(&mut rt, "stop-button");
        press(&mut rt, "avd-1");
        press(&mut rt, "refresh-button");
        assert_eq!(rt.state.actions, vec![Action::Stop(5554), Action::Select(1), Action::Refresh]);
    }

    #[test]
    fn stopped_avd_offers_start() {
        let mut rt = runtime(demo);
        press(&mut rt, "avd-1"); // offline poll applies the selection
        rt.frame(2.0 / 60.0); // rebuild the view with it
        rt.poll = None;
        press(&mut rt, "start-button");
        assert_eq!(rt.state.actions, vec![Action::Start("pixel_6_api35".into())]);
    }

    #[test]
    fn empty_scene_has_placeholders() {
        let rt = runtime(|s| {
            s.offline = true;
            s.actions.clear();
        });
        assert!(rt.find("avds-empty").is_some());
        assert!(rt.find("detail").is_none());
    }
}
