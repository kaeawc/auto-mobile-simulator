//! sim-rs desktop: a live, interactive window onto a simulated iOS device, rendered with Rune.
//!
//! Pick a device on the left; boot it if needed. The screen streams from the device framebuffer;
//! clicks and drags become touches, typing goes to the device keyboard. ⌘⇧H is Home, ⌘L Lock, ⌘S
//! saves a screenshot to the Desktop.
//!
//! Handlers only queue [`Action`]s and [`Input`]s; `poll` performs them. That keeps handlers pure,
//! so the headless tests drive the real view without a device.
//!
//! `cargo run` opens the window (`SIM_RS_DEVICE=<udid|name|booted>` preselects a device);
//! `cargo run -- --snap shot.png --scene demo` renders headless.

mod debug;
mod device;
mod icons;
mod keys;
mod surface;
mod view;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use rune::{AppSpec, NativeSurface, Pixels, Tx, Waker};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::gesture::Phase;
use sim_core::hid::Button;
use sim_coresim::{Context, Direction};

use device::Live;
use keys::KeyAction;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Refresh,
    Select(String),
    Boot,
    Shutdown,
    Home,
    Lock,
    ToggleAppearance,
    Screenshot,
    Debug(debug::DebugAction),
}

/// Device input, in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    Key(KeyAction),
}

pub struct State {
    pub devices: Vec<DeviceInfo>,
    pub selected: Option<String>,
    /// The synthetic screen of headless scenes.
    pub pixels: Pixels,
    /// The live device framebuffer, shown without copying.
    pub native: NativeSurface,
    pub live: Option<Live>,
    /// A pressed hardware button and when to release it.
    held: Option<(Button, f64)>,
    pub booting: bool,
    pub status: Option<String>,
    pub debug: debug::Debug,
    /// The unix socket this window serves the API on.
    pub socket: Option<String>,
    pub actions: Vec<Action>,
    pub inputs: Vec<Input>,
    /// No device service (headless scenes and tests).
    pub offline: bool,
    /// Headless scenes show the screen without a live device.
    pub offline_screen: bool,
    offline_size: (u32, u32),
    ctx: Option<Context>,
    waker: Option<Waker>,
    boot_done: Arc<AtomicBool>,
    boot_error: Arc<Mutex<Option<String>>>,
    sized_for: Option<(u32, u32)>,
}

impl State {
    pub fn selected_info(&self) -> Option<&DeviceInfo> {
        self.selected.as_ref().and_then(|u| self.devices.iter().find(|d| &d.udid == u))
    }

    /// The device screen in pixels; a phone-shaped default until the first frame.
    pub fn screen_size(&self) -> (u32, u32) {
        self.live.as_ref().and_then(|l| l.shared.size()).unwrap_or(self.offline_size)
    }

    fn refresh(&mut self) {
        let Some(ctx) = &self.ctx else { return };
        match device::list(ctx) {
            Ok(list) => self.devices = list,
            Err(e) => self.status = Some(e),
        }
    }

    fn connect(&mut self) {
        let (Some(ctx), Some(udid), Some(waker)) = (&self.ctx, self.selected.clone(), self.waker.clone()) else { return };
        if self.live.as_ref().is_some_and(|l| l.udid == udid) {
            return;
        }
        self.live = None;
        if self.selected_info().is_some_and(|d| d.state == DeviceState::Booted) {
            match Live::connect(ctx, &udid, self.native.clone(), waker) {
                Ok(live) => {
                    self.debug.data_path = Some(live.data_path());
                    self.live = Some(live);
                    self.status = None;
                    self.debug.reload(&udid);
                }
                Err(e) => self.status = Some(e),
            }
        }
    }
}

fn init() -> State {
    State {
        devices: Vec::new(),
        selected: std::env::var("SIM_RS_DEVICE").ok(),
        pixels: Pixels::new(),
        native: NativeSurface::new(),
        live: None,
        held: None,
        booting: false,
        status: None,
        debug: debug::Debug::default(),
        socket: None,
        actions: vec![Action::Refresh],
        inputs: Vec::new(),
        offline: false,
        offline_screen: false,
        offline_size: (1206, 2622),
        ctx: None,
        waker: None,
        boot_done: Arc::new(AtomicBool::new(false)),
        boot_error: Arc::new(Mutex::new(None)),
        sized_for: None,
    }
}

fn info(udid: &str, name: &str, runtime: &str, state: DeviceState) -> DeviceInfo {
    DeviceInfo {
        udid: udid.into(),
        name: name.into(),
        state,
        runtime_id: format!("com.apple.CoreSimulator.SimRuntime.{runtime}"),
        device_type_id: "com.apple.CoreSimulator.SimDeviceType.iPhone-17".into(),
        available: true,
    }
}

/// Fixed data and a synthetic screen for snapshots and tests.
fn demo(s: &mut State) {
    s.offline = true;
    s.offline_screen = true;
    s.actions.clear();
    s.devices = vec![
        info("A1", "iPhone 17", "iOS-26-5", DeviceState::Booted),
        info("B2", "iPhone 17 Pro Max", "iOS-26-5", DeviceState::Shutdown),
        info("C3", "iPhone 18 Pro", "iOS-27-0", DeviceState::Shutdown),
    ];
    s.selected = Some("A1".into());
    let (w, h) = (402u32, 874u32);
    s.offline_size = (w, h);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 200, 255]);
        }
    }
    s.pixels.set(w, h, rgba);
    s.debug.overlays = vec![(2, true)];
    s.debug.text_size = 3;
    s.debug.processes = vec![
        sim_debug::process::Process {
            pid: 412,
            ppid: 1,
            cpu: 7.5,
            rss_kb: 612_000,
            elapsed: "10:00".into(),
            name: "SpringBoard".into(),
            path: "/x/SpringBoard".into(),
        },
        sim_debug::process::Process {
            pid: 980,
            ppid: 1,
            cpu: 1.2,
            rss_kb: 88_000,
            elapsed: "01:00".into(),
            name: "Preferences".into(),
            path: "/x/Preferences".into(),
        },
    ];
    s.debug.connections = vec![sim_debug::net::Connection {
        pid: 980,
        process: "nsurlsessiond".into(),
        protocol: "TCP".into(),
        local: "10.0.0.2:51000".into(),
        remote: "17.253.4.125:443".into(),
        state: "ESTABLISHED".into(),
    }];
    s.debug.traffic = vec![sim_debug::net::Traffic { pid: 980, process: "nsurlsessiond".into(), bytes_in: 1_400_000, bytes_out: 52_000 }];
    s.debug.containers = vec![sim_debug::storage::App {
        bundle_id: "com.example.playground".into(),
        data: Some("Containers/Data/Application/X".into()),
        bundle: None,
        data_bytes: 3 << 20,
        bundle_bytes: 0,
    }];
    s.debug.logs = vec!["2026-11-22 01:00:00.000 Df SpringBoard[412:1] launched".into()];
}

fn empty(s: &mut State) {
    s.offline = true;
    s.actions.clear();
    s.selected = None;
}

fn headless() -> bool {
    std::env::args().any(|a| matches!(a.as_str(), "--snap" | "--tree" | "--script" | "--list-scenes" | "--video"))
}

fn setup(s: &mut State, waker: Waker) {
    s.native.set_waker(waker.clone());
    s.waker = Some(waker);
    if s.offline || headless() {
        s.offline = true;
        return;
    }
    match Context::new() {
        Ok(ctx) => s.ctx = Some(ctx),
        Err(e) => s.status = Some(e.to_string()),
    }
    if let Some(w) = s.waker.clone() {
        s.debug.start(w);
    }
    // The window also serves the API, so scripts can drive the device it shows.
    let socket =
        std::env::var_os("SIM_RS_SOCKET").map_or_else(|| std::env::temp_dir().join("sim-rs-desktop.sock"), std::path::PathBuf::from);
    match sim_api::serve_in_background(socket.clone()) {
        Ok(()) => s.socket = Some(socket.display().to_string()),
        Err(e) => s.status = Some(format!("API socket {}: {e}", socket.display())),
    }
}

/// Presses a button now and releases it on a later frame.
fn press_button(s: &mut State, button: Button, t: f64, tx: &mut Tx) {
    let Some(live) = &s.live else { return };
    if let Some((held, _)) = s.held.take() {
        live.button(held, Direction::Up);
    }
    live.button(button, Direction::Down);
    s.held = Some((button, t + 0.06));
    tx.wake_after(Duration::from_millis(60));
}

fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    if let Some((button, due)) = s.held
        && t >= due
    {
        s.held = None;
        s.live.iter().for_each(|l| l.button(button, Direction::Up));
    }
    for action in std::mem::take(&mut s.actions) {
        changed = true;
        if s.offline {
            if let Action::Select(udid) = action {
                s.selected = Some(udid);
            }
            continue;
        }
        match action {
            Action::Refresh => {
                s.refresh();
                if s.selected.as_deref() == Some("booted") {
                    s.selected = s.devices.iter().find(|d| d.state == DeviceState::Booted).map(|d| d.udid.clone());
                } else if let Some(q) = s.selected.clone()
                    && !s.devices.iter().any(|d| d.udid == q)
                {
                    s.selected = s.devices.iter().find(|d| d.name == q).map(|d| d.udid.clone());
                }
                s.connect();
            }
            Action::Select(udid) => {
                s.selected = Some(udid);
                s.connect();
            }
            Action::Boot => {
                if let (Some(udid), Some(waker)) = (s.selected.clone(), s.waker.clone()) {
                    s.booting = true;
                    s.boot_done.store(false, Ordering::Relaxed);
                    device::boot_async(udid, s.boot_done.clone(), s.boot_error.clone(), waker);
                }
            }
            Action::Shutdown => {
                if let Some(live) = s.live.take() {
                    if let Err(e) = live.shutdown() {
                        s.status = Some(e);
                    }
                    drop(live);
                }
                s.sized_for = None;
                s.refresh();
            }
            Action::Home => press_button(s, Button::Home, t, tx),
            Action::Lock => press_button(s, Button::Lock, t, tx),
            Action::ToggleAppearance => s.live.iter_mut().for_each(Live::toggle_appearance),
            Action::Screenshot => s.status = Some(screenshot(s)),
            Action::Debug(a) => {
                if let Some(udid) = s.live.as_ref().map(|l| l.udid.clone())
                    && let Some(msg) = s.debug.perform(&udid, a)
                {
                    s.status = Some(msg);
                }
            }
        }
    }
    for input in std::mem::take(&mut s.inputs) {
        if s.offline {
            continue;
        }
        if s.live.is_none() {
            continue;
        }
        match input {
            Input::Key(KeyAction::Home) => press_button(s, Button::Home, t, tx),
            Input::Key(KeyAction::Lock) => press_button(s, Button::Lock, t, tx),
            Input::Key(KeyAction::Screenshot) => {
                s.actions.push(Action::Screenshot);
                tx.wake_after(Duration::ZERO);
            }
            input => {
                let Some(live) = &s.live else { continue };
                apply(live, input);
            }
        }
    }
    let (debug_changed, debug_status) = s.debug.drain();
    if let Some(msg) = debug_status {
        s.status = Some(msg);
    }
    changed |= debug_changed;
    if s.booting && s.boot_done.load(Ordering::Relaxed) {
        s.booting = false;
        if let Some(e) = s.boot_error.lock().ok().and_then(|mut g| g.take()) {
            s.status = Some(e);
        }
        s.refresh();
        s.connect();
        changed = true;
    }
    // Fit the window to the device once its screen size is known.
    if let Some(size) = s.live.as_ref().and_then(|l| l.shared.size())
        && s.sized_for != Some(size)
    {
        s.sized_for = Some(size);
        let screen_h = 760.0_f32;
        let screen_w = screen_h * size.0 as f32 / size.1 as f32;
        tx.resize_window((240.0 + 32.0 + screen_w.max(420.0) + 340.0, screen_h + 110.0));
        changed = true;
    }
    changed
}

/// Sends one touch or keystroke input to the device.
fn apply(live: &Live, input: Input) {
    match input {
        Input::Down(x, y) => live.touch(x, y, Phase::Down),
        Input::Move(x, y) => live.touch(x, y, Phase::Move),
        Input::Up(x, y) => live.touch(x, y, Phase::Up),
        Input::Key(KeyAction::Strokes(strokes)) => live.strokes(&strokes),
        Input::Key(_) => {}
    }
}

/// Saves the current frame to the Desktop; returns a status line.
fn screenshot(s: &State) -> String {
    let (w, h, rgba) = match &s.live {
        Some(live) => match live.capture() {
            Ok(f) => (f.width, f.height, f.rgba),
            Err(e) => return format!("screenshot failed: {e}"),
        },
        None => {
            let (w, h) = s.pixels.size();
            (w, h, s.pixels.rgba())
        }
    };
    if w == 0 || h == 0 {
        return "no frame to save".into();
    }
    let dir = std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join("Desktop")).unwrap_or_else(|_| std::env::temp_dir());
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = dir.join(format!("sim-rs-{secs}.png"));
    let result = std::fs::File::create(&path).map_err(|e| e.to_string()).and_then(|f| {
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().and_then(|mut wr| wr.write_image_data(&rgba)).map_err(|e| e.to_string())
    });
    match result {
        Ok(()) => format!("saved {}", path.display()),
        Err(e) => format!("screenshot failed: {e}"),
    }
}

fn main() {
    rune::run(AppSpec::<State> {
        scenes: &[("demo", demo), ("empty", empty)],
        setup: Some(setup),
        poll: Some(poll),
        min_size: Some((1000.0, 560.0)),
        ..AppSpec::new("sim-rs", (1240.0, 870.0), init, view::view)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Key, KeyEvent, Runtime};

    fn runtime(scene: fn(&mut State)) -> Runtime<State> {
        let mut s = init();
        scene(&mut s);
        let mut rt = Runtime::new(s, view::view, (1240.0, 870.0), Env::new(true, 2.0));
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
    fn demo_scene_shows_devices_screen_and_controls() {
        let rt = runtime(demo);
        for key in ["device-list", "device-0", "device-1", "device-2", "device-screen", "home-button", "lock-button", "device-title"] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        assert!(rt.find("placeholder").is_none());
    }

    #[test]
    fn empty_scene_shows_placeholders() {
        let rt = runtime(empty);
        assert!(rt.find("devices-empty").is_some());
        assert!(rt.find("placeholder").is_some());
        assert!(rt.find("device-screen").is_none());
    }

    #[test]
    fn buttons_queue_actions() {
        let mut rt = runtime(demo);
        press(&mut rt, "home-button");
        press(&mut rt, "lock-button");
        press(&mut rt, "device-1");
        assert_eq!(rt.state.actions, vec![Action::Home, Action::Lock, Action::Select("B2".into())]);
    }

    #[test]
    fn drags_map_to_device_pixels() {
        let mut rt = runtime(demo);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        let (dw, dh) = rt.state.screen_size();
        rt.pointer_down(x + w * 0.25, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_move(x + w * 0.75, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.75, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        let inputs = &rt.state.inputs;
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            matches!(inputs.first(), Some(Input::Down(px, py)) if near(*px, dw as f32 * 0.25) && near(*py, dh as f32 * 0.5)),
            "{inputs:?}"
        );
        assert!(inputs.iter().any(|i| matches!(i, Input::Move(px, _) if near(*px, dw as f32 * 0.75))), "{inputs:?}");
        assert!(matches!(inputs.last(), Some(Input::Up(px, _)) if near(*px, dw as f32 * 0.75)), "{inputs:?}");
    }

    #[test]
    fn debug_panel_tabs_render_and_queue_actions() {
        let mut rt = runtime(demo);
        for key in [
            "debug-panel",
            "tab-device",
            "overlay-blended-layers",
            "toggle-contrast",
            "text-larger",
            "bio-face-match",
            "status-demo",
            "location-tokyo",
        ] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        press(&mut rt, "overlay-blended-layers");
        press(&mut rt, "text-larger");
        assert_eq!(
            rt.state.actions,
            vec![Action::Debug(debug::DebugAction::ToggleOverlay(2)), Action::Debug(debug::DebugAction::TextSize(1))]
        );
        rt.state.actions.clear();
        for (tab, row) in
            [("tab-network", "socket-0"), ("tab-storage", "container-0"), ("tab-processes", "process-0"), ("tab-logs", "log-0")]
        {
            let next = match tab {
                "tab-network" => debug::Tab::Network,
                "tab-storage" => debug::Tab::Storage,
                "tab-processes" => debug::Tab::Processes,
                _ => debug::Tab::Logs,
            };
            rt.update(move |s, _| s.debug.tab = next);
            rt.frame(1.0 / 60.0);
            assert!(rt.find(row).is_some(), "{row} rendered on {tab}");
        }
        press(&mut rt, "logs-2m");
        assert_eq!(rt.state.actions, vec![Action::Debug(debug::DebugAction::Logs("2m"))]);
    }

    #[test]
    fn every_icon_renders() {
        fn icon_view(_: &State, _: &mut rune::Cx<State>) -> rune::El {
            let mut r = rune::row();
            for (i, glyph) in icons::ALL.iter().enumerate() {
                r = r.child(rune::icon(glyph, 16.0).key(format!("icon-{i}")));
            }
            r
        }
        let mut rt = Runtime::new(init(), icon_view, (900.0, 100.0), Env::new(true, 2.0));
        rt.frame(0.0);
        for i in 0..icons::ALL.len() {
            assert!(rt.find(&format!("icon-{i}")).is_some(), "icon {i}");
        }
    }

    #[test]
    fn typing_on_the_focused_screen_queues_keystrokes() {
        let mut rt = runtime(demo);
        press(&mut rt, "device-screen");
        rt.state.inputs.clear();
        rt.key(KeyEvent::from(Key::Text("a".into())));
        rt.key(KeyEvent::from(Key::Enter));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.inputs.len(), 2, "{:?}", rt.state.inputs);
        assert!(matches!(&rt.state.inputs[0], Input::Key(KeyAction::Strokes(s)) if s[0].usage == 0x04));
    }
}
