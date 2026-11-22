//! The window: a device picker on the left, the live screen and its controls on the right.

use rune::tok::{self, Theme, radius, type_};
use rune::{Color, Cx, Drag, El, Icon, KeyEvent, Phase, ScrollSpec, SurfaceFit, Tx, col, el, row, text};
use sim_core::device::DeviceState;

use crate::{Action, Input, State, icons};

const GREEN: Color = Color { r: 0.20, g: 0.78, b: 0.45, a: 1.0 };
const GREY: Color = Color { r: 0.55, g: 0.55, b: 0.58, a: 1.0 };
const AMBER: Color = Color { r: 0.95, g: 0.70, b: 0.25, a: 1.0 };

pub fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    let mut root = row().w_full().h_full().fill(t.bg).child(sidebar(s, t)).child(stage(s, t));
    if s.live.is_some() || s.offline_screen {
        root = root.child(crate::debug::panel(s, t));
    }
    root
}

/// A toolbar button: an icon (labelled for accessibility), with visible text only for the primary
/// action, which also gets the accent fill.
fn button(key: &str, glyph: &'static Icon, label: &str, action: Action, primary: bool, t: Theme) -> El {
    let b = crate::debug::icon_button(key, glyph, label, primary || key == "refresh-button", t, move |s: &mut State| {
        s.actions.push(action.clone())
    });
    if primary { b.fill(tok::accent(0, true)) } else { b }
}

fn sidebar(s: &State, t: Theme) -> El {
    let mut list = col().key("device-list").gap(2.0);
    if s.devices.is_empty() {
        list = list.child(text("No devices").key("devices-empty").font(type_::BODY).color(t.text_secondary).padding(8.0));
    }
    let mut runtime = "";
    for (i, d) in s.devices.iter().enumerate() {
        if d.runtime_id != runtime {
            runtime = &d.runtime_id;
            let short = runtime_label(runtime);
            list = list.child(text(short).font(type_::SECTION).color(t.text_secondary).px(8.0).padding(4.0));
        }
        let selected = s.selected.as_deref() == Some(d.udid.as_str());
        let color = match d.state {
            DeviceState::Booted => GREEN,
            DeviceState::Booting | DeviceState::ShuttingDown | DeviceState::Creating => AMBER,
            _ => GREY,
        };
        let udid = d.udid.clone();
        let mut item = row()
            .key(format!("device-{i}"))
            .role("button")
            .label(d.name.clone())
            .w_full()
            .h(30.0)
            .px(8.0)
            .gap(8.0)
            .center_y()
            .radius(radius::SM)
            .on_press(move |s: &mut State| s.actions.push(Action::Select(udid.clone())))
            .child(el().size(8.0, 8.0).radius(4.0).fill(color))
            .child(text(d.name.clone()).font(type_::BODY).color(t.text).grow(1.0).truncate());
        if selected {
            item = item.fill(t.card);
        }
        list = list.child(item);
    }
    col()
        .w(240.0)
        .h_full()
        .padding(12.0)
        .gap(8.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        // Clear the window's title-bar buttons, which overlap the top-left corner.
        .child(el().h(22.0))
        .child(row().w_full().center_y().child(text("sim-rs").font(type_::TITLE).color(t.text).grow(1.0)).child(button(
            "refresh-button",
            &icons::REFRESH,
            "Refresh",
            Action::Refresh,
            false,
            t,
        )))
        .child(list.scroll(ScrollSpec { stick_bottom: false }).shrink().min_h(0.0))
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-5` → `iOS 26.5`.
pub fn runtime_label(id: &str) -> String {
    let tail = id.rsplit('.').next().unwrap_or(id);
    match tail.split_once('-') {
        Some((platform, version)) => format!("{platform} {}", version.replace('-', ".")),
        None => tail.to_owned(),
    }
}

fn stage(s: &State, t: Theme) -> El {
    let mut controls = row().key("controls").w_full().gap(8.0).center_y();
    let title = s.selected_info().map_or_else(|| "Select a device".to_string(), |d| format!("{} · {}", d.name, d.state.as_str()));
    controls = controls.child(text(title).key("device-title").font(type_::SECTION).color(t.text).grow(1.0).truncate());
    let booted = s.selected_info().is_some_and(|d| d.state == DeviceState::Booted);
    if s.booting {
        controls = controls.child(text("Booting…").key("booting").font(type_::BODY).color(t.text_secondary));
    } else if s.selected.is_some() && !booted {
        controls = controls.child(button("boot-button", &icons::PLAY, "Boot", Action::Boot, true, t));
    }
    if s.live.is_some() || s.offline_screen {
        controls = controls
            .child(button("home-button", &icons::HOME, "Home", Action::Home, false, t))
            .child(button("lock-button", &icons::LOCK, "Lock", Action::Lock, false, t))
            .child(button("appearance-button", &icons::MOON, "Appearance", Action::ToggleAppearance, false, t))
            .child(button("screenshot-button", &icons::CAMERA, "Screenshot", Action::Screenshot, false, t))
            .child(button("shutdown-button", &icons::POWER, "Shut Down", Action::Shutdown, false, t));
    }
    let body = if s.live.is_some() || s.offline_screen { screen(s) } else { placeholder(s, t) };
    let mut stage = col().w_full().h_full().grow(1.0).min_w(0.0).padding(16.0).gap(12.0).child(controls).child(body);
    let footer = match (&s.status, &s.socket) {
        (Some(msg), _) => Some(msg.clone()),
        (None, Some(sock)) => Some(format!("API: {sock}")),
        (None, None) => None,
    };
    if let Some(msg) = footer {
        stage = stage.child(text(msg).key("status").font(type_::MONO_CAPTION).color(t.text_secondary).truncate());
    }
    stage
}

fn placeholder(s: &State, t: Theme) -> El {
    let msg = if s.booting { "Waiting for the device to finish booting…" } else { "Boot a device to see its screen" };
    col().key("placeholder").w_full().grow(1.0).center().child(text(msg).font(type_::BODY).color(t.text_secondary))
}

/// The live screen. Pointer positions are local to the element, which has exactly the device's
/// aspect ratio, so they map to device pixels by scaling alone.
fn screen(s: &State) -> El {
    let (w, h) = s.screen_size();
    let ratio = w as f32 / h.max(1) as f32;
    let device = el().key("device-screen").aspect_fit(ratio).radius(36.0);
    let device = if s.live.is_some() { device.native_surface(&s.native, SurfaceFit::Fill) } else { device.pixels(&s.pixels) };
    let device = device
        .focusable(true)
        .on_drag(|s: &mut State, d: &mut Drag, _: &mut Tx| {
            let (w, h) = s.screen_size();
            let to_device = |x: f32, y: f32| (x / d.w.max(1.0) * w as f32, y / d.h.max(1.0) * h as f32);
            let (x, y) = to_device(d.x, d.y);
            s.inputs.push(match d.phase {
                Phase::Down => Input::Down(x, y),
                Phase::Move => Input::Move(x, y),
                Phase::Up if d.cancelled() => Input::Up(x, y),
                Phase::Up => Input::Up(x, y),
            });
        })
        .on_key(|s: &mut State, k: &KeyEvent, _: &mut Tx| {
            let action = crate::keys::map(k);
            let handled = action != crate::keys::KeyAction::Ignore;
            if handled {
                s.inputs.push(Input::Key(action));
            }
            handled
        });
    col().w_full().grow(1.0).min_h(0.0).center().child(device)
}
