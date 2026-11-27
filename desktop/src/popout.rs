//! Pop-out windows: any running device leaves the main window for a native window of its own,
//! sized to its screen, and comes back (#286).
//!
//! Pop out from the device list's context menu, the stage toolbar's pop-out button or ⌘⇧O; the
//! main window then shows the next running device. Pop back in from the pop-out's toolbar, ⌘⇧O
//! in it, or by closing it: closing returns the device to the main window, it never shuts it
//! down. Several devices can be out at once, each in its own window.
//!
//! The device's connection itself moves, not a copy of it: an iOS device's capture thread and HID
//! session (the [`NativeSurface`] it presents into keeps its latest frame, and its waker becomes
//! the pop-out's), an Android device's display stream and console worker (its [`rune::Pixels`]
//! wake whichever window draws them). Nothing reconnects, so the first frame the pop-out draws is
//! the one the main window last showed. Each window owns the connection it shows, so its touches
//! and keys can only reach that device, and it keeps drawing while the main window is hidden.
//!
//! Rune opens the windows at run time ([`rune::open_window`]), each with its own state, view tree,
//! focus and clocks, on the event loop and GPU device the main window has. The windows take turns
//! on the UI thread, so the [`Hub`] they share is an `Rc`: it lists the open pop-outs (for the
//! device list's marks and their frames) and carries a device handed back to the main window.
//!
//! Pop-outs are flat: the 3D device view (#304) stays in the main window. Each window's frame is
//! kept in the settings by device ([`crate::settings::Settings::popouts`]), so popping the same
//! device out again, in this run or the next, opens it where it was.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use rune::tok::{self, Theme, radius, type_};
use rune::{
    AppSpec, Cx, Drag, El, Icon, Key, KeyEvent, NativeSurface, Phase, Pixels, PointerPress, PopupItem, PopupMenu, SurfaceFit, Tx, Waker,
    col, el, icon, row, text,
};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::form_factor::Shape;
use sim_core::hid::{Button, Direction};
use sim_core::rotation::Rotation;

use crate::device::Live;
use crate::keys::KeyAction;
use crate::settings::WindowFrame;
use crate::{Action, Input, State, android, icons, work};

/// The traffic lights' clearance at the top of the window.
const TITLE_BAR: f32 = 22.0;

/// The narrowest pop-out: its toolbar still fits.
pub const MIN_W: f32 = 420.0;

/// The widest a fitted pop-out gets, in points: a 13-inch laptop's display holds it whole.
const MAX_W: f32 = 1440.0;

/// Points around the screen: the gutters, and the toolbar and status line above and below it.
const CHROME_W: f32 = 32.0;
const CHROME_H: f32 = 16.0 + TITLE_BAR + 28.0 + 12.0 + 12.0 + 16.0 + 16.0;

/// The window that fits a device screen of `size` pixels: the screen at
/// [`android::SCREEN_POINTS`] tall, as the main window shows it, shorter when that would be wider
/// than [`MAX_W`] (a landscape tablet, a TV), and never narrower than [`MIN_W`].
pub fn fit(size: (u32, u32)) -> (f32, f32) {
    let aspect = size.0.max(1) as f32 / size.1.max(1) as f32;
    let screen_w = (android::SCREEN_POINTS * aspect).min(MAX_W - CHROME_W);
    let screen_h = screen_w / aspect;
    ((screen_w + CHROME_W).max(MIN_W), screen_h + CHROME_H)
}

/// What the main window and its pop-outs share (on the UI thread, where they take turns).
#[derive(Default)]
pub struct Hub {
    /// The devices out in a window of their own, in the order they popped out.
    open: Vec<Open>,
    /// Devices handed back, for the main window's next poll.
    returned: Vec<Returned>,
    /// Wakes the main window for what's returned.
    main_waker: Option<Waker>,
    /// Pop-outs opened so far, for unique window keys.
    opened: u64,
}

struct Open {
    udid: String,
    /// The native window (none in headless runs and tests).
    window: Option<rune::WindowHandle>,
}

/// A device on its way back to the main window, with where its window was.
pub struct Returned {
    pub device: Popped,
    pub frame: Option<WindowFrame>,
}

impl Hub {
    pub fn is_open(&self, udid: &str) -> bool {
        self.open.iter().any(|o| o.udid == udid)
    }

    pub fn open_udids(&self) -> Vec<String> {
        self.open.iter().map(|o| o.udid.clone()).collect()
    }

    fn window(&self, udid: &str) -> Option<&rune::WindowHandle> {
        self.open.iter().find(|o| o.udid == udid)?.window.as_ref()
    }

    /// Where `udid`'s window is now, as the settings keep it. On the UI thread.
    fn frame(&self, udid: &str) -> Option<WindowFrame> {
        self.window(udid)?.placement().as_ref().and_then(crate::panes::frame_of)
    }

    fn give_back(&mut self, returned: Returned) {
        self.returned.push(returned);
        if let Some(wake) = &self.main_waker {
            wake();
        }
    }
}

/// The main window's side: the hub, and a device to pop out once it's connected.
#[derive(Default)]
pub struct Popouts {
    pub hub: Rc<RefCell<Hub>>,
    /// Pop this device out once the main window has connected it (a list row's menu on a device
    /// that wasn't selected).
    pending: Option<String>,
    /// Pop-outs made where no window can open (headless runs and tests), for the caller to drive.
    pub unopened: Vec<Window>,
}

impl Popouts {
    pub fn is_open(&self, udid: &str) -> bool {
        self.hub.borrow().is_open(udid)
    }
}

/// A device's connection and what its screen is shown with, as it moves between windows.
pub struct Popped {
    pub info: DeviceInfo,
    pub live: Option<Live>,
    pub screen: Option<android::Screen>,
    /// The iOS framebuffer the capture thread presents into.
    pub native: NativeSurface,
    /// A headless scene's screen.
    pub pixels: Pixels,
    /// The screen size before the first frame says it (and a headless scene's), and its turn.
    pub size: (u32, u32),
    pub turn: Rotation,
    pub shape: Shape,
}

impl Popped {
    /// The device screen in pixels, upright.
    pub fn screen_size(&self) -> (u32, u32) {
        let live = self.live.as_ref().and_then(|l| l.shared.size());
        live.or_else(|| self.screen.as_ref().and_then(android::Screen::device_size)).unwrap_or(self.size)
    }

    fn rotation(&self) -> Rotation {
        self.screen.as_ref().map_or(self.turn, android::Screen::rotation)
    }

    /// No live connection: a headless scene's device.
    fn offline(&self) -> bool {
        self.live.is_none() && self.screen.is_none()
    }

    fn stopped(&self) -> bool {
        self.screen.as_ref().is_some_and(|sc| !sc.alive())
    }
}

/// A pop-out window's state: the device it owns and what its toolbar and screen queued.
#[derive(Default)]
pub struct Window {
    pub device: Option<Popped>,
    hub: Rc<RefCell<Hub>>,
    pub actions: Vec<Action>,
    pub inputs: Vec<Input>,
    /// What a headless device would have been sent, in order.
    pub sent: Vec<Input>,
    held: Option<(Button, f64)>,
    pub status: Option<String>,
    outbox: work::Outbox,
    screenshot_dir: Option<std::path::PathBuf>,
    /// The screen size the window was last fitted to; a window opened at its saved frame starts
    /// with its size, so its first fit keeps it.
    sized_for: Option<(u32, u32)>,
    keep_size: bool,
    titled: bool,
    /// The screen element's size in points.
    screen_view: (f32, f32),
}

impl Window {
    pub fn udid(&self) -> Option<&str> {
        self.device.as_ref().map(|d| d.info.udid.as_str())
    }

    /// Hands the device back to the main window (once), with where this window is.
    fn give_back(&mut self) {
        let Some(device) = self.device.take() else { return };
        if let (Some((button, _)), Some(live)) = (self.held.take(), &device.live) {
            live.button(button, Direction::Up);
        }
        let mut hub = self.hub.borrow_mut();
        let frame = hub.frame(&device.info.udid);
        hub.give_back(Returned { device, frame });
    }
}

/// What the main window's poll does with a pop-out action; `false` leaves it to the poll. A
/// selected device that's out focuses its window instead.
pub fn perform(s: &mut State, action: &Action, tx: &mut Tx) -> bool {
    match action {
        Action::PopOut(udid) => {
            pop_out(s, udid, tx);
            true
        }
        Action::PopIn(udid) => {
            let window = s.popouts.hub.borrow().window(udid).cloned();
            match window {
                Some(w) => w.close(),
                None => return_unopened(s, udid),
            }
            true
        }
        Action::Select(udid) if s.popouts.is_open(udid) => {
            focus(s, udid);
            true
        }
        _ => false,
    }
}

/// A headless pop-out closes by handing its device straight back.
fn return_unopened(s: &mut State, udid: &str) {
    if let Some(i) = s.popouts.unopened.iter().position(|w| w.udid() == Some(udid)) {
        let mut w = s.popouts.unopened.remove(i);
        w.give_back();
    }
}

fn focus(s: &State, udid: &str) {
    if let Some(w) = s.popouts.hub.borrow().window(udid) {
        w.focus();
    }
}

/// Pops `udid` out: at once when the main window shows it, else once it's selected and connected.
fn pop_out(s: &mut State, udid: &str, tx: &mut Tx) {
    if s.popouts.is_open(udid) {
        return focus(s, udid);
    }
    if s.selected.as_deref() != Some(udid) {
        s.popouts.pending = Some(udid.to_owned());
        s.actions.push(Action::Select(udid.to_owned()));
        tx.request_poll();
        return;
    }
    if !move_out(s) {
        s.popouts.pending = Some(udid.to_owned());
    }
}

/// Moves the selected device's connection into a new pop-out window. False when there's none to
/// move yet (still connecting); a device that isn't running says so on the status line.
fn move_out(s: &mut State) -> bool {
    let Some(info) = s.selected_info().cloned() else { return true };
    if info.state != DeviceState::Booted {
        s.status = Some(format!("Boot {} to pop it out", info.name));
        return true;
    }
    if s.live.is_none() && s.screen.is_none() && !s.offline_screen {
        return false;
    }
    if let Some((button, _)) = s.held.take() {
        s.live.iter().for_each(|l| l.button(button, Direction::Up));
    }
    crate::stylus_off(s);
    let size = s.screen_size();
    let device = Popped {
        live: s.live.take(),
        screen: s.screen.take(),
        native: std::mem::take(&mut s.native),
        pixels: s.pixels.clone(),
        size,
        turn: s.screen_rotation(),
        shape: s.shape,
        info,
    };
    // The main window's next device presents into a surface of its own.
    if let Some(w) = &s.waker {
        s.native.set_waker(w.clone());
    }
    s.sized_for = None;
    let udid = device.info.udid.clone();
    let frame = s.prefs.current.popouts.get(&udid).copied().filter(|f| f.width > 0.0 && f.height > 0.0);
    let mut w = Window {
        hub: s.popouts.hub.clone(),
        screenshot_dir: s.prefs.current.screenshot_dir.clone(),
        keep_size: frame.is_some(),
        ..Window::default()
    };
    let fitted = fit(device.screen_size());
    w.device = Some(device);
    let n = {
        let mut hub = s.popouts.hub.borrow_mut();
        hub.main_waker = s.waker.clone();
        hub.open.push(Open { udid: udid.clone(), window: None });
        hub.opened += 1;
        hub.opened
    };
    if s.offline {
        s.popouts.unopened.push(w);
    } else {
        let size = frame.map_or(fitted, |f| (f.width.max(MIN_W), f.height));
        // Keyed by count too: the device's last window may still be closing this turn.
        let key = format!("popout-{udid}-{n}");
        let window = rune::open_window(spec(key, w, size, frame.map(|f| (f.x, f.y))));
        if let Some(o) = s.popouts.hub.borrow_mut().open.iter_mut().find(|o| o.udid == udid) {
            o.window = Some(window);
        }
    }
    // The main window moves on to the next running device that's still in it.
    let next = s.devices.iter().find(|d| d.state == DeviceState::Booted && d.udid != udid && !s.popouts.is_open(&d.udid));
    if let Some(next) = next.map(|d| d.udid.clone()) {
        s.actions.push(Action::Select(next));
    }
    s.status = Some(format!("{} is in its own window", s.devices.iter().find(|d| d.udid == udid).map_or(udid.as_str(), |d| &d.name)));
    true
}

/// The pop-out window's spec: its own state, view and hooks, flat (no 3D compositor), at
/// `position` when it has a saved frame.
fn spec(key: String, w: Window, size: (f32, f32), position: Option<(i32, i32)>) -> rune::WindowSpec<Window> {
    let spec = AppSpec::<Window> {
        setup: Some(setup),
        poll: Some(poll_window),
        on_key: Some(window_key),
        min_size: Some((MIN_W, 320.0)),
        ..AppSpec::new("simee", size, Window::default, view)
    };
    let app = rune::App::new(spec).on_close(on_close).on_quit(on_quit).on_visibility(on_visibility);
    rune::WindowSpec::from_app(key, app).state(w).options(rune::WindowOptions { position, ..rune::WindowOptions::default() })
}

/// Every main-window poll: brings handed-back devices home and pops out a pending one once
/// it's connected. Returns whether the window should redraw.
pub fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    let returned = std::mem::take(&mut s.popouts.hub.borrow_mut().returned);
    let mut changed = !returned.is_empty();
    for r in returned {
        move_in(s, r);
    }
    if let Some(udid) = s.popouts.pending.clone() {
        if s.selected.as_deref() != Some(udid.as_str()) {
            // Selected elsewhere since: that wins (its own selection may still be queued).
            if !s.actions.contains(&Action::Select(udid)) {
                s.popouts.pending = None;
            }
        } else if move_out(s) {
            s.popouts.pending = None;
            changed = true;
            tx.request_poll();
        }
    }
    changed
}

/// Shows a device handed back from its window in the main window again, as its selected device,
/// on the connection it had: the device the main window showed meanwhile is switched away from.
fn move_in(s: &mut State, r: Returned) {
    let Returned { device: p, frame } = r;
    let udid = p.info.udid.clone();
    s.popouts.hub.borrow_mut().open.retain(|o| o.udid != udid);
    if let Some(f) = frame {
        s.prefs.current.popouts.insert(udid.clone(), f);
    }
    if !s.offline {
        if let Some((button, _)) = s.held.take() {
            s.live.iter().for_each(|l| l.button(button, Direction::Up));
        }
        crate::stylus_off(s);
        s.pause_screen();
        s.live = None;
        if let Some(sc) = &p.screen {
            sc.set_visible(s.visible);
        }
        if let Some(w) = &s.waker {
            p.native.set_waker(w.clone());
        }
        if let Some(live) = &p.live {
            s.debug.data_path = Some(live.data_path());
            s.debug.reload(&udid);
        }
        s.console_log.clear();
    }
    s.live = p.live;
    s.screen = p.screen;
    s.native = p.native;
    s.shape = p.shape;
    s.offline_size = p.size;
    s.sized_for = None;
    s.selected = Some(udid);
    if let Some(sc) = &s.scanner {
        sc.follow(s.selected.as_deref());
        sc.ask(s.selected.as_deref());
    }
    s.status = None;
}

/// `App::on_close` and `App::on_quit` of the main window: notes where each pop-out is for the
/// settings; closing the main window ends the app, pop-outs included, as it did before them.
pub fn on_main_close(s: &mut State, tx: &mut Tx) {
    {
        let hub = s.popouts.hub.borrow();
        for udid in hub.open_udids() {
            if let Some(f) = hub.frame(&udid) {
                s.prefs.current.popouts.insert(udid, f);
            }
        }
        if !hub.open.is_empty() && !rune::quitting() {
            tx.quit();
        }
    }
    crate::panes::on_close(s, tx);
}

/// ⌘⇧O in the main window: pops the selected device out, or back in when it's out.
pub fn on_key(s: &mut State, k: &KeyEvent, tx: &mut Tx) -> bool {
    if !is_toggle(k) {
        return false;
    }
    let Some(udid) = s.selected.clone() else { return true };
    s.actions.push(if s.popouts.is_open(&udid) { Action::PopIn(udid) } else { Action::PopOut(udid) });
    tx.request_poll();
    true
}

fn is_toggle(k: &KeyEvent) -> bool {
    k.key == Key::Command('o') && k.mods.shift && !k.mods.alt
}

/// A device row's context menu: pop a running device out, or show its window and pop it back in.
pub fn row_menu(s: &mut State, udid: String, press: PointerPress, tx: &mut Tx) {
    let running = s.devices.iter().any(|d| d.udid == udid && d.state == DeviceState::Booted);
    let menu = if s.popouts.is_open(&udid) {
        PopupMenu::new().item(PopupItem::new("popout-focus", "Show Window")).item(PopupItem::new("popout-in", "Pop Back In"))
    } else {
        PopupMenu::new().item(PopupItem::new("popout-out", "Pop Out to Window").enabled(running))
    };
    tx.popup_menu(menu, press, move |s: &mut State, picked, tx| {
        let action = match picked {
            Some("popout-out") => Action::PopOut(udid.clone()),
            Some("popout-in") => Action::PopIn(udid.clone()),
            Some("popout-focus") => return focus(s, &udid),
            _ => return,
        };
        s.actions.push(action);
        tx.request_poll();
    });
}

/// Headless scene: the demo with its iPhone out in a window of its own, still selected, so the
/// stage says where it went and the list marks it. Headless runs open no native windows (Rune's
/// `open_window`), so the pop-out itself renders in the tests, not in a snapshot.
pub fn scene(s: &mut State) {
    crate::demo(s);
    move_out(s);
    s.actions.clear();
    s.status = None;
}

/// The stage's toolbar button for the selected, running device.
pub fn stage_button(udid: &str, t: Theme) -> El {
    let udid = udid.to_owned();
    crate::debug::icon_button("popout-button", &icons::REVEAL, "Pop out to a window (⌘⇧O)", false, t, move |s: &mut State| {
        s.actions.push(Action::PopOut(udid.clone()))
    })
}

/// The device list's mark on a device that's out in its own window.
pub fn row_mark(i: usize, t: Theme) -> El {
    icon(&icons::REVEAL, 13.0).key(format!("device-{i}-popped")).label("In its own window").color(t.text_secondary)
}

/// What the stage shows for a selected device that's out in its own window.
pub fn placeholder(udid: &str, t: Theme) -> El {
    let (focus_udid, in_udid) = (udid.to_owned(), udid.to_owned());
    col()
        .key("popped-placeholder")
        .w_full()
        .grow(1.0)
        .center()
        .gap(14.0)
        .child(icon(&icons::REVEAL, 48.0).color(t.text_secondary))
        .child(text("This device is in its own window").font(type_::BODY).color(t.text_secondary))
        .child(
            row()
                .gap(8.0)
                .child(crate::debug::icon_button("popout-show", &icons::REVEAL, "Show Window", true, t, move |s: &mut State| {
                    focus(s, &focus_udid)
                }))
                .child(crate::debug::icon_button("popout-return", &icons::RECENTS, "Pop Back In", true, t, move |s: &mut State| {
                    s.actions.push(Action::PopIn(in_udid.clone()))
                })),
        )
}

// The pop-out window.

fn setup(w: &mut Window, waker: Waker) {
    if let Some(d) = &w.device {
        d.native.set_waker(waker.clone());
    }
    w.outbox = work::Outbox::new(Some(waker));
}

/// Closing the window hands the device back to the main window; the app quitting doesn't.
fn on_close(w: &mut Window, _: &mut Tx) {
    if !rune::quitting() {
        w.give_back();
    }
}

fn on_quit(_: &mut Window, _: &mut Tx) {}

/// A hidden or minimised pop-out's Android screen reads no frames, as the main window's doesn't.
fn on_visibility(w: &mut Window, visible: bool, _: &mut Tx) {
    if let Some(sc) = w.device.as_ref().and_then(|d| d.screen.as_ref()) {
        sc.set_visible(visible);
    }
}

/// ⌘⇧O pops the device back in; ⌘S saves a screenshot wherever the focus is.
fn window_key(w: &mut Window, k: &KeyEvent, tx: &mut Tx) -> bool {
    let action = if is_toggle(k) {
        Action::PopIn(w.udid().unwrap_or_default().to_owned())
    } else if crate::keys::map(k) == KeyAction::Screenshot {
        Action::Screenshot
    } else {
        return false;
    };
    w.actions.push(action);
    tx.request_poll();
    true
}

/// The toolbar controls a pop-out performs: the device's own, without the stylus (a main-window
/// mode) or the 3D view's.
pub fn controls(d: &DeviceInfo) -> Vec<crate::controls::Control> {
    crate::controls::for_device(d)
        .into_iter()
        .filter(|c| !matches!(c.action, Action::ToggleStylus | Action::ToggleView3d | Action::View3d(_)))
        .collect()
}

fn button(key: &str, glyph: &'static Icon, label: &str, action: Action, t: Theme) -> El {
    row()
        .key(key.to_owned())
        .role("button")
        .label(label.to_owned())
        .h(28.0)
        .px(7.0)
        .center_y()
        .radius(radius::SM)
        .fill(t.control_off)
        .on_press(move |w: &mut Window| w.actions.push(action.clone()))
        .child(icon(glyph, 15.0).color(t.text))
}

pub fn view(w: &Window, _: &mut Cx<Window>) -> El {
    let t = tok::theme(true);
    let root = col().key("popout-window").w_full().h_full().fill(t.bg).padding(16.0).pt(16.0 + TITLE_BAR).gap(12.0);
    let Some(d) = &w.device else {
        return root.center().child(text("Back in the main window").key("popout-empty").font(type_::BODY).color(t.text_secondary));
    };
    let title = format!("{} · {}", d.info.name, d.info.label());
    let mut bar = row().key("popout-controls").w_full().gap(8.0).center_y();
    bar = bar.child(text(title).key("popout-title").font(type_::SECTION).color(t.text).grow(1.0).truncate());
    if !d.stopped() {
        for c in controls(&d.info) {
            bar = bar.child(button(c.key, c.icon, c.label, c.action, t));
        }
    }
    bar = bar.child(button("popin-button", &icons::RECENTS, "Pop back in (⌘⇧O)", Action::PopIn(d.info.udid.clone()), t));
    let body = if d.stopped() {
        col().key("popout-stopped").w_full().grow(1.0).center().child(text("The device stopped").font(type_::BODY).color(t.text_secondary))
    } else {
        screen(d)
    };
    let mut root = root.child(bar).child(body);
    if let Some(msg) = &w.status {
        root = root.child(text(msg.clone()).key("popout-status").font(type_::MONO_CAPTION).color(t.text_secondary).truncate());
    }
    root
}

/// Keys on the focused screen go to the device; what maps to nothing is left to the window.
fn device_key(w: &mut Window, k: &KeyEvent, _: &mut Tx) -> bool {
    let action = crate::keys::map(k);
    let handled = action != KeyAction::Ignore;
    if handled {
        w.inputs.push(Input::Key(action));
    }
    handled
}

/// The live screen, flat, filling the window at the device's aspect ratio. Pointer positions are
/// local to it, so they map to device pixels by scaling alone.
fn screen(d: &Popped) -> El {
    let (w, h) = d.screen_size();
    let ratio = w as f32 / h.max(1) as f32;
    let corners = crate::view::screen_radius(d.shape, 10_000.0);
    let device = el().key("popout-screen").aspect_fit(ratio).radius(corners);
    let turn = d.rotation();
    let device = match (&d.live, &d.screen) {
        (Some(_), _) => device.native_surface(&d.native, SurfaceFit::Fill),
        (None, Some(sc)) if turn != Rotation::Deg0 => crate::view::turned(device, &sc.pixels, turn, ratio, corners),
        (None, Some(sc)) => device.pixels_fit(&sc.pixels, SurfaceFit::Fill),
        (None, None) if turn != Rotation::Deg0 => crate::view::turned(device, &d.pixels, turn, ratio, corners),
        (None, None) => device.pixels(&d.pixels),
    };
    let device = device
        .focusable(true)
        .on_layout(|w: &mut Window, r: [f32; 4]| w.screen_view = (r[2], r[3]))
        .on_drag(|w: &mut Window, drag: &mut Drag, _: &mut Tx| {
            let Some((dw, dh)) = w.device.as_ref().map(Popped::screen_size) else { return };
            let (x, y) = (drag.x / drag.w.max(1.0) * dw as f32, drag.y / drag.h.max(1.0) * dh as f32);
            w.inputs.push(match drag.phase {
                Phase::Down => Input::Down(x, y),
                Phase::Move => Input::Move(x, y),
                Phase::Up => Input::Up(x, y),
            });
        })
        .on_key(device_key);
    col().w_full().grow(1.0).min_h(0.0).center().child(device)
}

/// Every pop-out poll: the toolbar's actions and the screen's input, on this window's device
/// only; status lines; the window's title and its fit to the screen.
pub fn poll_window(w: &mut Window, t: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if !w.titled
        && let Some(d) = &w.device
    {
        tx.set_title(format!("{} · {}", d.info.name, d.info.label()));
        w.titled = true;
    }
    if let Some((button, due)) = w.held
        && t >= due
    {
        w.held = None;
        if let Some(live) = w.device.as_ref().and_then(|d| d.live.as_ref()) {
            live.button(button, Direction::Up);
        }
    }
    for action in std::mem::take(&mut w.actions) {
        changed = true;
        if let Action::PopIn(_) = action {
            w.give_back();
            tx.close_window();
            return true;
        }
        perform_action(w, action, t, tx);
    }
    for input in crate::coalesce_moves(std::mem::take(&mut w.inputs)) {
        perform_input(w, input, t, tx);
    }
    for line in w.outbox.take() {
        w.status = Some(line);
        changed = true;
    }
    if let Some(size) = w.device.as_ref().map(Popped::screen_size)
        && w.sized_for != Some(size)
    {
        if w.sized_for.is_some() || !std::mem::take(&mut w.keep_size) {
            tx.resize_window(fit(size));
        }
        w.sized_for = Some(size);
        changed = true;
    }
    changed
}

/// Presses a button: on iOS now, releasing it on a later frame; on Android as one key tap.
fn press(w: &mut Window, button: Button, t: f64, tx: &mut Tx) {
    let Some(d) = &w.device else { return };
    if let Some(sc) = &d.screen {
        match button.android_key() {
            Some(key) => sc.key(key),
            None => w.status = Some(format!("the {} button doesn't exist on Android", button.as_str())),
        }
        return;
    }
    let Some(live) = &d.live else { return };
    if let Some((held, _)) = w.held.take() {
        live.button(held, Direction::Up);
    }
    live.button(button, Direction::Down);
    w.held = Some((button, t + 0.06));
    tx.wake_after(Duration::from_millis(60));
}

fn console(w: &Window, job: impl FnOnce(&sim_android::Device) -> Result<(), String> + Send + 'static) {
    if let Some(sc) = w.device.as_ref().and_then(|d| d.screen.as_ref()) {
        sc.console(move |d| job(d).map(|()| None));
    }
}

fn perform_action(w: &mut Window, action: Action, t: f64, tx: &mut Tx) {
    if w.device.as_ref().is_none_or(Popped::offline) {
        return;
    }
    match action {
        Action::Button(b) => press(w, b, t, tx),
        Action::Rotate => match w.device.as_mut().and_then(|d| d.live.as_mut()) {
            Some(live) => live.rotate(),
            None => console(w, sim_android::Device::rotate),
        },
        Action::ToggleAppearance => {
            if let Some(live) = w.device.as_mut().and_then(|d| d.live.as_mut()) {
                live.toggle_appearance();
            }
        }
        Action::Rotary(ticks) => {
            if let Some(d) = &w.device {
                d.live.iter().for_each(|l| l.crown(ticks));
                d.screen.iter().for_each(|sc| sc.wheel(ticks));
            }
        }
        Action::Fold(posture) => match w.device.as_ref().and_then(|d| d.live.as_ref()).map(|l| l.udid.clone()) {
            Some(udid) => {
                let outbox = w.outbox.clone();
                std::thread::spawn(move || {
                    let params = serde_json::json!({"device": udid, "posture": posture});
                    if let Err(e) = sim_api::call_method(&mut sim_api::Session::default(), "fold", &params) {
                        outbox.post(e.to_string());
                    }
                });
            }
            None => console(w, move |d| crate::fold_to(d, posture)),
        },
        Action::Xr(command) => console(w, move |d| d.xr(&command).map(drop)),
        Action::Screenshot => screenshot(w),
        _ => {}
    }
}

fn perform_input(w: &mut Window, input: Input, t: f64, tx: &mut Tx) {
    let Some(d) = &w.device else { return };
    if d.offline() {
        w.sent.push(input);
        return;
    }
    match input {
        Input::Key(KeyAction::Home) => press(w, Button::Home, t, tx),
        Input::Key(KeyAction::Lock) => press(w, Button::Lock, t, tx),
        Input::Key(KeyAction::Screenshot) => screenshot(w),
        Input::Key(KeyAction::Strokes(strokes)) => {
            d.live.iter().for_each(|l| l.strokes(&strokes));
            if let Some(sc) = &d.screen {
                sc.strokes(strokes);
            }
        }
        Input::Down(x, y) | Input::Move(x, y) | Input::Up(x, y) => {
            let phase = match input {
                Input::Down(..) => sim_core::gesture::Phase::Down,
                Input::Move(..) => sim_core::gesture::Phase::Move,
                _ => sim_core::gesture::Phase::Up,
            };
            d.live.iter().for_each(|l| l.touch(x, y, phase));
            d.screen.iter().for_each(|sc| sc.touch(x, y, phase));
        }
        Input::Key(KeyAction::Ignore) | Input::Hover(..) | Input::Leave | Input::Edge(..) => {}
    }
}

/// Saves the device's current frame off the UI thread, as the main window's ⌘S does; the status
/// line says where.
fn screenshot(w: &mut Window) {
    let Some(d) = &w.device else { return };
    let path = work::screenshot_path(w.screenshot_dir.as_deref());
    if let Some(sc) = &d.screen {
        let outbox = w.outbox.clone();
        sc.console(move |dev| match dev.screenshot_png_to(&path) {
            Ok(_) => {
                outbox.post(format!("saved {}", path.display()));
                Ok(None)
            }
            Err(e) => Err(format!("screenshot failed: {e}")),
        });
        return;
    }
    match d.live.as_ref().map(Live::capture) {
        Some(Ok(frame)) if frame.width > 0 && frame.height > 0 => work::save_png(frame, path, w.outbox.clone(), |_, _| {}),
        Some(Ok(_)) => w.status = Some("no frame to save".into()),
        Some(Err(e)) => w.status = Some(format!("screenshot failed: {e}")),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Mods, Runtime};

    fn cmd_shift(c: char) -> KeyEvent {
        KeyEvent { key: Key::Command(c), mods: Mods { cmd: true, shift: true, ..Mods::default() } }
    }

    /// The main window on the demo, polling as the app does.
    fn main_window(scene: fn(&mut State)) -> Runtime<State> {
        let mut s = crate::init();
        scene(&mut s);
        let mut rt = Runtime::new(s, crate::view::view, (1240.0, 870.0), Env::new(true, 2.0));
        rt.poll = Some(crate::poll);
        rt.on_key = Some(crate::on_key);
        rt.frame(0.0);
        rt
    }

    /// The demo with an iPhone and a Pixel both running.
    fn two_running(s: &mut State) {
        crate::demo(s);
        for d in &mut s.devices {
            if d.udid == "pixel_9" || d.udid == "A1" {
                d.state = DeviceState::Booted;
            }
        }
    }

    /// Queues `action` and runs frames until what it queued in turn has run.
    fn act(rt: &mut Runtime<State>, action: Action) {
        rt.state.actions.push(action);
        settle(rt);
    }

    fn settle(rt: &mut Runtime<State>) {
        for _ in 0..4 {
            rt.frame(1.0 / 60.0);
        }
    }

    /// The pop-out `udid` went to, as a runtime of its own.
    fn take_window(rt: &mut Runtime<State>, udid: &str) -> Runtime<Window> {
        let i = rt.state.popouts.unopened.iter().position(|w| w.udid() == Some(udid)).expect("a pop-out window");
        let w = rt.state.popouts.unopened.remove(i);
        let size = fit(w.device.as_ref().unwrap().screen_size());
        let mut win = Runtime::new(w, view, size, Env::new(true, 2.0));
        win.poll = Some(poll_window);
        win.on_key = Some(window_key);
        win.frame(0.0);
        win
    }

    #[test]
    fn a_pop_out_fits_the_device_screen() {
        let (w, h) = fit((1206, 2622));
        assert!((h - CHROME_H - android::SCREEN_POINTS).abs() < 0.5, "the screen at the main window's height: {h}");
        assert!(w >= MIN_W);
        // A wide head unit is shown shorter rather than wider than a laptop's display.
        let (w, h) = fit((2400, 720));
        assert_eq!(w, MAX_W);
        assert!(h < android::SCREEN_POINTS + CHROME_H);
        // A tall, narrow screen still gets its toolbar.
        assert_eq!(fit((300, 1000)).0, MIN_W);
    }

    /// Pop out: the device's screen moves to a window of its own, the main window moves on to
    /// the next running device and marks the one that's out; popping back in brings it home as
    /// the selected device and frees the window.
    #[test]
    fn popping_out_and_back_in() {
        let mut main = main_window(two_running);
        assert!(main.find("popout-button").is_some(), "the stage has the pop-out button");
        act(&mut main, Action::PopOut("A1".into()));
        assert!(main.state.popouts.is_open("A1"));
        assert_eq!(main.state.selected.as_deref(), Some("pixel_9"), "the main window shows the next running device");
        main.frame(1.0 / 60.0);
        assert!(main.find("device-0-popped").is_some(), "the list marks the device that's out");
        let mut win = take_window(&mut main, "A1");
        assert!(win.find("popout-screen").is_some() && win.find("home-button").is_some() && win.find("popin-button").is_some());
        assert!(win.find("scene-3d").is_none(), "pop-outs are flat");
        // Selecting it in the list focuses its window rather than showing it twice.
        act(&mut main, Action::Select("A1".into()));
        assert_eq!(main.state.selected.as_deref(), Some("pixel_9"));
        let (x, y) = win.find("popin-button").unwrap();
        win.pointer_down(x, y);
        win.pointer_up(x, y);
        win.frame(1.0 / 60.0);
        assert!(win.state.device.is_none(), "handed back");
        assert!(win.find("popout-empty").is_some());
        main.frame(1.0 / 60.0);
        assert!(!main.state.popouts.is_open("A1"));
        assert_eq!(main.state.selected.as_deref(), Some("A1"), "back as the main window's device");
        assert!(main.find("device-0-popped").is_none());
        assert!(main.find("device-screen").is_some());
    }

    /// Closing a pop-out returns its device; it doesn't shut it down.
    #[test]
    fn closing_a_pop_out_returns_its_device() {
        let mut main = main_window(two_running);
        act(&mut main, Action::PopOut("pixel_9".into()));
        let mut win = take_window(&mut main, "pixel_9");
        assert!(win.find("back-button").is_some(), "the Android toolbar");
        win.update(on_close);
        main.frame(1.0 / 60.0);
        assert!(!main.state.popouts.is_open("pixel_9"));
        assert_eq!(main.state.selected.as_deref(), Some("pixel_9"));
        let info = main.state.devices.iter().find(|d| d.udid == "pixel_9").unwrap();
        assert_eq!(info.state, DeviceState::Booted, "still running");
        assert!(main.state.actions.iter().all(|a| *a != Action::Shutdown));
    }

    /// Two devices out at once, each in its own window; ⌘⇧O pops the main window's out, and in
    /// a pop-out pops it back.
    #[test]
    fn several_pop_outs_at_once() {
        let mut main = main_window(two_running);
        main.key(cmd_shift('o'));
        main.frame(1.0 / 60.0);
        main.frame(1.0 / 60.0);
        assert!(main.state.popouts.is_open("A1"), "⌘⇧O popped the selected device out");
        assert_eq!(main.state.selected.as_deref(), Some("pixel_9"));
        main.key(cmd_shift('o'));
        main.frame(1.0 / 60.0);
        main.frame(1.0 / 60.0);
        assert_eq!(main.state.popouts.hub.borrow().open_udids(), ["A1", "pixel_9"]);
        assert_eq!(main.state.popouts.unopened.len(), 2);
        main.frame(1.0 / 60.0);
        assert!(main.find("popped-placeholder").is_some(), "nothing left to show: the stage says where it went");
        // Popping out again is focusing the window that's there.
        act(&mut main, Action::PopOut("A1".into()));
        assert_eq!(main.state.popouts.unopened.len(), 2);
        let mut pixel = take_window(&mut main, "pixel_9");
        pixel.key(cmd_shift('o'));
        pixel.frame(1.0 / 60.0);
        main.frame(1.0 / 60.0);
        assert_eq!(main.state.popouts.hub.borrow().open_udids(), ["A1"]);
        assert_eq!(main.state.selected.as_deref(), Some("pixel_9"));
        // From the main window: the placeholder's or the menu's Pop Back In.
        act(&mut main, Action::PopIn("A1".into()));
        assert!(main.state.popouts.hub.borrow().open_udids().is_empty());
        assert_eq!(main.state.selected.as_deref(), Some("A1"));
    }

    /// A device that isn't selected pops out once the main window has it; one that isn't running
    /// can't.
    #[test]
    fn the_list_menu_pops_out_any_running_device() {
        let mut main = main_window(two_running);
        act(&mut main, Action::PopOut("pixel_9".into()));
        assert!(main.state.popouts.is_open("pixel_9"));
        assert_eq!(main.state.selected.as_deref(), Some("A1"), "the device the main window showed stays");
        act(&mut main, Action::PopOut("B2".into()));
        assert!(!main.state.popouts.is_open("B2"));
        assert_eq!(main.state.status.as_deref(), Some("Boot iPhone 17 Pro Max to pop it out"));
        // The menu itself.
        let (x, y) = main.find("device-0").unwrap();
        main.secondary_down(x, y);
        main.secondary_up(x, y);
        main.frame(1.0 / 60.0);
        let titles: Vec<String> = main
            .popup_menu()
            .expect("a menu")
            .entries()
            .iter()
            .filter_map(|e| match e {
                rune::PopupEntry::Item(i) => Some(i.title().to_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["Pop Out to Window"]);
        main.resolve_popup(Some("popout-out")).expect("picked");
        main.frame(1.0 / 60.0);
        main.frame(1.0 / 60.0);
        assert!(main.state.popouts.is_open("A1"));
    }

    /// Each window's touches and keys go to its own device, in its own screen's pixels; the main
    /// window's go to the device it shows now.
    #[test]
    fn input_goes_to_the_device_of_the_window_it_lands_in() {
        let mut main = main_window(two_running);
        act(&mut main, Action::PopOut("A1".into()));
        // The Pixel's screen, as the Android scene has it.
        main.state.offline_size = (360, 800);
        act(&mut main, Action::PopOut("pixel_9".into()));
        let mut iphone = take_window(&mut main, "A1");
        let mut pixel = take_window(&mut main, "pixel_9");
        let ([x, y, w, h], _) = iphone.presented("popout-screen").expect("the iPhone's screen");
        iphone.pointer_down(x + w / 2.0, y + h / 2.0);
        iphone.pointer_up(x + w / 2.0, y + h / 2.0);
        iphone.key(KeyEvent::from(Key::Text("a".into())));
        iphone.frame(1.0 / 60.0);
        assert_eq!(iphone.state.sent.len(), 3, "{:?}", iphone.state.sent);
        match iphone.state.sent[0] {
            Input::Down(dx, dy) => assert!((dx - 201.0).abs() < 2.0 && (dy - 437.0).abs() < 2.0, "the iPhone's pixels: {dx},{dy}"),
            ref other => panic!("a touch first: {other:?}"),
        }
        assert!(pixel.state.sent.is_empty(), "nothing reached the other window's device");
        let ([x, y, w, h], _) = pixel.presented("popout-screen").expect("the Pixel's screen");
        pixel.pointer_down(x + w / 4.0, y + h / 4.0);
        pixel.pointer_up(x + w / 4.0, y + h / 4.0);
        pixel.frame(1.0 / 60.0);
        match pixel.state.sent[0] {
            Input::Down(dx, dy) => assert!((dx - 90.0).abs() < 2.0 && (dy - 200.0).abs() < 2.0, "the Pixel's pixels: {dx},{dy}"),
            ref other => panic!("a touch first: {other:?}"),
        }
        assert_eq!(iphone.state.sent.len(), 3, "and nothing from it reached the iPhone");
        assert!(main.state.inputs.is_empty());
    }

    /// The window's frame is kept by device as it pops back in, and the next pop-out of that
    /// device opens there, keeping its size on the first fit.
    #[test]
    fn a_pop_out_reopens_where_it_was() {
        let mut main = main_window(two_running);
        let frame = WindowFrame { x: 1500, y: 60, width: 500.0, height: 980.0 };
        main.state.popouts.hub.borrow_mut().give_back(Returned { device: popped("A1"), frame: Some(frame) });
        main.frame(1.0 / 60.0);
        assert_eq!(main.state.prefs.current.popouts.get("A1"), Some(&frame), "kept in the settings");
        act(&mut main, Action::PopOut("A1".into()));
        let w = &main.state.popouts.unopened[0];
        assert!(w.keep_size, "opened at its saved size, which its first fit keeps");
        // A device without a saved frame is fitted to its screen.
        act(&mut main, Action::PopOut("pixel_9".into()));
        assert!(!main.state.popouts.unopened[1].keep_size);
    }

    fn popped(udid: &str) -> Popped {
        let mut s = crate::init();
        two_running(&mut s);
        Popped {
            info: s.devices.iter().find(|d| d.udid == udid).unwrap().clone(),
            live: None,
            screen: None,
            native: NativeSurface::new(),
            pixels: s.pixels.clone(),
            size: (402, 874),
            turn: Rotation::Deg0,
            shape: Shape::Rounded,
        }
    }
}
