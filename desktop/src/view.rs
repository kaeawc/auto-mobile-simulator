//! The window: a device picker on the left (grouped by platform), the live screen and the controls
//! its form factor has in the middle, and a platform panel on the right (iOS: the debug panel;
//! Android: the guest log, logcat and the console's critical lines).

use rune::tok::{self, Theme, radius, type_};
use rune::{Color, Cx, Drag, El, Icon, KeyEvent, Phase, PointerEvent, ScrollSpec, SurfaceFit, Tx, col, el, row, text};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::form_factor::Shape;
use sim_core::platform::Platform;

use crate::{Action, Input, State, icons};

const GREEN: Color = Color { r: 0.20, g: 0.78, b: 0.45, a: 1.0 };
const GREY: Color = Color { r: 0.55, g: 0.55, b: 0.58, a: 1.0 };
const AMBER: Color = Color { r: 0.95, g: 0.70, b: 0.25, a: 1.0 };

/// The window is always drawn in the dark theme.
const DARK: bool = true;

pub fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(DARK);
    let mut root = row().w_full().h_full().fill(t.bg).child(sidebar(s, t)).child(stage(s, t));
    if s.showing_screen() {
        root = root.child(match s.selected_info().map(|d| d.platform) {
            Some(Platform::Android) => android_panel(s, t),
            _ => crate::debug::panel(s, t),
        });
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
    let mut platform = None;
    for (i, d) in s.devices.iter().enumerate() {
        if platform != Some(d.platform) {
            platform = Some(d.platform);
            runtime = "";
            list = list.child(
                text(d.platform.label())
                    .key(format!("platform-{}", d.platform.as_str()))
                    .font(type_::TITLE)
                    .color(t.text)
                    .px(4.0)
                    .padding(4.0),
            );
        }
        if d.runtime_id != runtime {
            runtime = &d.runtime_id;
            let short = runtime_label(runtime);
            list = list.child(text(short).font(type_::SECTION).color(t.text_secondary).px(8.0).padding(4.0));
        }
        let selected = s.selected.as_deref() == Some(d.udid.as_str());
        let highlight = selection(t, DARK);
        let color = match d.state {
            DeviceState::Booted => GREEN,
            DeviceState::Booting | DeviceState::ShuttingDown | DeviceState::Creating => AMBER,
            _ => GREY,
        };
        let udid = d.udid.clone();
        let mut badge = platform_badge(d, t).key(format!("device-{i}-platform"));
        if selected {
            // The secondary colour is too faint on the tint.
            badge = badge.color(t.text);
        }
        // The bar's slot is there on every row, so names line up whichever is selected.
        let bar =
            el().key(format!("device-{i}-bar")).w(3.0).h(16.0).radius(1.5).fill(if selected { highlight.bar } else { Color::TRANSPARENT });
        let mut item = row()
            .key(format!("device-{i}"))
            .role("button")
            .label(format!("{}, {}", d.name, d.label()))
            .focus_selected(selected)
            .w_full()
            .h(30.0)
            .center_y()
            .radius(radius::SM)
            .on_press(move |s: &mut State| s.actions.push(Action::Select(udid.clone())))
            .child(bar)
            .child(
                row()
                    .grow(1.0)
                    .min_w(0.0)
                    .h_full()
                    .pl(5.0)
                    .pr(8.0)
                    .gap(8.0)
                    .center_y()
                    .child(el().size(8.0, 8.0).radius(4.0).fill(color))
                    .child(text(d.name.clone()).font(type_::BODY).color(t.text).grow(1.0).truncate())
                    .child(badge),
            );
        if selected {
            item = item.fill(highlight.fill);
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
        .child(row().w_full().center_y().child(text("simee").font(type_::TITLE).color(t.text).grow(1.0)).child(button(
            "refresh-button",
            &icons::REFRESH,
            "Refresh",
            Action::Refresh,
            false,
            t,
        )))
        .child(list.scroll(ScrollSpec { stick_bottom: false }).shrink().min_h(0.0))
}

/// How the selected device's row is drawn.
pub struct Selection {
    /// The row: the sidebar tinted toward the accent, under the primary text colour.
    pub fill: Color,
    /// A bar at the row's leading edge in the accent, which stands out from the sidebar on its
    /// own (the tint alone would be too faint at 3:1 without washing out the text).
    pub bar: Color,
}

/// The selected row's colours for theme `t` (`dark` or light). The text on the tint and the bar
/// against the sidebar meet WCAG's 4.5:1 and 3:1 in both (a test checks).
pub fn selection(t: Theme, dark: bool) -> Selection {
    let accent = tok::accent(0, dark);
    Selection { fill: t.card.mix(accent, if dark { 0.24 } else { 0.14 }), bar: accent }
}

/// The device's sub-platform (`iPadOS`, `watchOS`, `Wear OS`), else its platform.
pub fn platform_badge(d: &DeviceInfo, t: Theme) -> El {
    let name = d.sub_platform.map_or_else(|| d.platform.label(), |s| s.label());
    text(name).font(type_::MONO_CAPTION).color(t.text_secondary)
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-5` → `iOS 26.5`;
/// `system-images;android-36;google_apis;arm64-v8a` → `Android 36 · google_apis`.
pub fn runtime_label(id: &str) -> String {
    if let Some(["system-images", platform, tag, _abi]) = id.contains(';').then(|| id.split(';').collect::<Vec<_>>()).as_deref() {
        return format!("Android {} · {tag}", platform.trim_start_matches("android-"));
    }
    let tail = id.rsplit('.').next().unwrap_or(id);
    match tail.split_once('-') {
        Some((platform, version)) => format!("{platform} {}", version.replace('-', ".")),
        None => tail.to_owned(),
    }
}

fn stage(s: &State, t: Theme) -> El {
    let mut controls = row().key("controls").w_full().gap(8.0).center_y();
    let title =
        s.selected_info().map_or_else(|| "Select a device".to_string(), |d| format!("{} · {} · {}", d.name, d.label(), d.state.as_str()));
    controls = controls.child(text(title).key("device-title").font(type_::SECTION).color(t.text).grow(1.0).truncate());
    let booted = s.selected_info().is_some_and(|d| d.state == DeviceState::Booted);
    if let Some(busy) = s.busy {
        controls = controls.child(text(busy).key("busy").font(type_::BODY).color(t.text_secondary));
    } else if s.selected.is_some() && !booted {
        controls = controls.child(button("boot-button", &icons::PLAY, "Boot", Action::Boot, true, t));
    }
    if s.showing_screen()
        && let Some(d) = s.selected_info()
    {
        for c in crate::controls::for_device(d) {
            controls = controls.child(button(c.key, c.icon, c.label, c.action, false, t));
        }
    }
    let body = if s.showing_screen() { screen(s) } else { placeholder(s, t) };
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
    let msg = if s.busy == Some("Booting…") { "Waiting for the device to finish booting…" } else { "Boot a device to see its screen" };
    col().key("placeholder").w_full().grow(1.0).center().child(text(msg).font(type_::BODY).color(t.text_secondary))
}

/// The live screen. Pointer positions are local to the element, which has exactly the device's
/// aspect ratio, so they map to device pixels by scaling alone.
fn screen(s: &State) -> El {
    let (w, h) = s.screen_size();
    let ratio = w as f32 / h.max(1) as f32;
    let device = el().key("device-screen").aspect_fit(ratio).radius(screen_radius(s.shape, 10_000.0));
    let device = match (&s.live, &s.screen) {
        (Some(_), _) => device.native_surface(&s.native, SurfaceFit::Fill),
        (None, Some(sc)) => device.pixels_fit(&sc.pixels, SurfaceFit::Fill),
        (None, None) => device.pixels(&s.pixels),
    };
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
        // With the stylus on (Android tablets), the pointer over the screen is the pen hovering.
        .on_layout(|s: &mut State, r: [f32; 4]| s.screen_view = (r[2], r[3]))
        .on_pointer_move_local(|s: &mut State, p: PointerEvent, _: &mut Tx| {
            if s.stylus && !p.pressing {
                let (w, h) = s.screen_size();
                let (vw, vh) = s.screen_view;
                s.inputs.push(Input::Hover(p.x / vw.max(1.0) * w as f32, p.y / vh.max(1.0) * h as f32));
            }
        })
        .on_hover_change(|s: &mut State, inside: bool, _: &mut Tx| {
            if s.stylus && !inside {
                s.inputs.push(Input::Leave);
            }
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

/// Corner radius, in points, for a screen of `width` points with this outline: a round watch is
/// a circle; phones and watches with corners get the device-like curve.
pub fn screen_radius(shape: Shape, width: f32) -> f32 {
    match shape {
        Shape::Round => width / 2.0,
        Shape::Rounded => 36.0_f32.min(width / 8.0),
        Shape::Rectangle => 4.0,
    }
}

/// What an Android panel says when there's no log: emu keeps the kernel console quiet, and the
/// logcat stream exists only for devices booted from here.
const EMPTY_LOG: &str = "No log yet. Devices booted here stream logcat (info and up) to this panel. \
                         The kernel console only has critical messages: for the full kernel log, \
                         start emu with -show-kernel or EMU_CONSOLE=full.";

/// An Android device's panel: what it is, and its guest log (the console's critical lines, then
/// logcat).
fn android_panel(s: &State, t: Theme) -> El {
    let mut facts = col().gap(4.0);
    if let Some(d) = s.selected_info() {
        let (w, h) = s.screen_size();
        for (key, line) in
            [("android-kind", d.label()), ("android-runtime", runtime_label(&d.runtime_id)), ("android-screen", format!("{w}×{h} px"))]
        {
            facts = facts.child(text(line).key(key).font(type_::MONO_CAPTION).color(t.text_secondary).truncate());
        }
    }
    let mut log = col().key("log").gap(1.0);
    for (i, line) in s.console_log.iter().enumerate() {
        log = log.child(text(line.clone()).key(format!("log-{i}")).font(type_::MONO_CAPTION).color(t.text).truncate());
    }
    if s.console_log.is_empty() {
        log = log.child(text(EMPTY_LOG).key("log-empty").font(type_::BODY).color(t.text_secondary).wrap());
    }
    col()
        .key("android-panel")
        .w(340.0)
        .h_full()
        .padding(12.0)
        .gap(10.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(el().h(22.0))
        .child(text("DEVICE").font(type_::SECTION).color(t.text_secondary))
        .child(facts)
        .child(text("LOG").font(type_::SECTION).color(t.text_secondary))
        .child(log.scroll(ScrollSpec { stick_bottom: true }).shrink().min_h(0.0))
}
