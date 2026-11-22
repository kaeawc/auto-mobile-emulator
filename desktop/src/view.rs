use crate::backend::AvdRow;
use crate::{Action, State};
use rune::tok::{self, Theme, radius, type_};
use rune::{Color, Cx, El, ScrollSpec, col, el, row, text};

const GREEN: Color = Color { r: 0.20, g: 0.78, b: 0.45, a: 1.0 };
const GREY: Color = Color { r: 0.55, g: 0.57, b: 0.60, a: 1.0 };

pub fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    col()
        .w_full()
        .h_full()
        .padding(20.0)
        .gap(14.0)
        .fill(t.bg)
        .child(header(s, t))
        .child(row().w_full().grow(1.0).min_h(0.0).gap(14.0).child(avd_list(s, t)).child(detail(s, t)))
}

fn button(key: &str, label: &str, action: Action, primary: bool, t: Theme) -> El {
    let fill = if primary { tok::accent(0, true) } else { t.control_off };
    row()
        .key(key)
        .role("button")
        .label(label.to_string())
        .focusable(true)
        .h(28.0)
        .px(12.0)
        .center_y()
        .radius(radius::SM)
        .fill(fill)
        .on_press(move |s: &mut State| s.actions.push(action.clone()))
        .child(text(label.to_string()).font(type_::LABEL).color(t.text))
}

fn dot(c: Color) -> El {
    el().size(8.0, 8.0).radius(4.0).fill(c)
}

fn header(s: &State, t: Theme) -> El {
    let status = s.status.clone().unwrap_or_default();
    row()
        .w_full()
        .center_y()
        .gap(12.0)
        .child(text("emu-rs").font(type_::TITLE).color(t.text))
        .child(text(status).font(type_::MONO_CAPTION).color(t.text_secondary).grow(1.0).truncate())
        .child(button("refresh-button", "Refresh", Action::Refresh, false, t))
}

fn avd_item(r: &AvdRow, idx: usize, selected: bool, t: Theme) -> El {
    let api = r.api.map_or_else(|| "API ?".to_string(), |a| format!("API {a}"));
    let mut item = row()
        .key(format!("avd-{idx}"))
        .role("button")
        .label(r.name.clone())
        .w_full()
        .h(44.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .radius(radius::SM)
        .on_press(move |s: &mut State| s.actions.push(Action::Select(idx)))
        .child(dot(if r.running.is_some() { GREEN } else { GREY }))
        .child(
            col()
                .grow(1.0)
                .child(text(r.name.clone()).font(type_::BODY).color(t.text).truncate())
                .child(text(api).font(type_::MONO_CAPTION).color(t.text_secondary)),
        );
    if selected {
        item = item.fill(t.card).border(1.0, t.card_border);
    }
    item
}

fn avd_list(s: &State, t: Theme) -> El {
    let mut list = col().key("avd-list").gap(2.0);
    if s.avds.is_empty() {
        list = list.child(text("No AVDs found").key("avds-empty").font(type_::BODY).color(t.text_secondary).padding(12.0));
    }
    for (i, r) in s.avds.iter().enumerate() {
        list = list.child(avd_item(r, i, s.selected == Some(i), t));
    }
    col()
        .w(280.0)
        .h_full()
        .min_h(0.0)
        .gap(6.0)
        .child(text("DEVICES").font(type_::SECTION).color(t.text_secondary))
        .child(list.scroll(ScrollSpec { stick_bottom: false }).shrink().min_h(0.0))
}

fn detail(s: &State, t: Theme) -> El {
    let Some(r) = s.current() else {
        return col().grow(1.0).child(text("Select a device").font(type_::BODY).color(t.text_secondary));
    };
    let facts = format!("{} vCPU · {} MB · {}×{}", r.cpus, r.ram_mb, r.lcd.0, r.lcd.1);
    let (state_line, control) = match r.running {
        Some(port) => (format!("running · emulator-{port}"), button("stop-button", "Stop", Action::Stop(port), false, t)),
        None => ("stopped".to_string(), button("start-button", "Start", Action::Start(r.name.clone()), true, t)),
    };
    let mut log = col().key("log").gap(1.0);
    for (i, line) in s.log.iter().enumerate() {
        log = log.child(text(line.clone()).key(format!("log-{i}")).font(type_::MONO_CAPTION).color(t.text).truncate());
    }
    if s.log.is_empty() {
        log = log.child(text("No console output yet").key("log-empty").font(type_::BODY).color(t.text_secondary));
    }
    col()
        .key("detail")
        .grow(1.0)
        .h_full()
        .min_h(0.0)
        .padding(14.0)
        .gap(10.0)
        .radius(radius::LG)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(
            row()
                .w_full()
                .center_y()
                .gap(10.0)
                .child(text(r.name.clone()).font(type_::SECTION).color(t.text).grow(1.0).truncate())
                .child(control),
        )
        .child(text(format!("{state_line} · {facts}")).font(type_::MONO_CAPTION).color(t.text_secondary))
        .child(text("CONSOLE").font(type_::SECTION).color(t.text_secondary))
        .child(log.scroll(ScrollSpec { stick_bottom: true }).shrink().min_h(0.0))
}
