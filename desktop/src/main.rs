//! sim-rs desktop: one window onto iOS simulator and Android emulator devices, rendered with Rune.
//!
//! Pick a device on the left (grouped by platform, each labelled with its sub-platform); boot it if
//! needed. iOS screens stream from the device framebuffer without copying; Android screens stream
//! from the emulator's display socket. Clicks and drags become touches, typing goes to the device
//! keyboard, and the toolbar has the controls the device's form factor has (Back/Home/Recents, a
//! foldable's hinge, a watch's rotary input, a TV's D-pad). ⌘⇧H is Home, ⌘L Lock, ⌘S saves a
//! screenshot to the Desktop.
//!
//! Handlers only queue [`Action`]s and [`Input`]s; `poll` performs them. That keeps handlers pure,
//! so the headless tests drive the real view without a device.
//!
//! `cargo run` opens the window (`SIM_RS_DEVICE=<udid|name|booted>` preselects a device);
//! `cargo run -- --snap shot.png --scene demo` renders headless (scenes: demo, android, foldable,
//! watch, tv, empty).

mod android;
mod controls;
mod debug;
mod device;
mod icons;
#[cfg(target_os = "macos")]
mod ios;
mod keys;
#[cfg(target_os = "macos")]
mod surface;
mod view;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use rune::{AppSpec, NativeSurface, Pixels, Tx, Waker};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::form_factor::{FormFactor, Kind, Posture, Shape};
use sim_core::gesture::Phase;
use sim_core::hid::{Button, Direction};
use sim_core::platform::Platform;

use device::{Context, Live};
use keys::KeyAction;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Refresh,
    Select(String),
    Boot,
    Shutdown,
    Button(Button),
    ToggleAppearance,
    Screenshot,
    /// Android: rotate 90° clockwise.
    Rotate,
    /// Android foldables: set the hinge.
    Fold(Posture),
    /// Android watches: turn the rotary input.
    Rotary(i32),
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
    /// The live iOS framebuffer, shown without copying.
    pub native: NativeSurface,
    /// The selected iOS device, when it's running.
    pub live: Option<Live>,
    /// The selected Android device, when it's running.
    pub screen: Option<android::Screen>,
    /// The selected Android device's guest console, newest last.
    pub console_log: Vec<String>,
    /// The selected device's screen outline (round Wear OS watches).
    pub shape: Shape,
    /// A pressed hardware button and when to release it.
    held: Option<(Button, f64)>,
    /// What's running in the background ("Booting…", "Shutting down…").
    pub busy: Option<&'static str>,
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
    android_backend: Option<sim_android::Android>,
    next_scan: f64,
    waker: Option<Waker>,
    work_done: Arc<AtomicBool>,
    work_error: Arc<Mutex<Option<String>>>,
    sized_for: Option<(u32, u32)>,
}

impl State {
    pub fn selected_info(&self) -> Option<&DeviceInfo> {
        self.selected.as_ref().and_then(|u| self.devices.iter().find(|d| &d.udid == u))
    }

    /// The device screen in pixels; the device's configured size (or a phone) until the first frame.
    pub fn screen_size(&self) -> (u32, u32) {
        let live = self.live.as_ref().and_then(|l| l.shared.size());
        live.or_else(|| self.screen.as_ref().and_then(android::Screen::device_size)).unwrap_or(self.offline_size)
    }

    /// Whether a device screen is shown (live, or a headless scene's).
    pub fn showing_screen(&self) -> bool {
        self.live.is_some() || self.screen.is_some() || self.offline_screen
    }

    fn refresh(&mut self) {
        let mut list = Vec::new();
        if let Some(ctx) = &self.ctx {
            match device::list(ctx) {
                Ok(l) => list.extend(l),
                Err(e) => self.status = Some(e),
            }
        }
        if let Some(a) = &self.android_backend {
            list.extend(a.devices().into_iter().filter(|d| d.available));
        }
        list.sort_by_key(|d| (d.platform != Platform::Ios, d.state != DeviceState::Booted, d.runtime_id.clone(), d.name.clone()));
        self.devices = list;
    }

    fn connect(&mut self) {
        let Some(info) = self.selected_info().cloned() else { return };
        self.shape = info.kind().map_or(Shape::Rounded, |k| k.profile().shape);
        match info.platform {
            Platform::Ios => {
                self.screen = None;
                let (Some(ctx), Some(waker)) = (&self.ctx, self.waker.clone()) else { return };
                if self.live.as_ref().is_some_and(|l| l.udid == info.udid) {
                    return;
                }
                self.live = None;
                if info.state == DeviceState::Booted {
                    match Live::connect(ctx, &info.udid, self.native.clone(), waker) {
                        Ok(live) => {
                            self.debug.data_path = Some(live.data_path());
                            self.live = Some(live);
                            self.status = None;
                            self.debug.reload(&info.udid);
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
            }
            Platform::Android => {
                self.live = None;
                if self.screen.as_ref().is_some_and(|sc| sc.udid == info.udid && sc.alive()) {
                    return;
                }
                self.screen = None;
                let Some(device) = self.android_device(&info.udid) else { return };
                if let Ok(p) = device.profile() {
                    self.shape = p.shape;
                }
                if let Ok(size) = device.screen_size() {
                    self.offline_size = size;
                }
                let dir = device.avd.as_ref().map(|a| a.dir.clone());
                self.console_log = dir.as_deref().map(|d| sim_android::console_log_tail(d, 200)).unwrap_or_default();
                if info.state == DeviceState::Booted
                    && let Some(dir) = dir
                {
                    match android::Screen::connect(&info.udid, dir, 1080) {
                        Ok(sc) => {
                            self.screen = Some(sc);
                            self.status = None;
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
            }
        }
    }

    fn android_device(&mut self, udid: &str) -> Option<sim_android::Device> {
        let backend = self.android_backend.as_ref()?;
        match backend.device(udid) {
            Ok(d) => Some(d),
            Err(e) => {
                self.status = Some(e);
                None
            }
        }
    }

    /// Runs `job` on a worker thread (booting and shutting down block for seconds); `poll`
    /// refreshes the list when it's done.
    fn run_async(&mut self, label: &'static str, job: impl FnOnce() -> Result<(), String> + Send + 'static) {
        let Some(waker) = self.waker.clone() else { return };
        self.busy = Some(label);
        self.work_done.store(false, Ordering::Relaxed);
        let (done, error) = (self.work_done.clone(), self.work_error.clone());
        std::thread::spawn(move || {
            if let Err(e) = job()
                && let Ok(mut slot) = error.lock()
            {
                *slot = Some(e);
            }
            done.store(true, Ordering::Relaxed);
            waker();
        });
    }
}

fn init() -> State {
    State {
        devices: Vec::new(),
        selected: std::env::var("SIM_RS_DEVICE").ok(),
        pixels: Pixels::new(),
        native: NativeSurface::new(),
        live: None,
        screen: None,
        console_log: Vec::new(),
        shape: Shape::Rounded,
        held: None,
        busy: None,
        status: None,
        debug: debug::Debug::default(),
        socket: None,
        actions: vec![Action::Refresh],
        inputs: Vec::new(),
        offline: false,
        offline_screen: false,
        offline_size: (1206, 2622),
        ctx: None,
        android_backend: None,
        next_scan: 0.0,
        waker: None,
        work_done: Arc::new(AtomicBool::new(false)),
        work_error: Arc::new(Mutex::new(None)),
        sized_for: None,
    }
}

fn info(udid: &str, name: &str, device_type: &str, runtime: &str, state: DeviceState) -> DeviceInfo {
    let runtime_id = format!("com.apple.CoreSimulator.SimRuntime.{runtime}");
    let device_type_id = format!("com.apple.CoreSimulator.SimDeviceType.{device_type}");
    let kind = sim_core::form_factor::apple_kind(&device_type_id, &runtime_id);
    DeviceInfo {
        udid: udid.into(),
        name: name.into(),
        state,
        runtime_id,
        device_type_id,
        available: true,
        platform: Platform::Ios,
        form_factor: None,
        sub_platform: None,
    }
    .with_kind(kind)
}

fn android_info(udid: &str, form_factor: FormFactor, image: &str, state: DeviceState) -> DeviceInfo {
    DeviceInfo {
        udid: udid.into(),
        name: udid.into(),
        state,
        runtime_id: format!("system-images;{image};arm64-v8a"),
        device_type_id: "custom".into(),
        available: true,
        platform: Platform::Android,
        form_factor: None,
        sub_platform: None,
    }
    .with_kind(Kind::new(Platform::Android, form_factor).ok())
}

/// A synthetic screen of `w` x `h` for headless scenes.
fn gradient(s: &mut State, w: u32, h: u32) {
    s.offline_size = (w, h);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 200, 255]);
        }
    }
    s.pixels.set(w, h, rgba);
}

/// Fixed data and a synthetic screen for snapshots and tests.
fn demo(s: &mut State) {
    s.offline = true;
    s.offline_screen = true;
    s.actions.clear();
    s.devices = vec![
        info("A1", "iPhone 17", "iPhone-17", "iOS-26-5", DeviceState::Booted),
        info("B2", "iPhone 17 Pro Max", "iPhone-17-Pro-Max", "iOS-26-5", DeviceState::Shutdown),
        info("D4", "iPad Pro 11-inch (M4)", "iPad-Pro-11-inch-M4", "iOS-26-5", DeviceState::Shutdown),
        info("C3", "iPhone 18 Pro", "iPhone-18-Pro", "iOS-27-0", DeviceState::Shutdown),
        info("E5", "Apple Watch Series 10 (46mm)", "Apple-Watch-Series-10-46mm", "watchOS-26-0", DeviceState::Shutdown),
        android_info("pixel_9", FormFactor::Phone, "android-36;google_apis", DeviceState::Booted),
        android_info("pixel_fold", FormFactor::Foldable, "android-36;google_apis", DeviceState::Shutdown),
        android_info("pixel_tablet", FormFactor::Tablet, "android-36;google_apis", DeviceState::Shutdown),
        android_info("wear_round", FormFactor::Watch, "android-34;android-wear", DeviceState::Shutdown),
        android_info("google_tv", FormFactor::Tv, "android-34;google-tv", DeviceState::Shutdown),
        android_info("automotive", FormFactor::Auto, "android-34;android-automotive", DeviceState::Shutdown),
    ];
    s.selected = Some("A1".into());
    gradient(s, 402, 874);
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
    s.console_log = vec![
        "[    0.000000] Booting Linux on physical CPU 0x0000000000".into(),
        "[    0.871529] init: init first stage started!".into(),
        "[    8.431130] init: Encryption policy of /data/incremental set".into(),
        "[   22.104210] init: starting service 'adbd'...".into(),
    ];
}

/// The demo data with one Android device selected and running.
fn select_android(s: &mut State, udid: &str, (w, h): (u32, u32), shape: Shape) {
    demo(s);
    for d in &mut s.devices {
        d.state = if d.udid == udid { DeviceState::Booted } else { DeviceState::Shutdown };
    }
    s.selected = Some(udid.into());
    s.shape = shape;
    gradient(s, w, h);
}

fn android_scene(s: &mut State) {
    select_android(s, "pixel_9", (360, 800), Shape::Rounded);
}

fn foldable_scene(s: &mut State) {
    select_android(s, "pixel_fold", (552, 460), Shape::Rectangle);
}

fn watch_scene(s: &mut State) {
    select_android(s, "wear_round", (454, 454), Shape::Round);
}

fn tv_scene(s: &mut State) {
    select_android(s, "google_tv", (960, 540), Shape::Rectangle);
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
    s.android_backend = Some(sim_android::Android::from_env());
    match Context::new() {
        Ok(ctx) => s.ctx = Some(ctx),
        // Only worth a status line when there's nothing else to show.
        Err(e) if s.android_backend.as_ref().is_none_or(|a| a.devices().is_empty()) => s.status = Some(e.to_string()),
        Err(_) => {}
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

/// Presses a button: on iOS now, releasing it on a later frame; on Android as one key tap.
fn press_button(s: &mut State, button: Button, t: f64, tx: &mut Tx) {
    if let Some(sc) = &s.screen {
        match button.android_key() {
            Some(key) => sc.key(key),
            None => s.status = Some(format!("the {} button doesn't exist on Android", button.as_str())),
        }
        return;
    }
    let Some(live) = &s.live else { return };
    if let Some((held, _)) = s.held.take() {
        live.button(held, Direction::Up);
    }
    live.button(button, Direction::Down);
    s.held = Some((button, t + 0.06));
    tx.wake_after(Duration::from_millis(60));
}

/// Runs a console action on the selected Android device; failures go to the status line.
fn android_action(s: &mut State, f: impl FnOnce(&sim_android::Device) -> Result<(), String>) {
    let Some(udid) = s.screen.as_ref().map(|sc| sc.udid.clone()) else { return };
    if let Some(d) = s.android_device(&udid)
        && let Err(e) = f(&d)
    {
        s.status = Some(e);
    }
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
                let Some(info) = s.selected_info().cloned() else { continue };
                match info.platform {
                    Platform::Ios => {
                        let Some(waker) = s.waker.clone() else { continue };
                        s.busy = Some("Booting…");
                        s.work_done.store(false, Ordering::Relaxed);
                        device::boot_async(info.udid, s.work_done.clone(), s.work_error.clone(), waker);
                    }
                    Platform::Android => {
                        let Some(backend) = s.android_backend.clone() else { continue };
                        s.run_async("Booting…", move || backend.device(&info.udid)?.boot(&backend, Duration::from_secs(300)));
                    }
                }
            }
            Action::Shutdown => {
                if let Some(live) = s.live.take() {
                    if let Err(e) = live.shutdown() {
                        s.status = Some(e);
                    }
                    drop(live);
                    s.refresh();
                }
                if let Some(sc) = s.screen.take()
                    && let Some(backend) = s.android_backend.clone()
                {
                    s.run_async("Shutting down…", move || backend.device(&sc.udid)?.shutdown());
                }
                s.sized_for = None;
            }
            Action::Button(b) => press_button(s, b, t, tx),
            Action::ToggleAppearance => s.live.iter_mut().for_each(Live::toggle_appearance),
            Action::Screenshot => s.status = Some(screenshot(s)),
            Action::Rotate => android_action(s, sim_android::Device::rotate),
            Action::Fold(posture) => android_action(s, |d| d.fold(posture.angle())),
            Action::Rotary(ticks) => s.screen.iter().for_each(|sc| sc.wheel(ticks)),
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
        if s.offline || (s.live.is_none() && s.screen.is_none()) {
            continue;
        }
        match input {
            Input::Key(KeyAction::Home) => press_button(s, Button::Home, t, tx),
            Input::Key(KeyAction::Lock) => press_button(s, Button::Lock, t, tx),
            Input::Key(KeyAction::Screenshot) => {
                s.actions.push(Action::Screenshot);
                tx.wake_after(Duration::ZERO);
            }
            Input::Key(KeyAction::Strokes(strokes)) if s.screen.is_some() => android_action(s, |d| d.strokes(&strokes)),
            input => apply(s, input),
        }
    }
    let (debug_changed, debug_status) = s.debug.drain();
    if let Some(msg) = debug_status {
        s.status = Some(msg);
    }
    changed |= debug_changed;
    if s.busy.is_some() && s.work_done.load(Ordering::Relaxed) {
        s.busy = None;
        if let Some(e) = s.work_error.lock().ok().and_then(|mut g| g.take()) {
            s.status = Some(e);
        }
        s.refresh();
        s.connect();
        changed = true;
    }
    // Android devices start and stop outside this window (the CLI, AutoMobile, a crash): rescan
    // every two seconds, and follow the selected device's guest console.
    if !s.offline && s.android_backend.is_some() && t >= s.next_scan {
        s.next_scan = t + 2.0;
        tx.wake_after(Duration::from_secs(2));
        let before: Vec<(String, DeviceState)> = s.devices.iter().map(|d| (d.udid.clone(), d.state)).collect();
        s.refresh();
        if s.screen.as_ref().is_some_and(|sc| !sc.alive()) {
            s.screen = None;
        }
        if s.devices.iter().map(|d| (d.udid.clone(), d.state)).ne(before) {
            s.connect();
            changed = true;
        }
        if let Some(dir) = s.screen.as_ref().map(|sc| sc.avd_dir.clone()) {
            let log = sim_android::console_log_tail(&dir, 200);
            if log != s.console_log {
                s.console_log = log;
                changed = true;
            }
        }
    }
    // Fit the window to the device once its screen size is known.
    let live_size = s.live.as_ref().and_then(|l| l.shared.size()).or_else(|| s.screen.as_ref().and_then(android::Screen::device_size));
    if let Some(size) = live_size
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

/// Sends one touch or keystroke input to the device on screen.
fn apply(s: &State, input: Input) {
    let (x, y, phase) = match input {
        Input::Down(x, y) => (x, y, Phase::Down),
        Input::Move(x, y) => (x, y, Phase::Move),
        Input::Up(x, y) => (x, y, Phase::Up),
        Input::Key(KeyAction::Strokes(strokes)) => {
            s.live.iter().for_each(|l| l.strokes(&strokes));
            return;
        }
        Input::Key(_) => return,
    };
    if let Some(live) = &s.live {
        live.touch(x, y, phase);
    } else if let Some(sc) = &s.screen {
        sc.touch(x, y, phase);
    }
}

/// Saves the current frame to the Desktop; returns a status line.
fn screenshot(s: &mut State) -> String {
    let capture = if let Some(live) = &s.live {
        Some(live.capture())
    } else if let Some(udid) = s.screen.as_ref().map(|sc| sc.udid.clone()) {
        s.android_device(&udid).map(|d| d.screenshot())
    } else {
        None
    };
    let (w, h, rgba) = match capture {
        Some(Ok(f)) => (f.width, f.height, f.rgba),
        Some(Err(e)) => return format!("screenshot failed: {e}"),
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
    let result = sim_core::frame::encode_png(&sim_core::frame::Frame { width: w, height: h, rgba })
        .and_then(|png| std::fs::write(&path, png).map_err(|e| e.to_string()));
    match result {
        Ok(()) => format!("saved {}", path.display()),
        Err(e) => format!("screenshot failed: {e}"),
    }
}

fn main() {
    rune::run(AppSpec::<State> {
        scenes: &[
            ("demo", demo),
            ("android", android_scene),
            ("foldable", foldable_scene),
            ("watch", watch_scene),
            ("tv", tv_scene),
            ("empty", empty),
        ],
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

    fn text_of(rt: &Runtime<State>, key: &str) -> String {
        let e = rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} rendered"));
        e.text.or(e.label).unwrap_or_default()
    }

    #[test]
    fn devices_and_title_carry_platform_labels() {
        let rt = runtime(demo);
        assert_eq!(text_of(&rt, "device-0-platform"), "iOS");
        assert_eq!(text_of(&rt, "device-2-platform"), "iPadOS");
        assert_eq!(text_of(&rt, "device-4-platform"), "watchOS");
        assert_eq!(text_of(&rt, "device-title"), "iPhone 17 · iOS · Phone · Booted");
        let row = rt.elements().into_iter().find(|e| e.key.as_deref() == Some("device-2")).and_then(|e| e.label);
        assert_eq!(row.as_deref(), Some("iPad Pro 11-inch (M4), iPadOS · Tablet"));
    }

    #[test]
    fn the_device_list_groups_platforms() {
        let rt = runtime(demo);
        assert_eq!(text_of(&rt, "platform-ios"), "iOS");
        assert_eq!(text_of(&rt, "platform-android"), "Android");
        assert_eq!(text_of(&rt, "device-5-platform"), "Android");
        assert_eq!(text_of(&rt, "device-8-platform"), "Wear OS");
        assert_eq!(text_of(&rt, "device-9-platform"), "Android TV");
        assert_eq!(text_of(&rt, "device-10-platform"), "Android Automotive");
    }

    #[test]
    fn android_devices_get_android_controls_and_their_console() {
        let mut rt = runtime(android_scene);
        assert_eq!(text_of(&rt, "device-title"), "pixel_9 · Android · Phone · Booted");
        for key in ["back-button", "home-button", "recents-button", "lock-button", "rotate-button", "android-panel", "log-0", "log-3"] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        for key in ["appearance-button", "debug-panel", "fold-half", "crown-up"] {
            assert!(rt.find(key).is_none(), "{key} is iOS- or form-factor-specific");
        }
        press(&mut rt, "back-button");
        press(&mut rt, "rotate-button");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Back), Action::Rotate]);
    }

    #[test]
    fn form_factors_get_their_own_controls() {
        let mut rt = runtime(foldable_scene);
        press(&mut rt, "fold-half");
        assert_eq!(rt.state.actions, vec![Action::Fold(Posture::HalfOpened)]);
        let mut rt = runtime(watch_scene);
        assert!(rt.find("recents-button").is_none());
        press(&mut rt, "crown-down");
        assert_eq!(rt.state.actions, vec![Action::Rotary(1)]);
        let mut rt = runtime(tv_scene);
        assert!(rt.find("rotate-button").is_none());
        press(&mut rt, "dpad-select");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::DpadCenter)]);
    }

    #[test]
    fn round_watches_get_a_round_screen() {
        let rt = runtime(watch_scene);
        let ([_, _, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert!((w - h).abs() < 1.0, "a square box for a round screen: {w}x{h}");
        assert_eq!(rt.state.shape, Shape::Round);
        assert!(view::screen_radius(Shape::Round, w) >= w / 2.0);
        assert!(view::screen_radius(Shape::Rounded, w) < w / 4.0);
    }

    #[test]
    fn drags_map_to_android_device_pixels() {
        let mut rt = runtime(android_scene);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert_eq!(rt.state.screen_size(), (360, 800));
        rt.pointer_down(x + w * 0.5, y + h * 0.25);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.5, y + h * 0.25);
        rt.frame(1.0 / 60.0);
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            matches!(rt.state.inputs.first(), Some(Input::Down(px, py)) if near(*px, 180.0) && near(*py, 200.0)),
            "{:?}",
            rt.state.inputs
        );
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
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Home), Action::Button(Button::Lock), Action::Select("B2".into())]);
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
