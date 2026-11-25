//! simee desktop: one window onto iOS simulator and Android emulator devices, rendered with Rune.
//!
//! Pick a device on the left (grouped by platform, each labelled with its sub-platform); boot it if
//! needed. iOS screens stream from the device framebuffer without copying; Android screens stream
//! from the emulator's display socket. Clicks and drags become touches, typing goes to the device
//! keyboard, and the toolbar has the controls the device's form factor has (Back/Home/Recents, a
//! foldable's hinge, a watch's rotary input, a TV's D-pad). ⌘⇧H is Home, ⌘L Lock, ⌘S saves a
//! screenshot to the Desktop.
//!
//! Handlers only queue [`Action`]s and [`Input`]s; `poll` performs them. That keeps handlers pure,
//! so the headless tests drive the real view without a device. Rune runs `poll` at the start of a
//! frame, before that frame's handlers, so a handler that queues work also asks for the next frame
//! ([`queue`], [`request_poll`]); otherwise the work would wait for the next input or timer.
//!
//! Switching away from an Android device pauses its stream and keeps its screen (and last frame)
//! for a while; switching back shows that frame at once and resumes the stream.
//!
//! `cargo run` opens the window (`SIMEE_DEVICE=<udid|name|booted>` preselects a device);
//! `cargo run -- --snap shot.png --scene demo` renders headless (scenes: demo, android, foldable,
//! watch, tv, apple-watch, apple-tv, empty).

mod android;
mod controls;
mod debug;
mod device;
mod icons;
#[cfg(target_os = "macos")]
mod ios;
mod keys;
mod scan;
#[cfg(target_os = "macos")]
mod surface;
mod view;
mod work;

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
    /// Rotate 90° clockwise.
    Rotate,
    /// Android foldables: set the hinge.
    Fold(Posture),
    /// Watches: turn the rotary input (Wear OS) or the Digital Crown (Apple Watch).
    Rotary(i32),
    /// Android XR: head pose and recentering.
    Xr(sim_core::xr::XrCommand),
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
    /// Android screens switched away from, oldest first: streams paused, last frames kept.
    paused: Vec<Paused>,
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
    /// The UI thread's simulator service connection, made when an iOS device is first shown
    /// (the scanner has its own).
    ctx: Option<Result<Context, String>>,
    android_backend: Option<sim_android::Android>,
    /// Lists devices off the UI thread.
    scanner: Option<scan::Scanner>,
    /// The selected AVD as the last scan resolved it, and the UDID that scan was for.
    android: Option<sim_android::Device>,
    android_for: Option<String>,
    /// A refresh asked to resolve `selected` (`booted`, a name) against the next scan.
    resolve_pending: bool,
    /// The next scan should (re)connect the selected device.
    connect_pending: bool,
    /// Status lines from background work.
    outbox: work::Outbox,
    waker: Option<Waker>,
    work_done: Arc<AtomicBool>,
    work_error: Arc<Mutex<Option<String>>>,
    sized_for: Option<(u32, u32)>,
    /// Whether the window shows (not minimised, hidden, or covered where the system says so).
    visible: bool,
}

/// An Android screen switched away from, with what the window showed it with.
struct Paused {
    screen: android::Screen,
    shape: Shape,
    size: (u32, u32),
}

/// Paused Android screens kept at most. Each holds its display connection, a parked frame thread
/// and its last frame (about 10 MB for a 1080x2400 phone); the oldest goes first.
const PAUSED_LIMIT: usize = 4;

/// Asks for a frame now, so `poll` performs what a handler just queued.
pub fn request_poll(tx: &mut Tx) {
    tx.wake_after(Duration::ZERO);
}

/// Queues `action` for `poll` and asks for the frame that performs it.
pub fn queue(s: &mut State, tx: &mut Tx, action: Action) {
    s.actions.push(action);
    request_poll(tx);
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

    /// Asks the scanner for a scan now; `poll` applies it when it arrives.
    fn rescan(&self) {
        if let Some(sc) = &self.scanner {
            sc.ask(self.selected.as_deref());
        }
    }

    /// Applies a scan: the device list, the selected AVD and its console, and a (re)connect when
    /// the list changed or one is pending. Returns whether anything shown changed.
    fn apply_scan(&mut self, scan: scan::Scan) -> bool {
        let before: Vec<(String, DeviceState)> = self.devices.iter().map(|d| (d.udid.clone(), d.state)).collect();
        self.devices = scan.devices;
        let mut changed = self.devices.iter().map(|d| (d.udid.clone(), d.state)).ne(before);
        if let Some(e) = scan.error {
            self.status = Some(e);
        }
        if std::mem::take(&mut self.resolve_pending) {
            if self.selected.as_deref() == Some("booted") {
                self.selected = self.devices.iter().find(|d| d.state == DeviceState::Booted).map(|d| d.udid.clone());
            } else if let Some(q) = self.selected.clone()
                && !self.devices.iter().any(|d| d.udid == q)
            {
                self.selected = self.devices.iter().find(|d| d.name == q).map(|d| d.udid.clone());
            }
            changed = true;
        }
        if scan.selected.is_some() && scan.selected == self.selected {
            self.android_for = scan.selected;
            self.android = match scan.android {
                Some(Ok(d)) => Some(d),
                Some(Err(e)) => {
                    self.status = Some(e);
                    None
                }
                None => None,
            };
            if self.android.is_some() && scan.console_log != self.console_log {
                self.console_log = scan.console_log;
                changed = true;
            }
        }
        if self.screen.as_ref().is_some_and(|sc| !sc.alive()) {
            self.screen = None;
        }
        // A paused device that stopped (here or elsewhere) has nothing to come back to.
        let devices = &self.devices;
        self.paused.retain(|p| p.screen.alive() && devices.iter().any(|d| d.udid == p.screen.udid && d.state == DeviceState::Booted));
        if changed || self.connect_pending {
            self.connect();
        }
        changed
    }

    /// Pauses the Android screen shown, keeping it (and its last frame) for a switch back.
    fn pause_screen(&mut self) {
        let Some(mut screen) = self.screen.take() else { return };
        if !screen.alive() {
            return;
        }
        screen.pause();
        self.paused.retain(|p| p.screen.udid != screen.udid);
        self.paused.push(Paused { screen, shape: self.shape, size: self.offline_size });
        if self.paused.len() > PAUSED_LIMIT {
            self.paused.remove(0);
        }
    }

    /// Shows `udid`'s paused screen again: its last frame at once, live frames as the emulator
    /// sends them. False when there's none, or its emulator went away meanwhile.
    fn resume_screen(&mut self, udid: &str) -> bool {
        let Some(i) = self.paused.iter().position(|p| p.screen.udid == udid) else { return false };
        let mut p = self.paused.remove(i);
        let backend = self.android_backend.clone().unwrap_or_default();
        let console = work::Worker::start("simee-android-console", android::lookup(backend, udid.to_owned()), self.outbox.clone());
        p.screen.resume(console, self.visible);
        if !p.screen.alive() {
            return false;
        }
        (self.shape, self.offline_size) = (p.shape, p.size);
        self.screen = Some(p.screen);
        self.status = None;
        true
    }

    fn connect(&mut self) {
        self.connect_pending = false;
        let Some(info) = self.selected_info().cloned() else { return };
        if self.screen.as_ref().is_some_and(|sc| sc.udid != info.udid) {
            self.pause_screen();
        }
        self.shape = info.kind().map_or(Shape::Rounded, |k| k.profile().shape);
        match info.platform {
            Platform::Ios => {
                self.screen = None;
                if self.ctx.is_none() {
                    self.ctx = Some(Context::new().map_err(|e| e.to_string()));
                }
                let (Some(Ok(ctx)), Some(waker)) = (&self.ctx, self.waker.clone()) else { return };
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
                if info.state == DeviceState::Booted && self.resume_screen(&info.udid) {
                    // Its log and running instance, without waiting for the periodic scan.
                    self.rescan();
                    return;
                }
                // The AVD is resolved on the scanner thread (it reads its config): wait for a
                // scan of this one, unless the last one already failed to resolve it.
                let Some(device) = self.android.clone().filter(|d| d.info.udid == info.udid) else {
                    if self.android_for.as_deref() != Some(info.udid.as_str()) {
                        self.connect_pending = true;
                        self.rescan();
                    }
                    return;
                };
                if let Ok(p) = device.profile() {
                    self.shape = p.shape;
                }
                if let Ok(size) = device.screen_size() {
                    self.offline_size = size;
                }
                let dir = device.avd.as_ref().map(|a| a.dir.clone());
                if info.state == DeviceState::Booted
                    && let Some(dir) = dir
                {
                    // The worker looks the AVD up for each job: this scan's copy goes stale
                    // when the AVD starts, stops or restarts elsewhere.
                    let backend = self.android_backend.clone().unwrap_or_default();
                    let find = android::lookup(backend, info.udid.clone());
                    let console = work::Worker::start("simee-android-console", find, self.outbox.clone());
                    match android::Screen::connect(&info.udid, dir, android::stream_max_width(self.offline_size), console) {
                        Ok(sc) => {
                            sc.set_visible(self.visible);
                            self.screen = Some(sc);
                            self.status = None;
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
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
        selected: std::env::var("SIMEE_DEVICE").ok(),
        pixels: Pixels::new(),
        native: NativeSurface::new(),
        live: None,
        screen: None,
        paused: Vec::new(),
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
        scanner: None,
        android: None,
        android_for: None,
        resolve_pending: false,
        connect_pending: false,
        outbox: work::Outbox::default(),
        waker: None,
        work_done: Arc::new(AtomicBool::new(false)),
        work_error: Arc::new(Mutex::new(None)),
        sized_for: None,
        visible: true,
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
        info("F6", "Apple TV 4K (3rd generation)", "Apple-TV-4K-3rd-generation-4K", "tvOS-26-0", DeviceState::Shutdown),
        info("G7", "Apple Vision Pro", "Apple-Vision-Pro", "xrOS-26-0", DeviceState::Shutdown),
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

/// The demo data with one device selected and running.
fn select_running(s: &mut State, udid: &str, (w, h): (u32, u32), shape: Shape) {
    demo(s);
    for d in &mut s.devices {
        d.state = if d.udid == udid { DeviceState::Booted } else { DeviceState::Shutdown };
    }
    s.selected = Some(udid.into());
    s.shape = shape;
    gradient(s, w, h);
}

fn android_scene(s: &mut State) {
    select_running(s, "pixel_9", (360, 800), Shape::Rounded);
}

fn foldable_scene(s: &mut State) {
    select_running(s, "pixel_fold", (552, 460), Shape::Rectangle);
}

fn watch_scene(s: &mut State) {
    select_running(s, "wear_round", (454, 454), Shape::Round);
}

fn tv_scene(s: &mut State) {
    select_running(s, "google_tv", (960, 540), Shape::Rectangle);
}

fn apple_watch_scene(s: &mut State) {
    select_running(s, "E5", (416, 496), Shape::Rounded);
}

fn apple_tv_scene(s: &mut State) {
    select_running(s, "F6", (960, 540), Shape::Rectangle);
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
    s.outbox = work::Outbox::new(Some(waker.clone()));
    s.waker = Some(waker);
    if s.offline || headless() {
        s.offline = true;
        return;
    }
    s.android_backend = Some(sim_android::Android::from_env());
    // Devices are listed on the scanner thread, so the window paints before the simulator
    // service has answered. Android devices start and stop outside this window (the CLI,
    // AutoMobile, a crash): it rescans every two seconds, following the selected device's
    // guest console.
    if let Some(w) = s.waker.clone() {
        match scan::Scanner::start(Duration::from_secs(2), w) {
            Ok(sc) => s.scanner = Some(sc),
            Err(e) => s.status = Some(format!("device scanner: {e}")),
        }
    }
    if let Some(w) = s.waker.clone() {
        s.debug.start(w);
    }
    // The window also serves the API, so scripts can drive the device it shows.
    let socket = std::env::var_os("SIMEE_SOCKET").map_or_else(|| std::env::temp_dir().join("simee-desktop.sock"), std::path::PathBuf::from);
    match sim_api::serve_in_background(socket.clone()) {
        Ok(()) => s.socket = Some(socket.display().to_string()),
        Err(e) => s.status = Some(format!("API socket {}: {e}", socket.display())),
    }
}

/// The window went off screen or came back: a hidden window's Android screen reads no frames.
fn on_visibility(s: &mut State, visible: bool, _: &mut Tx) {
    s.visible = visible;
    if let Some(sc) = &s.screen {
        sc.set_visible(visible);
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

/// Queues a console action for the selected Android device on its console worker; failures go
/// to the status line when it finishes.
fn android_action(s: &State, f: impl FnOnce(&sim_android::Device) -> Result<(), String> + Send + 'static) {
    if let Some(sc) = &s.screen {
        sc.console(move |d| f(d).map(|()| None));
    }
}

/// Keeps only the newest of each run of pointer moves queued between two polls: a frame's worth
/// of moves is one sample (iOS drops moves under ~16 ms apart anyway, so sending them all could
/// lose the last one), and downs, ups and keys keep their order.
fn coalesce_moves(inputs: Vec<Input>) -> Vec<Input> {
    let mut out: Vec<Input> = Vec::with_capacity(inputs.len());
    for input in inputs {
        if let (Input::Move(..), Some(Input::Move(..))) = (&input, out.last()) {
            out.pop();
        }
        out.push(input);
    }
    out
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
                s.resolve_pending = true;
                s.connect_pending = true;
                s.rescan();
            }
            Action::Select(udid) => {
                s.selected = Some(udid);
                if let Some(sc) = &s.scanner {
                    sc.follow(s.selected.as_deref());
                }
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
                    s.rescan();
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
            Action::Screenshot => {
                if let Some(msg) = screenshot(s) {
                    s.status = Some(msg);
                }
            }
            Action::Rotate => match s.live.as_mut() {
                Some(live) => live.rotate(),
                None => android_action(s, sim_android::Device::rotate),
            },
            Action::Fold(posture) => android_action(s, move |d| d.fold(posture.angle())),
            Action::Rotary(ticks) => {
                s.live.iter().for_each(|l| l.crown(ticks));
                s.screen.iter().for_each(|sc| sc.wheel(ticks));
            }
            Action::Xr(command) => android_action(s, move |d| d.xr(&command).map(drop)),
            Action::Debug(a) => {
                if let Some(udid) = s.live.as_ref().map(|l| l.udid.clone())
                    && let Some(msg) = s.debug.perform(&udid, a)
                {
                    s.status = Some(msg);
                }
            }
        }
    }
    for input in coalesce_moves(std::mem::take(&mut s.inputs)) {
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
            Input::Key(KeyAction::Strokes(strokes)) if s.screen.is_some() => {
                if let Some(sc) = &s.screen {
                    sc.strokes(strokes);
                }
            }
            input => apply(s, input),
        }
    }
    let (debug_changed, debug_status) = s.debug.drain();
    if let Some(msg) = debug_status {
        s.status = Some(msg);
    }
    changed |= debug_changed;
    for line in s.outbox.take() {
        s.status = Some(line);
        changed = true;
    }
    if s.busy.is_some() && s.work_done.load(Ordering::Relaxed) {
        s.busy = None;
        if let Some(e) = s.work_error.lock().ok().and_then(|mut g| g.take()) {
            s.status = Some(e);
        }
        s.connect_pending = true;
        s.rescan();
        changed = true;
    }
    if let Some(scan) = s.scanner.as_ref().and_then(scan::Scanner::take) {
        changed |= s.apply_scan(scan);
    }
    // Fit the window to the device once its screen size is known.
    let live_size = s.live.as_ref().and_then(|l| l.shared.size()).or_else(|| s.screen.as_ref().and_then(android::Screen::device_size));
    if let Some(size) = live_size
        && s.sized_for != Some(size)
    {
        s.sized_for = Some(size);
        let screen_h = android::SCREEN_POINTS;
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

/// Saves the current frame to the Desktop, off the UI thread: an Android device's emulator
/// writes its PNG itself (on the console worker, no decode and re-encode), an iOS frame is
/// copied here and encoded on a thread. The result reaches the status line through the outbox;
/// returns a status line only for what fails at once.
fn screenshot(s: &State) -> Option<String> {
    let path = work::screenshot_path();
    if let Some(sc) = &s.screen {
        sc.console(move |d| match d.screenshot_png_to(&path) {
            Ok(_) => Ok(Some(format!("saved {}", path.display()))),
            Err(e) => Err(format!("screenshot failed: {e}")),
        });
        return None;
    }
    let frame = match s.live.as_ref().map(Live::capture) {
        Some(Ok(f)) => f,
        Some(Err(e)) => return Some(format!("screenshot failed: {e}")),
        None => {
            let (width, height) = s.pixels.size();
            sim_core::frame::Frame { width, height, rgba: s.pixels.rgba() }
        }
    };
    if frame.width == 0 || frame.height == 0 {
        return Some("no frame to save".into());
    }
    work::save_png(frame, path, s.outbox.clone());
    None
}

fn main() {
    let spec = AppSpec::<State> {
        scenes: &[
            ("demo", demo),
            ("android", android_scene),
            ("foldable", foldable_scene),
            ("watch", watch_scene),
            ("tv", tv_scene),
            ("apple-watch", apple_watch_scene),
            ("apple-tv", apple_tv_scene),
            ("empty", empty),
        ],
        setup: Some(setup),
        poll: Some(poll),
        min_size: Some((1000.0, 560.0)),
        ..AppSpec::new("simee", (1240.0, 870.0), init, view::view)
    };
    // Live Rust editing runs the spec its own way, without the visibility hook.
    #[cfg(feature = "hot")]
    rune::run(spec);
    #[cfg(not(feature = "hot"))]
    rune::App::new(spec).on_visibility(on_visibility).run();
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
        assert_eq!(text_of(&rt, "device-5-platform"), "tvOS");
        assert_eq!(text_of(&rt, "device-6-platform"), "visionOS");
        assert_eq!(text_of(&rt, "device-7-platform"), "Android");
        assert_eq!(text_of(&rt, "device-10-platform"), "Wear OS");
        assert_eq!(text_of(&rt, "device-11-platform"), "Android TV");
        assert_eq!(text_of(&rt, "device-12-platform"), "Android Automotive");
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
        // Apple's: the crown and side button on a watch, the Siri Remote on a TV.
        let mut rt = runtime(apple_watch_scene);
        assert_eq!(text_of(&rt, "device-title"), "Apple Watch Series 10 (46mm) · watchOS · Watch · Booted");
        assert!(rt.find("lock-button").is_none() && rt.find("rotate-button").is_none());
        press(&mut rt, "crown-up");
        press(&mut rt, "side-button");
        assert_eq!(rt.state.actions, vec![Action::Rotary(-1), Action::Button(Button::Side)]);
        let mut rt = runtime(apple_tv_scene);
        press(&mut rt, "back-button");
        press(&mut rt, "dpad-left");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Menu), Action::Button(Button::DpadLeft)]);
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

    /// 1000 moves between two polls send one, the last, before the up; downs, ups and keys keep
    /// their places.
    #[test]
    fn queued_moves_coalesce_to_the_newest_before_the_up() {
        let mut inputs = vec![Input::Down(0.0, 0.0)];
        inputs.extend((1..=1000).map(|i| Input::Move(i as f32, 0.0)));
        inputs.push(Input::Up(1000.0, 0.0));
        inputs.push(Input::Down(5.0, 5.0));
        inputs.push(Input::Move(6.0, 6.0));
        inputs.push(Input::Key(KeyAction::Home));
        inputs.push(Input::Move(7.0, 7.0));
        inputs.push(Input::Move(8.0, 8.0));
        inputs.push(Input::Up(8.0, 8.0));
        assert_eq!(
            coalesce_moves(inputs),
            [
                Input::Down(0.0, 0.0),
                Input::Move(1000.0, 0.0),
                Input::Up(1000.0, 0.0),
                Input::Down(5.0, 5.0),
                Input::Move(6.0, 6.0),
                Input::Key(KeyAction::Home),
                Input::Move(8.0, 8.0),
                Input::Up(8.0, 8.0),
            ]
        );
    }

    /// A scan replaces the list, resolves a `booted` selection when a refresh asked, and an
    /// AVD that failed to resolve says why instead of asking again.
    #[test]
    fn scans_update_the_list_and_selection() {
        let mut s = init();
        demo(&mut s);
        s.offline = false;
        s.selected = Some("booted".into());
        s.resolve_pending = true;
        let mut devices = s.devices.clone();
        for d in &mut devices {
            d.state = if d.udid == "pixel_fold" { DeviceState::Booted } else { DeviceState::Shutdown };
        }
        assert!(s.apply_scan(scan::Scan { devices: devices.clone(), ..scan::Scan::default() }));
        assert_eq!(s.selected.as_deref(), Some("pixel_fold"));
        assert!(s.connect_pending, "waits for a scan that resolves the AVD");
        let failed = scan::Scan {
            devices: devices.clone(),
            selected: Some("pixel_fold".into()),
            android: Some(Err("pixel_fold: config unreadable".into())),
            ..scan::Scan::default()
        };
        s.apply_scan(failed);
        assert_eq!(s.status.as_deref(), Some("pixel_fold: config unreadable"));
        assert!(!s.connect_pending && s.screen.is_none(), "no retry loop for an AVD that can't resolve");
        assert!(!s.apply_scan(scan::Scan { devices, ..scan::Scan::default() }), "same list: nothing changed");
    }

    fn element(rt: &Runtime<State>, key: &str) -> rune::ElementInfo {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} rendered"))
    }

    /// The selected device's row stands out from the sidebar (and says it's selected to
    /// assistive technology); the others don't, and the highlight follows a new selection.
    #[test]
    fn the_selected_device_is_highlighted() {
        let mut rt = runtime(demo);
        let sel = view::selection(rune::tok::theme(true), true);
        let (row0, row1) = (element(&rt, "device-0"), element(&rt, "device-1"));
        assert!(row0.item_selected && !row1.item_selected, "only iPhone 17 is selected");
        assert_eq!(row0.fill, sel.fill, "the selected row is tinted");
        assert_ne!(row1.fill, sel.fill);
        assert_eq!(element(&rt, "device-0-bar").fill, sel.bar, "and marked with the accent bar");
        assert_eq!(element(&rt, "device-1-bar").fill.a, 0.0, "the bar's slot is empty on the others");
        press(&mut rt, "device-1");
        rt.update(|s, tx| {
            poll(s, 1.0, tx);
        });
        rt.frame(1.0 / 60.0);
        assert!(element(&rt, "device-1").item_selected && !element(&rt, "device-0").item_selected);
    }

    /// WCAG contrast of two opaque colours (linear light in, as Rune's colours are).
    fn contrast(a: rune::Color, b: rune::Color) -> f32 {
        let (la, lb) = (a.luminance() + 0.05, b.luminance() + 0.05);
        la.max(lb) / la.min(lb)
    }

    /// In both themes: the row's text (and its badge, drawn in the primary text colour when
    /// selected) reads at 4.5:1 on the tint, and the accent bar stands at 3:1 against the sidebar.
    #[test]
    fn the_selection_highlight_has_accessible_contrast_in_light_and_dark() {
        for dark in [true, false] {
            let t = rune::tok::theme(dark);
            let sel = view::selection(t, dark);
            assert!(contrast(t.text, sel.fill) >= 4.5, "text on the tint (dark: {dark}): {}", contrast(t.text, sel.fill));
            assert!(contrast(sel.bar, t.card) >= 3.0, "bar on the sidebar (dark: {dark}): {}", contrast(sel.bar, t.card));
            assert_ne!(sel.fill, t.card, "the tint differs from the sidebar (dark: {dark})");
        }
    }

    /// `poll` runs at the start of a frame, before that frame's handlers: a handler that queues
    /// work asks for the next frame, so the work starts at once instead of on the next input.
    #[test]
    fn handlers_that_queue_work_ask_for_a_poll() {
        let mut rt = runtime(demo);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.wake_deadline(), None, "a still window sleeps");
        for key in ["device-1", "home-button", "overlay-blended-layers", "tab-network", "debug-reload"] {
            press(&mut rt, key);
            assert!(rt.wake_deadline().is_some(), "{key} asks for a poll");
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.wake_deadline(), None, "{key}: one poll, then the window sleeps again");
        }
        let mut rt = runtime(demo);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        rt.pointer_down(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        assert!(rt.wake_deadline().is_some(), "a touch asks for a poll");
        rt.pointer_up(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.frame(1.0 / 60.0);
        rt.key(KeyEvent::from(Key::Text("a".into())));
        rt.frame(1.0 / 60.0);
        assert!(rt.wake_deadline().is_some(), "a keystroke asks for a poll");
    }

    /// A fake worker finishing queued work wakes the window, and the poll that wakes it shows
    /// the result.
    #[test]
    fn finished_work_wakes_the_window_and_the_next_poll_shows_it() {
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let mut rt = runtime(demo);
        rt.state.outbox = work::Outbox::new(Some(Arc::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        })));
        let worker = work::Worker::start("fake-worker", (), rt.state.outbox.clone());
        assert!(worker.run(|()| Ok(Some("rotated".into()))));
        let start = std::time::Instant::now();
        while woken.load(Ordering::Relaxed) == 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "the worker woke the window");
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut changed = false;
        rt.update(|s, tx| changed = poll(s, 1.0, tx));
        assert!(changed && rt.state.status.as_deref() == Some("rotated"));
    }

    /// A display socket that takes one client and records its lines.
    fn recording_display(name: &str) -> (std::path::PathBuf, Arc<Mutex<Vec<String>>>) {
        use std::io::BufRead;
        let dir = std::path::PathBuf::from("/tmp").join(format!("rs-main-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = std::os::unix::net::UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let l = lines.clone();
        std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            for line in std::io::BufReader::new(conn).lines().map_while(Result::ok) {
                l.lock().expect("lines").push(line);
            }
        });
        (dir, lines)
    }

    /// Switching away from an Android device pauses its stream and keeps its last frame; switching
    /// back shows that frame at once (the same buffer, no placeholder, no reconnect) and asks the
    /// emulator for live frames again.
    #[test]
    fn a_paused_device_shows_its_last_frame_when_switched_back() {
        let (dir, lines) = recording_display("paused");
        let mut rt = runtime(android_scene);
        rt.update(|s, _| {
            s.offline = false;
            s.offline_screen = false;
            let console =
                work::Worker::start("test-console", android::lookup(sim_android::Android::default(), "pixel_9".into()), s.outbox.clone());
            let sc = android::Screen::connect("pixel_9", dir.clone(), 1080, console).expect("connect");
            sc.pixels.set(2, 1, vec![9; 8]);
            s.screen = Some(sc);
        });
        rt.frame(1.0 / 60.0);
        let shown = rt.state.screen.as_ref().map(|sc| sc.pixels.signature()).expect("a screen");
        // Away to a device that isn't running, and back.
        for udid in ["pixel_fold", "pixel_9"] {
            rt.update(|s, tx| {
                s.actions.push(Action::Select(udid.into()));
                poll(s, 1.0, tx);
            });
            rt.frame(1.0 / 60.0);
            if udid == "pixel_fold" {
                assert!(rt.find("placeholder").is_some() && rt.state.screen.is_none(), "the fold isn't running");
            }
        }
        assert!(rt.find("placeholder").is_none() && rt.find("device-screen").is_some(), "the screen shows at once");
        let sc = rt.state.screen.as_ref().expect("the paused screen came back");
        assert_eq!(sc.pixels.signature(), shown, "the same buffer and frame, not a new one");
        assert_eq!(sc.pixels.rgba(), [9; 8]);
        let start = std::time::Instant::now();
        while lines.lock().expect("lines").len() < 4 {
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}", lines.lock().expect("lines"));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(*lines.lock().expect("lines"), ["shm", "hello 1080", "hello 0", "hello 1080"], "paused, then resumed");
        let _ = std::fs::remove_dir_all(dir);
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
