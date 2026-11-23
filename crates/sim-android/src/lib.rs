//! The Android backend, client side. Android devices are AVDs (the SDK's own format, read by
//! `emu-avd`) run by the `emu` emulator binary, one process per device: Hypervisor.framework
//! allows one VM per process, and a crashed guest can't take the window or the API server down.
//!
//! This crate starts and stops `emu`, and drives a running instance the way any tool can: over
//! its console port (the stock text protocol `adb emu` speaks) and its display socket
//! (`sim_core::stream`). It has no FFI, so it builds and is tested on every host.
//!
//! An AVD's name is its identifier (the `udid` of its [`DeviceInfo`]): AVD names are unique within
//! an AVD home, and Android has no other stable id for a device that isn't running.

pub mod console;
pub mod display;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use emu_avd::{Avd, Env, NewAvd, Running};
use sim_core::device::{DeviceInfo, DeviceState, RuntimeInfo, Selection, select};
use sim_core::form_factor::{FormFactor, InputMethod, Kind, Profile, android_profile};
use sim_core::frame::Frame;
use sim_core::gesture::{Phase, Sample};
use sim_core::hid::{Button, Direction, KEY_LEFT_SHIFT, KeyStroke, keystroke, linux_keycode};
use sim_core::platform::{AndroidOs, Platform, SubPlatform};
use sim_core::stream::Input;
use sim_core::vhal;
use sim_core::xr::XrCommand;

/// The Android backend over one SDK and AVD home.
#[derive(Clone, Debug)]
pub struct Android {
    pub env: Env,
    /// Where running emulators advertise themselves (default: [`emu_avd::running_dir`]).
    pub running_dir: Option<PathBuf>,
}

impl Default for Android {
    fn default() -> Self {
        Self::from_env()
    }
}

/// A running emulator answers its console within this long of advertising itself.
const CONSOLE_GRACE: Duration = Duration::from_secs(5);

impl Android {
    /// The SDK and AVD home from the environment, resolved like the SDK tools do.
    pub fn from_env() -> Self {
        Android::new(Env::from_env())
    }

    pub fn new(env: Env) -> Self {
        Android { env, running_dir: emu_avd::running_dir() }
    }

    fn running(&self) -> Vec<Running> {
        self.running_dir.as_deref().map(emu_avd::running_instances_in).unwrap_or_default()
    }

    /// Every AVD, labelled with its form factor; running ones (emu-rs or stock) are `Booted`.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let running = self.running();
        self.env.list_avds().into_iter().map(|name| self.describe(&name, &running).info).collect()
    }

    /// Resolves a UDID-or-name query (`booted` is the only running device) to a device.
    pub fn device(&self, query: &str) -> Result<Device, String> {
        let running = self.running();
        let devices: Vec<Device> = self.env.list_avds().into_iter().map(|name| self.describe(&name, &running)).collect();
        let infos: Vec<DeviceInfo> = devices.iter().map(|d| d.info.clone()).collect();
        match select(&infos, query) {
            Selection::One(i) => devices.into_iter().nth(i).ok_or_else(|| format!("no device matches `{query}`")),
            Selection::None => Err(format!("no device matches `{query}`")),
            Selection::Ambiguous => Err(format!("`{query}` matches several devices; use a UDID")),
        }
    }

    fn describe(&self, name: &str, running: &[Running]) -> Device {
        let running = running.iter().find(|r| r.avd_name == name).cloned();
        let state = if running.is_some() { DeviceState::Booted } else { DeviceState::Shutdown };
        let mut info = DeviceInfo {
            udid: name.to_owned(),
            name: name.to_owned(),
            state,
            runtime_id: String::new(),
            device_type_id: "custom".into(),
            available: false,
            platform: Platform::Android,
            form_factor: None,
            sub_platform: None,
        };
        let avd = self.env.load(name).ok();
        if let Some(avd) = &avd {
            let profile = avd.profile();
            info = info.with_kind(Some(profile.kind));
            info.name = avd.get("avd.ini.displayname").unwrap_or(name).to_owned();
            info.runtime_id = runtime_id(&avd.sysdir);
            info.device_type_id = avd.get("hw.device.name").unwrap_or("custom").to_owned();
            info.available = true;
        }
        Device { info, avd, running }
    }

    /// Installed system images, as runtimes (`system-images;android-36;google_apis;arm64-v8a`).
    pub fn runtimes(&self) -> Vec<RuntimeInfo> {
        let Some(root) = &self.env.sdk_root else { return Vec::new() };
        let mut out = Vec::new();
        let dirs =
            |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>();
        for platform in dirs(&root.join("system-images")) {
            for tag in dirs(&platform) {
                for abi in dirs(&tag) {
                    if !abi.join("system.img").is_file() && !abi.join("kernel-ranchu").is_file() {
                        continue;
                    }
                    let props = std::fs::read_to_string(abi.join("source.properties")).map(|t| emu_avd::parse_ini(&t)).unwrap_or_default();
                    let api = props.get("AndroidVersion.ApiLevel").cloned().unwrap_or_default();
                    let tag_name = tag.file_name().map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
                    out.push(RuntimeInfo {
                        identifier: runtime_id(&abi),
                        name: format!("Android {api} ({tag_name})"),
                        version: api,
                        build: props.get("Pkg.Revision").cloned().unwrap_or_default(),
                        available: true,
                        platform: Platform::Android,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.identifier.cmp(&b.identifier));
        out
    }

    /// Creates an AVD from an installed system image, shaped like `form_factor` (else like the
    /// image's own kind: a phone for mobile images, a watch for Wear OS, ...). Rejects a form
    /// factor the image can't boot, such as a tablet from a Wear OS image.
    pub fn create(&self, name: &str, system_image: &str, form_factor: Option<FormFactor>) -> Result<DeviceInfo, String> {
        let sysdir = sysdir_path(system_image)?;
        let tag = sysdir.split('/').nth(2).unwrap_or_default().to_owned();
        let image = android_profile(|k| (k == "tag.id").then(|| tag.clone())).kind.form_factor;
        let wanted = form_factor.unwrap_or(image);
        let kind = Kind::new(Platform::Android, wanted).map_err(|e| e.to_string())?;
        let mobile = |f: FormFactor| matches!(f, FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable);
        if wanted != image && !(mobile(wanted) && mobile(image)) {
            let label = Kind::new(Platform::Android, image).map(|k| k.label()).unwrap_or_default();
            return Err(format!("system image `{system_image}` (tag `{tag}`) boots {label} devices, not {}", wanted.as_str()));
        }
        self.env.create_avd(&NewAvd::for_profile(name, &sysdir, &kind.image_profile(&tag))).map_err(|e| e.to_string())?;
        Ok(self.describe(name, &[]).info)
    }

    /// Deletes a stopped AVD (its directory and pointer file).
    pub fn delete(&self, device: &Device) -> Result<(), String> {
        if device.running.is_some() {
            return Err(format!("{} is running; shut it down first", device.info.name));
        }
        let avd = device.avd.as_ref().ok_or_else(|| format!("{} has no AVD directory", device.info.name))?;
        std::fs::remove_dir_all(&avd.dir).map_err(|e| format!("{}: {e}", avd.dir.display()))?;
        let pointer = self.env.avd_home.join(format!("{}.ini", device.info.udid));
        if pointer.is_file() {
            std::fs::remove_file(&pointer).map_err(|e| format!("{}: {e}", pointer.display()))?;
        }
        Ok(())
    }

    /// `adb`, from the SDK's platform-tools or the PATH.
    pub fn adb(&self) -> Option<PathBuf> {
        let sdk = self.env.sdk_root.as_ref().map(|r| r.join("platform-tools/adb")).filter(|p| p.is_file());
        sdk.or_else(|| Command::new("adb").arg("version").output().ok().filter(|o| o.status.success()).map(|_| PathBuf::from("adb")))
    }
}

/// `system-images;android-36;google_apis;arm64-v8a` for an image directory.
fn runtime_id(sysdir: &Path) -> String {
    let parts: Vec<String> = sysdir.components().rev().take(4).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    match parts.as_slice() {
        [abi, tag, platform, root] if root == "system-images" => format!("system-images;{platform};{tag};{abi}"),
        _ => sysdir.display().to_string(),
    }
}

/// `system-images/android-36/google_apis/arm64-v8a` from either the SDK package form
/// (`system-images;android-36;google_apis;arm64-v8a`) or the path form.
fn sysdir_path(image: &str) -> Result<String, String> {
    let path = image.replace(';', "/");
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["system-images", platform, tag, abi] => Ok(format!("system-images/{platform}/{tag}/{abi}")),
        [platform, tag, abi] => Ok(format!("system-images/{platform}/{tag}/{abi}")),
        _ => Err(format!("system image `{image}` should look like system-images;android-36;google_apis;arm64-v8a")),
    }
}

/// The `emu` binary: `$EMU_BIN`, else next to this executable, else this checkout's build, else
/// the PATH.
pub fn emu_binary() -> PathBuf {
    if let Some(p) = std::env::var_os("EMU_BIN") {
        return PathBuf::from(p);
    }
    if let Some(beside) = std::env::current_exe().ok().map(|e| e.with_file_name("emu")).filter(|p| p.is_file()) {
        return beside;
    }
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    ["release", "debug"].iter().map(|p| target.join(p).join("emu")).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("emu"))
}

/// One AVD, running or not.
#[derive(Clone, Debug)]
pub struct Device {
    pub info: DeviceInfo,
    /// `None` when its config or system image can't be read.
    pub avd: Option<Avd>,
    pub running: Option<Running>,
}

impl Device {
    fn running(&self) -> Result<&Running, String> {
        self.running.as_ref().ok_or_else(|| format!("{} isn't running; boot it first", self.info.name))
    }

    fn avd(&self) -> Result<&Avd, String> {
        self.avd.as_ref().ok_or_else(|| format!("{}: its config or system image can't be read", self.info.name))
    }

    /// The form factor profile (screen, shape, fold, inputs).
    pub fn profile(&self) -> Result<Profile, String> {
        Ok(self.avd()?.profile())
    }

    /// Sends console commands in one session; returns the reply lines.
    pub fn console(&self, commands: &[String]) -> Result<Vec<String>, String> {
        console::send(self.running()?.console_port, commands)
    }

    /// Screen size in pixels (the AVD's LCD).
    pub fn screen_size(&self) -> Result<(u32, u32), String> {
        let (w, h, _) = self.avd()?.lcd();
        Ok((w, h))
    }

    /// Starts `emu` for this AVD (unless it's running) and waits until it has booted: until
    /// `sys.boot_completed` is 1 when adb is available, else until the display has a frame.
    /// The emulator's own output goes to `<avd>/emu-rs/emu.log`.
    pub fn boot(&self, android: &Android, timeout: Duration) -> Result<(), String> {
        let avd = self.avd()?;
        let deadline = Instant::now() + timeout;
        let mut child = None;
        if self.running.is_none() {
            let log_dir = avd.dir.join("emu-rs");
            std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
            let log = std::fs::File::create(log_dir.join("emu.log")).map_err(|e| e.to_string())?;
            let bin = emu_binary();
            let signer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sign-run.sh");
            let mut cmd = if cfg!(target_os = "macos") && signer.is_file() {
                let mut c = Command::new(signer);
                c.arg(&bin);
                c
            } else {
                Command::new(&bin)
            };
            let err = log.try_clone().map_err(|e| e.to_string())?;
            let spawned = cmd.args(["-avd", &self.info.udid, "-no-boot-anim"]).stdin(Stdio::null()).stdout(log).stderr(err).spawn();
            child = Some(spawned.map_err(|e| format!("starting {}: {e}", bin.display()))?);
        }
        let adb = android.adb();
        loop {
            if let Some(c) = child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                let log = avd.dir.join("emu-rs/emu.log");
                let tail = std::fs::read_to_string(&log).unwrap_or_default();
                let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_owned();
                return Err(format!("emu exited ({status}) before booting: {last} (log: {})", log.display()));
            }
            if let Some(r) = android.running().into_iter().find(|r| r.avd_name == self.info.udid)
                && booted(&r, adb.as_deref())
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("{} did not finish booting within {}s", self.info.name, timeout.as_secs()));
            }
            sleep(Duration::from_millis(500));
        }
    }

    /// Asks the emulator to quit (console `kill`) and waits for it to go.
    pub fn shutdown(&self) -> Result<(), String> {
        let r = self.running()?;
        // `kill` closes the session after its reply, so the reply may be cut short.
        let _ = console::send(r.console_port, &["kill".into()]);
        let deadline = Instant::now() + Duration::from_secs(15);
        while r.alive() && Instant::now() < deadline {
            sleep(Duration::from_millis(200));
        }
        if r.alive() { Err(format!("{} is still running after `kill`", self.info.name)) } else { Ok(()) }
    }

    /// The current screen at full resolution, from the emulator's renderer, decoded.
    pub fn screenshot(&self) -> Result<Frame, String> {
        let (_, _, png) = self.screenshot_png()?;
        decode_png(&png)
    }

    /// The current screen as the emulator encodes it: width, height and PNG bytes, with no
    /// decode and re-encode on the way.
    pub fn screenshot_png(&self) -> Result<(u32, u32, Vec<u8>), String> {
        let path = std::env::temp_dir().join(format!("sim-android-{}-{}.png", std::process::id(), self.info.udid));
        let size = self.screenshot_png_to(&path);
        let png = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()));
        let _ = std::fs::remove_file(&path);
        let (w, h) = size?;
        Ok((w, h, png?))
    }

    /// Has the emulator write the current screen to `path` as PNG (it runs on this host);
    /// returns the width and height.
    pub fn screenshot_png_to(&self, path: &Path) -> Result<(u32, u32), String> {
        let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.console(&[format!("screenshot {}", path.display())])?;
        let mut header = [0u8; 24];
        std::fs::File::open(&path)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut header))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        sim_core::frame::png_size(&header).ok_or_else(|| format!("{} isn't a PNG", path.display()))
    }

    /// Plays touch samples (device pixels) over one display connection.
    pub fn play(&self, samples: &[Sample]) -> Result<(), String> {
        let mut display = self.display(self.touch_screen()?)?;
        for s in samples {
            sleep(s.delay);
            display.send(&Input::Touch { x: s.point.x as f32, y: s.point.y as f32, phase: s.phase })?;
        }
        Ok(())
    }

    /// One touch sample at device pixels.
    pub fn touch(&self, x: f64, y: f64, phase: Phase) -> Result<(), String> {
        self.display(self.touch_screen()?)?.send(&Input::Touch { x: x as f32, y: y as f32, phase })
    }

    /// A touch on the touchpad beside the screen (`hw.touchpad0`: Android XR display glasses), in
    /// touchpad units: `phase` is down, move, up or tap. Through emu-rs's console `touchpad`.
    pub fn touchpad(&self, x: u32, y: u32, phase: &str) -> Result<(), String> {
        self.touchpad_size()?;
        self.console(&[format!("touchpad {x} {y} {phase}")]).map(drop)
    }

    /// A swipe across the touchpad from `from` to `to` (touchpad units) over about `duration`.
    pub fn touchpad_swipe(&self, from: (u32, u32), to: (u32, u32), duration: Duration) -> Result<(), String> {
        let (w, h) = self.touchpad_size()?;
        if from.0.max(to.0) >= w || from.1.max(to.1) >= h {
            return Err(format!("the touchpad is {w}x{h}"));
        }
        const STEPS: u32 = 10;
        let lerp = |a: u32, b: u32, i: u32| (f64::from(a) + (f64::from(b) - f64::from(a)) * f64::from(i) / f64::from(STEPS)).round() as u32;
        let mut lines = vec![format!("touchpad {} {} down", from.0, from.1)];
        lines.extend((1..=STEPS).map(|i| format!("touchpad {} {} move", lerp(from.0, to.0, i), lerp(from.1, to.1, i))));
        lines.push(format!("touchpad {} {} up", to.0, to.1));
        let pause = duration / STEPS;
        for line in lines {
            self.console(&[line])?;
            sleep(pause);
        }
        Ok(())
    }

    /// The touchpad's size, if the AVD has one.
    fn touchpad_size(&self) -> Result<(u32, u32), String> {
        self.avd()?
            .touchpad()
            .ok_or_else(|| format!("{} has no touchpad (hw.touchpad0; Android XR display glasses have one)", self.info.name))
    }

    /// A connection to the running emulator's display socket (frames and input).
    fn display(&self, avd: &Avd) -> Result<display::Display, String> {
        self.running()?;
        display::Display::connect(&avd.dir)
    }

    /// The AVD, if its screen takes touch (an SDK TV's says `hw.screen=no-touch`).
    fn touch_screen(&self) -> Result<&Avd, String> {
        let avd = self.avd()?;
        if avd.touch_screen() {
            Ok(avd)
        } else {
            Err(format!("{} has no touch screen (hw.screen=no-touch); use button up|down|left|right|select", self.info.name))
        }
    }

    /// Presses or releases a hardware button.
    pub fn button(&self, button: Button, direction: Direction) -> Result<(), String> {
        let key = button.android_key().ok_or_else(|| format!("the {} button doesn't exist on Android", button.as_str()))?;
        self.console(&[format!("event send EV_KEY:{key}:{}", u8::from(direction == Direction::Down))]).map(drop)
    }

    /// Presses or releases a key by HID usage.
    pub fn key(&self, usage: u32, direction: Direction) -> Result<(), String> {
        let code = linux_keycode(usage).ok_or_else(|| format!("HID usage {usage:#x} has no Android key"))?;
        self.console(&[format!("event send EV_KEY:{code}:{}", u8::from(direction == Direction::Down))]).map(drop)
    }

    /// Types text on the hardware keyboard (US layout), in one console session.
    pub fn type_text(&self, text: &str) -> Result<(), String> {
        let strokes = text
            .chars()
            .map(|c| keystroke(c).ok_or_else(|| format!("no key for {c:?} on the US layout")))
            .collect::<Result<Vec<_>, _>>()?;
        self.strokes(&strokes)
    }

    /// Presses each key stroke (HID usage, with shift if set), in one console session.
    pub fn strokes(&self, strokes: &[KeyStroke]) -> Result<(), String> {
        let shift = linux_keycode(KEY_LEFT_SHIFT).unwrap_or(42);
        let mut commands = Vec::with_capacity(strokes.len());
        for stroke in strokes {
            let code = linux_keycode(stroke.usage).ok_or_else(|| format!("HID usage {:#x} has no Android key", stroke.usage))?;
            commands.push(if stroke.shift {
                format!("event send EV_KEY:{shift}:1 EV_KEY:{code}:1 EV_KEY:{code}:0 EV_KEY:{shift}:0")
            } else {
                format!("event send EV_KEY:{code}:1 EV_KEY:{code}:0")
            });
        }
        self.console(&commands).map(drop)
    }

    /// Simulated location.
    pub fn set_location(&self, latitude: f64, longitude: f64) -> Result<(), String> {
        // The console takes longitude first.
        self.console(&[format!("geo fix {longitude} {latitude}")]).map(drop)
    }

    /// Rotates the device 90° clockwise.
    pub fn rotate(&self) -> Result<(), String> {
        self.console(&["rotate".into()]).map(drop)
    }

    fn require(&self, input: InputMethod, what: &str) -> Result<(), String> {
        match self.info.kind() {
            Some(k) if k.accepts(input) => Ok(()),
            Some(k) => Err(format!("{what} needs a device with {}; {} is {}", input.label(Platform::Android), self.info.name, k.label())),
            None => Err(format!("{what}: {}'s form factor is unknown", self.info.name)),
        }
    }

    /// Sets a foldable's hinge angle (0 closed, 180 flat). Foldables only.
    pub fn fold(&self, angle: f32) -> Result<(), String> {
        self.require(InputMethod::Hinge, "folding")?;
        if !(0.0..=180.0).contains(&angle) {
            return Err(format!("hinge angle {angle} is outside 0-180"));
        }
        self.console(&[format!("sensor set hinge-angle0 {angle}")]).map(drop)
    }

    /// Turns the rotary input (Wear OS crown or bezel) by `ticks` clicks, which the emulator
    /// scales to stock's 15 units per click (`sim_core::stream::wheel_units`). Watches only.
    pub fn rotary(&self, ticks: i32) -> Result<(), String> {
        self.require(InputMethod::Rotary, "rotary input")?;
        self.display(self.avd()?)?.send(&Input::Wheel(ticks))
    }

    /// An XR control (head pose, environment, input mode, pointer) through emu-rs's `xr` console
    /// command; returns its reply lines. Android XR devices only.
    pub fn xr(&self, command: &XrCommand) -> Result<Vec<String>, String> {
        match self.info.kind() {
            Some(k) if k.sub_platform == SubPlatform::Android(AndroidOs::Xr) => self.console(&[command.console()]),
            Some(k) => Err(format!("XR controls need an Android XR device; {} is {}", self.info.name, k.label())),
            None => Err(format!("XR controls: {}'s form factor is unknown", self.info.name)),
        }
    }

    /// Sets a vehicle HAL property through the emulator's `car` console command (an emu-rs
    /// extension). `property` is a VHAL name (`GEAR_SELECTION`, `gear`) or id; `value` is typed
    /// by the property (`drive`, `12.5`, `true`). Automotive devices only.
    pub fn vehicle_set(&self, property: &str, area_id: i32, value: &str) -> Result<(), String> {
        self.require(InputMethod::HeadUnit, "the vehicle HAL")?;
        // Checked here so a bad name or value fails before any emulator is involved.
        let prop = vhal::property(property)?;
        vhal::parse_value(prop, area_id, value)?;
        self.console(&[format!("car set {prop:#x} {area_id} {value}")]).map(drop)
    }

    /// A vehicle HAL property's current value, read from the guest. Automotive devices only.
    pub fn vehicle_get(&self, property: &str, area_id: i32) -> Result<vhal::Reading, String> {
        self.require(InputMethod::HeadUnit, "the vehicle HAL")?;
        let prop = vhal::property(property)?;
        let lines = self.console(&[format!("car get {prop:#x} {area_id}")])?;
        lines.iter().find_map(|l| vhal::Reading::parse(l)).ok_or_else(|| format!("unexpected `car get` reply: {lines:?}"))
    }

    /// Every vehicle HAL property's current value. Automotive devices only.
    pub fn vehicle_list(&self) -> Result<Vec<vhal::Reading>, String> {
        self.require(InputMethod::HeadUnit, "the vehicle HAL")?;
        Ok(self.console(&["car list".into()])?.iter().filter_map(|l| vhal::Reading::parse(l)).collect())
    }
}

/// Whether a running instance has finished booting.
fn booted(r: &Running, adb: Option<&Path>) -> bool {
    let started = Instant::now();
    let console_up = loop {
        if console::send(r.console_port, &["ping".into()]).is_ok() {
            break true;
        }
        if started.elapsed() > CONSOLE_GRACE {
            break false;
        }
        sleep(Duration::from_millis(200));
    };
    if !console_up {
        return false;
    }
    match adb {
        Some(adb) => Command::new(adb)
            .args(["-s", &r.serial(), "shell", "getprop", "sys.boot_completed"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "1"),
        None => console::send(r.console_port, &["frames".into()])
            .ok()
            .and_then(|l| l.first().and_then(|n| n.trim().parse::<u64>().ok()))
            .is_some_and(|n| n > 0),
    }
}

/// The last `n` lines of a device's guest console (`<avd>/emu-rs/console.log`).
pub fn console_log_tail(avd_dir: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(avd_dir.join("emu-rs/console.log")) else { return Vec::new() };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| (*l).to_owned()).collect()
}

/// Decodes an 8-bit RGBA PNG (what the emulator's `screenshot` writes).
fn decode_png(bytes: &[u8]) -> Result<Frame, String> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!("unexpected screenshot format {:?} {:?}", info.color_type, info.bit_depth));
    }
    buf.truncate(info.buffer_size());
    Ok(Frame { width: info.width, height: info.height, rgba: buf })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_emulator, wait_for};

    struct Fixture {
        root: PathBuf,
        android: Android,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// An SDK with a mobile and a Wear OS image, and an empty AVD home.
    fn fixture(tag: &str) -> Fixture {
        // Under /tmp, not $TMPDIR: the fake display socket's path must fit a unix socket address
        // (104 bytes on macOS), and macOS's $TMPDIR alone takes about 50.
        let root = PathBuf::from("/tmp").join(format!("sa-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        for (dir, api, image_tag) in
            [("android-36/google_apis/arm64-v8a", 36, "google_apis"), ("android-34/android-wear/arm64-v8a", 34, "android-wear")]
        {
            let d = sdk.join("system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(
                d.join("source.properties"),
                format!("AndroidVersion.ApiLevel={api}\nPkg.Revision=7\nSystemImage.TagId={image_tag}\n"),
            )
            .expect("props");
        }
        let mut android = Android::new(Env { sdk_root: Some(sdk), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        Fixture { root, android }
    }

    #[test]
    fn runtimes_are_installed_system_images() {
        let f = fixture("runtimes");
        let rts = f.android.runtimes();
        let ids: Vec<&str> = rts.iter().map(|r| r.identifier.as_str()).collect();
        assert_eq!(ids, ["system-images;android-34;android-wear;arm64-v8a", "system-images;android-36;google_apis;arm64-v8a"]);
        assert_eq!((rts[1].name.as_str(), rts[1].version.as_str(), rts[1].platform), ("Android 36 (google_apis)", "36", Platform::Android));
    }

    #[test]
    fn created_avds_list_as_labelled_android_devices() {
        let f = fixture("create");
        let phone = f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        assert_eq!(
            (phone.platform, phone.form_factor, phone.label()),
            (Platform::Android, Some(FormFactor::Phone), "Android · Phone".to_owned())
        );
        assert_eq!(phone.runtime_id, "system-images;android-36;google_apis;arm64-v8a");
        let fold = f.android.create("fold", "system-images/android-36/google_apis/arm64-v8a", Some(FormFactor::Foldable)).expect("fold");
        assert_eq!(fold.form_factor, Some(FormFactor::Foldable));
        let watch = f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        assert_eq!(watch.label(), "Wear OS · Watch");
        let devices = f.android.devices();
        assert_eq!(devices.iter().map(|d| d.udid.as_str()).collect::<Vec<_>>(), ["fold", "phone", "watch"]);
        assert!(devices.iter().all(|d| d.state == DeviceState::Shutdown && d.available));
        let d = f.android.device("watch").expect("watch");
        assert_eq!(d.profile().expect("profile").shape, sim_core::form_factor::Shape::Round);
        assert_eq!(d.screen_size(), Ok((454, 454)));
        assert!(f.android.device("nope").expect_err("missing").contains("no device matches `nope`"));
        assert!(f.android.device("booted").is_err(), "nothing is running");
    }

    #[test]
    fn create_rejects_form_factors_the_image_cant_boot() {
        let f = fixture("reject");
        let wear = "system-images;android-34;android-wear;arm64-v8a";
        let err = f.android.create("t", wear, Some(FormFactor::Tablet)).expect_err("tablet from a Wear image");
        assert_eq!(err, format!("system image `{wear}` (tag `android-wear`) boots Wear OS · Watch devices, not tablet"));
        let err = f.android.create("v", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Vision)).expect_err("vision");
        assert!(err.ends_with("(tag `google_apis`) boots Android · Phone devices, not vision"), "{err}");
        assert!(f.android.create("x", "android-36", None).expect_err("bad image").contains("should look like"));
        assert!(f.android.devices().is_empty(), "nothing was written");
    }

    #[test]
    fn stopped_devices_refuse_running_only_operations() {
        let f = fixture("stopped");
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let d = f.android.device("watch").expect("watch");
        assert_eq!(d.rotate(), Err("watch isn't running; boot it first".into()));
        assert_eq!(d.touch(1.0, 1.0, Phase::Down), Err("watch isn't running; boot it first".into()), "not a socket error");
        assert_eq!(d.rotary(1), Err("watch isn't running; boot it first".into()));
        assert!(d.fold(90.0).expect_err("not a foldable").starts_with("folding needs a device with Hinge; watch is Wear OS · Watch"));
        assert!(d.button(Button::Siri, Direction::Down).expect_err("no Siri").contains("doesn't exist on Android"));
        f.android.delete(&d).expect("delete");
        assert!(f.android.devices().is_empty());
    }

    #[test]
    fn running_devices_are_driven_over_console_and_display() {
        let f = fixture("running");
        f.android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("fold");
        let fake = fake_emulator(&f.android, "fold");
        let d = f.android.device("booted").expect("the running device is `booted`");
        assert_eq!((d.info.udid.as_str(), d.info.state), ("fold", DeviceState::Booted));
        let r = d.running.clone().expect("running");
        assert!(booted(&r, None), "console answers and the display has frames");

        d.type_text("Hi").expect("type");
        d.button(Button::Home, Direction::Down).expect("home down");
        d.button(Button::Back, Direction::Up).expect("back up");
        d.key(0x28, Direction::Down).expect("return");
        d.set_location(37.5, -122.25).expect("geo");
        d.rotate().expect("rotate");
        d.fold(90.0).expect("fold");
        assert_eq!(
            wait_for(&fake.console, 10)[2..],
            [
                "event send EV_KEY:42:1 EV_KEY:35:1 EV_KEY:35:0 EV_KEY:42:0",
                "event send EV_KEY:23:1 EV_KEY:23:0",
                "event send EV_KEY:KEY_HOME:1",
                "event send EV_KEY:KEY_BACK:0",
                "event send EV_KEY:28:1",
                "geo fix -122.25 37.5",
                "rotate",
                "sensor set hinge-angle0 90",
            ]
        );
        let shot = d.screenshot().expect("screenshot");
        assert_eq!((shot.width, shot.height, shot.rgba[0]), (2, 1, 9));
        let (w, h, png) = d.screenshot_png().expect("png");
        assert_eq!((w, h, sim_core::frame::png_size(&png)), (2, 1, Some((2, 1))));
        let to = f.root.join("shot.png");
        assert_eq!(d.screenshot_png_to(&to), Ok((2, 1)));
        assert!(std::fs::read(&to).expect("written").starts_with(b"\x89PNG"), "the emulator wrote it there");

        d.play(&sim_core::gesture::tap(sim_core::geometry::Point::new(100.0, 200.0), Duration::ZERO)).expect("tap");
        assert_eq!(wait_for(&fake.display, 2), ["touch 100 200 down", "touch 100 200 up"]);
        assert!(d.rotary(1).expect_err("not a watch").starts_with("rotary input needs a device with Rotary input"));
        assert!(f.android.delete(&d).expect_err("running").contains("is running"));
    }

    /// Adds an installed system image under the fixture's SDK.
    fn add_image(f: &Fixture, dir: &str, image_tag: &str) {
        let d = f.root.join("sdk/system-images").join(dir);
        std::fs::create_dir_all(&d).expect("image dir");
        std::fs::write(d.join("system.img"), b"s").expect("system.img");
        std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=34\nSystemImage.TagId={image_tag}\n")).expect("props");
    }

    #[test]
    fn only_devices_with_a_touchpad_take_touchpad_input() {
        let f = fixture("pad");
        add_image(&f, "android-36/ai-glasses/arm64-v8a", "ai-glasses");
        f.android.create("glasses", "system-images;android-36;ai-glasses;arm64-v8a", None).expect("glasses");
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let watch = f.android.device("watch").expect("watch");
        assert!(watch.touchpad(1, 1, "tap").expect_err("no touchpad").starts_with("watch has no touchpad"));
        let glasses = f.android.device("glasses").expect("glasses");
        assert_eq!(glasses.touchpad_swipe((0, 0), (1542, 10), Duration::ZERO), Err("the touchpad is 1542x297".into()));
        assert_eq!(glasses.touchpad(1, 1, "tap"), Err("glasses isn't running; boot it first".into()));
    }

    #[test]
    fn tvs_have_no_touch_screen() {
        let f = fixture("tv");
        add_image(&f, "android-34/android-tv/arm64-v8a", "android-tv");
        f.android.create("tv", "system-images;android-34;android-tv;arm64-v8a", None).expect("tv");
        let tv = f.android.device("tv").expect("tv");
        assert_eq!(tv.avd.as_ref().and_then(|a| a.get("hw.screen")), Some("no-touch"));
        let err = tv.touch(10.0, 10.0, Phase::Down).expect_err("no touch screen");
        assert!(err.starts_with("tv has no touch screen (hw.screen=no-touch)"), "{err}");
        assert!(tv.play(&[]).expect_err("no swipes either").contains("no touch screen"));
    }

    #[test]
    fn android_xr_images_make_headsets_and_glasses() {
        let f = fixture("xr");
        add_image(&f, "android-34/google-xr/arm64-v8a", "android-xr");
        add_image(&f, "android-36/ai-glasses/arm64-v8a", "ai-glasses");
        let headset = f.android.create("headset", "system-images;android-34;google-xr;arm64-v8a", None).expect("headset");
        assert_eq!((headset.form_factor, headset.label()), (Some(FormFactor::Vision), "Android XR · Vision".to_owned()));
        let d = f.android.device("headset").expect("headset");
        assert_eq!(d.screen_size(), Ok((2560, 2558)));
        assert_eq!(d.avd.as_ref().and_then(|a| a.get("hw.initialOrientation")), Some("landscape"));
        let glasses =
            f.android.create("glasses", "system-images;android-36;ai-glasses;arm64-v8a", Some(FormFactor::Vision)).expect("glasses");
        assert_eq!(glasses.sub_platform.map(|s| s.as_str()), Some("androidxr"));
        assert_eq!(f.android.device("glasses").expect("glasses").screen_size(), Ok((450, 450)));
        let err = f.android.create("p", "system-images;android-34;google-xr;arm64-v8a", Some(FormFactor::Phone)).expect_err("phone");
        assert!(err.ends_with("boots Android XR · Vision devices, not phone"), "{err}");

        // XR controls are for Android XR devices, and go to a running one's console.
        let rotate = XrCommand::Rotate { x: 0.0, y: 0.5, z: 0.0 };
        assert_eq!(d.xr(&rotate), Err("headset isn't running; boot it first".into()));
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let watch = f.android.device("watch").expect("watch");
        assert_eq!(watch.xr(&rotate), Err("XR controls need an Android XR device; watch is Wear OS · Watch".into()));
        let fake = fake_emulator(&f.android, "headset");
        let d = f.android.device("headset").expect("running headset");
        d.xr(&rotate).expect("rotate");
        d.xr(&XrCommand::Recenter).expect("recenter");
        assert_eq!(wait_for(&fake.console, 2), ["xr rotate 0 0.5 0", "xr recenter"]);
    }

    #[test]
    fn automotive_devices_drive_the_vehicle_hal() {
        let f = fixture("car");
        let image = f.android.env.sdk_root.clone().expect("sdk").join("system-images/android-34/android-automotive/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=34\nSystemImage.TagId=android-automotive\n")
            .expect("props");
        let car = f.android.create("car", "system-images;android-34;android-automotive;arm64-v8a", None).expect("car");
        assert_eq!(car.form_factor, Some(FormFactor::Auto));
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let watch = f.android.device("watch").expect("watch");
        assert!(
            watch
                .vehicle_set("gear", 0, "drive")
                .expect_err("not a car")
                .starts_with("the vehicle HAL needs a device with Automotive head unit; watch is Wear OS")
        );

        let d = f.android.device("car").expect("car");
        assert!(d.vehicle_set("warp_drive", 0, "1").expect_err("unknown").contains("unknown vehicle property"));
        assert!(d.vehicle_set("gear", 0, "fast").expect_err("bad value").contains("isn't a Int32 value"));
        assert_eq!(d.vehicle_set("gear", 0, "drive"), Err("car isn't running; boot it first".into()));

        let fake = fake_emulator(&f.android, "car");
        let d = f.android.device("car").expect("car");
        d.vehicle_set("gear", 0, "drive").expect("set");
        d.vehicle_set("HVAC_TEMPERATURE_SET", 49, "21.5").expect("set seat");
        let gear = d.vehicle_get("GEAR_SELECTION", 0).expect("get");
        assert_eq!((gear.property.as_str(), gear.id, gear.value.as_str()), ("GEAR_SELECTION", 0x1140_0400, "8"));
        let all = d.vehicle_list().expect("list");
        assert_eq!(all.iter().map(|r| r.value.as_str()).collect::<Vec<_>>(), ["8", "Toy Car"]);
        assert_eq!(
            wait_for(&fake.console, 4),
            ["car set 0x11400400 0 drive", "car set 0x15600503 49 21.5", "car get 0x11400400 0", "car list"]
        );
    }

    #[test]
    fn decodes_screenshots() {
        let frame = Frame { width: 1, height: 2, rgba: vec![1, 2, 3, 255, 4, 5, 6, 255] };
        assert_eq!(decode_png(&sim_core::frame::encode_png(&frame).expect("png")), Ok(frame));
        assert!(decode_png(b"nope").is_err());
    }
}
