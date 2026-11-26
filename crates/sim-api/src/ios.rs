//! The iOS backend: every device method, served through the host's simulator service.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use sim_core::debug;
use sim_core::device::{DeviceState, privacy_service};
use sim_core::form_factor::FormFactor;
use sim_core::geometry::Size;
use sim_core::gesture::Sample;
use sim_core::hid::{KEY_LEFT_SHIFT, keystroke};
use sim_core::reuse::{BootKey, Reuse};
use sim_core::worker::Workers;
use sim_coresim::{Appearance, Context, Device, Direction, Display, Hid, Orientation, StateWatch, StatusBar, WatchGesture};

use crate::params::{
    button, crown_delta, err, need_bool, need_f64, need_str, on_carplay, opt_bool, opt_form_factor, opt_orientation, opt_u64, pairs,
    remote_swipe_samples, screenshot_result, str_param, strings, swipe_samples, tap_samples, touch_sample,
};

/// Shaping proxies, one per device, owned by this (long-running) process.
static PROXIES: std::sync::Mutex<Option<std::collections::HashMap<String, sim_netshape::proxy::Proxy>>> = std::sync::Mutex::new(None);

fn with_proxies<T>(f: impl FnOnce(&mut std::collections::HashMap<String, sim_netshape::proxy::Proxy>) -> T) -> T {
    let mut guard = PROXIES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Default::default))
}

fn profile_from(p: &Value, base: sim_netshape::Profile) -> Result<sim_netshape::Profile, String> {
    let mut profile = match str_param(p, "preset") {
        Some(name) => sim_netshape::Profile::preset(name).ok_or_else(|| format!("unknown preset `{name}`"))?,
        None => base,
    };
    let num = |key: &str| p.get(key).and_then(Value::as_u64);
    if let Some(v) = p.get("offline").and_then(Value::as_bool) {
        profile.offline = v;
    }
    if let Some(v) = num("latency_ms") {
        profile.latency_ms = v as u32;
    }
    if let Some(v) = num("down_kbps") {
        profile.down_kbps = v as u32;
    }
    if let Some(v) = num("up_kbps") {
        profile.up_kbps = v as u32;
    }
    if let Some(v) = num("loss_pct") {
        profile.loss_pct = (v as u32).min(100);
    }
    if let Some(v) = num("port_base") {
        profile.port_base = u16::try_from(v).map_err(err)?;
    }
    if let Some(v) = num("port_count") {
        profile.port_count = u16::try_from(v).map_err(err)?;
    }
    if let Some(rules) = p.get("rules") {
        profile.rules = serde_json::from_value(rules.clone()).map_err(|e| format!("bad rules: {e}"))?;
    }
    Ok(profile)
}

/// A local port nothing listens on.
fn closed_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(err)?;
    l.local_addr().map(|a| a.port()).map_err(err)
}

/// Runs the in-device proxy tool; succeeds only if it reports success.
fn device_proxy(device: &Device, args: &[&str]) -> Result<(), String> {
    let tool = sim_netshape::scproxy_tool()?;
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let out = device.exec(&tool.display().to_string(), &args, &[]).map_err(err)?;
    if out.stdout.contains(" ok ") || out.stdout.starts_with("set ok") || out.stdout.starts_with("clear ok") {
        Ok(())
    } else {
        Err(format!("device proxy {}: {}", args.join(" "), out.stdout.lines().next().unwrap_or(out.stderr.trim())))
    }
}

/// Whether the device was booted with the current shaper build in its environment: its
/// SpringBoard maps that exact dylib. An older build counts as not injected (so it gets rebooted).
fn boot_injected(udid: &str) -> bool {
    let Ok(dylib) = sim_netshape::dylib() else { return false };
    let Ok(procs) = sim_debug::process::collect(udid) else { return false };
    let Some(sb) = procs.iter().find(|p| p.name == "SpringBoard") else { return false };
    std::process::Command::new("/usr/sbin/lsof")
        .args(["-p", &sb.pid.to_string(), "-Fn"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&*dylib.display().to_string()))
}

/// Reboots the device with the shaper in its launchd environment, so every process (including
/// WebKit's networking extension and system daemons) is shaped by the device's profile.
fn boot_inject(device: &Device) -> Result<(), String> {
    let udid = device.info().udid;
    if boot_injected(&udid) {
        return Ok(());
    }
    let env = sim_netshape::launch_env(&udid, &sim_netshape::dylib()?);
    if device.state() == DeviceState::Booted {
        device.shutdown().map_err(err)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while device.state() != DeviceState::Shutdown && std::time::Instant::now() < deadline {
            sleep(Duration::from_millis(200));
        }
    }
    device.boot_with_env(&env).map_err(err)?;
    boot(device, Duration::from_secs(600))
}

/// Sets or removes device-wide socket-shaper injection for processes started from now on.
fn device_inject(device: &Device, on: bool) -> Result<(), String> {
    let udid = device.info().udid;
    let args: Vec<String> = if on {
        let dylib = sim_netshape::dylib()?;
        let mut a = vec!["setenv".to_owned()];
        for (k, v) in sim_netshape::launch_env(&udid, &dylib) {
            a.extend([k, v]);
        }
        a
    } else {
        vec!["unsetenv".into(), "DYLD_INSERT_LIBRARIES".into()]
    };
    device.exec("launchctl", &args, &[]).map_err(err)?;
    if !on {
        device.exec("launchctl", &["unsetenv".to_owned(), "SIMEE_NETSHAPE".to_owned()], &[]).map_err(err)?;
    }
    Ok(())
}

/// Running audio taps, keyed by capture id (`mute-<udid>` for mutes).
static CAPTURES: std::sync::Mutex<Option<std::collections::HashMap<String, sim_audio::Capture>>> = std::sync::Mutex::new(None);
static NEXT_CAPTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn with_captures<T>(f: impl FnOnce(&mut std::collections::HashMap<String, sim_audio::Capture>) -> T) -> T {
    let mut guard = CAPTURES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Default::default))
}

/// One simulator service connection, created on first use.
#[derive(Default)]
pub struct Session {
    ctx: Option<Context>,
}

impl Session {
    fn ctx(&mut self) -> Result<&Context, String> {
        if self.ctx.is_none() {
            self.ctx = Some(Context::new().map_err(|e| e.to_string())?);
        }
        self.ctx.as_ref().ok_or_else(|| "no service connection".to_owned())
    }

    fn device(&mut self, p: &Value) -> Result<Device, String> {
        let q = str_param(p, "device").unwrap_or("booted");
        self.ctx()?.device(q).map_err(|e| e.to_string())
    }

    /// The UDID the `device` parameter names: as given when it is one (no lookup), else resolved.
    fn udid(&mut self, p: &Value) -> Result<String, String> {
        let q = str_param(p, "device").unwrap_or("booted");
        if sim_core::device::is_udid(q) {
            return Ok(q.to_ascii_uppercase());
        }
        self.ctx()?.resolve(q).map(|(_, info)| info.udid).map_err(|e| e.to_string())
    }
}

/// The methods that send input, each run on its device's input worker.
const INPUT_METHODS: &[&str] = &["tap", "swipe", "touch", "rotary", "watch_gesture", "remote_swipe", "button", "type", "key"];

/// One input worker per device (by UDID), shared by every API connection. A worker keeps its
/// device's HID client and display between calls (the service's objects stay on the thread that
/// made them), runs that device's input in order (main-display `screen` and `screenshot` calls
/// too, so a capture follows the input sent before it), and lets go after five idle minutes.
static INPUT: Workers<String, Input> = Workers::new("simee-input", Duration::from_secs(300));

/// A device's input connection, owned by its worker thread: the device, a watch on its state, and
/// the HID client and main display, reused while the device stays in the same boot.
#[derive(Default)]
struct Input {
    ctx: Option<Context>,
    device: Option<Device>,
    /// State changes the watch has seen.
    changes: Arc<AtomicU64>,
    watch: Option<StateWatch>,
    hid: Reuse<BootKey, Hid>,
    display: Reuse<BootKey, Display>,
}

impl Input {
    /// Runs `send` with the device's HID client and, when `screen` is set, its screen size. Both
    /// are reused until the device's state changes (a reboot) and dropped, with the device, on any
    /// failure; a device whose boots can't be told apart connects on every call, as before.
    fn run<R>(&mut self, udid: &str, screen: bool, send: impl FnOnce(&Hid, Option<Size>) -> Result<R, String>) -> Result<R, String> {
        let result = self.try_run(udid, screen, send);
        if result.is_err() {
            self.device = None;
            self.watch = None;
        }
        result
    }

    /// Runs `with` on the device's main display, reused like the HID client (and dropped, with the
    /// device, on any failure).
    fn display<R>(&mut self, udid: &str, with: impl FnOnce(&Display) -> Result<R, String>) -> Result<R, String> {
        let result = self.try_display(udid, with);
        if result.is_err() {
            self.device = None;
            self.watch = None;
        }
        result
    }

    fn try_display<R>(&mut self, udid: &str, with: impl FnOnce(&Display) -> Result<R, String>) -> Result<R, String> {
        let Self { ctx, device, changes, watch, display, .. } = self;
        let (device, key) = connect(ctx, device, changes, watch, udid)?;
        display.with(key, || device.display().map_err(err), with)
    }

    fn try_run<R>(&mut self, udid: &str, screen: bool, send: impl FnOnce(&Hid, Option<Size>) -> Result<R, String>) -> Result<R, String> {
        let Self { ctx, device, changes, watch, hid, display } = self;
        let (device, key) = connect(ctx, device, changes, watch, udid)?;
        let size = if screen {
            let (w, h) = display.with(key, || device.display().map_err(err), |d| d.size().map_err(err))?;
            Some(Size::new(f64::from(w), f64::from(h)))
        } else {
            None
        };
        hid.with(key, || device.hid().map_err(err), |hid| send(hid, size))
    }
}

/// The worker's device (found and watched on first use) and the key its connections are reused
/// under.
fn connect<'a>(
    ctx: &'a mut Option<Context>,
    device: &'a mut Option<Device>,
    changes: &Arc<AtomicU64>,
    watch: &mut Option<StateWatch>,
    udid: &str,
) -> Result<(&'a Device, Option<BootKey>), String> {
    let ctx = match ctx {
        Some(ctx) => ctx,
        None => ctx.insert(Context::new().map_err(err)?),
    };
    let device = match device {
        Some(device) => device,
        None => {
            let found = ctx.device(udid).map_err(err)?;
            let seen = changes.clone();
            *watch = found
                .watch_state(move || {
                    seen.fetch_add(1, Ordering::Release);
                })
                .ok();
            device.insert(found)
        }
    };
    let changes = watch.as_ref().map(|_| changes.load(Ordering::Acquire));
    let key = BootKey::of(device.state() == DeviceState::Booted, changes, device.last_booted_at());
    Ok((device, key))
}

/// Runs `with` on the device's main display on its input worker, which keeps the display between
/// calls (and orders a capture after the input sent before it).
fn on_main_display<R: Send + 'static>(
    s: &mut Session,
    p: &Value,
    with: impl FnOnce(&Display) -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let udid = s.udid(p)?;
    let result = INPUT.run(&udid.clone(), move |input| sim_coresim::autoreleasepool(|| input.display(&udid, with)))?;
    result.map_err(|e| as_typed(e, p))
}

/// An error about the device the `device` parameter names, with the UDID as the caller typed it
/// (workers look devices up by the uppercased UDID).
fn as_typed(error: String, p: &Value) -> String {
    match str_param(p, "device") {
        Some(q) if sim_core::device::is_udid(q) && q != q.to_ascii_uppercase() => {
            error.replace(&format!("`{}`", q.to_ascii_uppercase()), &format!("`{q}`"))
        }
        _ => error,
    }
}

/// The `display` a method names: its size, or a capture. The main display goes through the
/// device's worker; the CarPlay screen is looked up per call (attaching it isn't a new boot).
fn screen_size(s: &mut Session, p: &Value) -> Result<(u32, u32), String> {
    if on_carplay(p)? {
        return display_of(&s.device(p)?, p)?.size().map_err(err);
    }
    on_main_display(s, p, |d| d.size().map_err(err))
}

fn capture(s: &mut Session, p: &Value) -> Result<sim_core::frame::Frame, String> {
    if on_carplay(p)? {
        return display_of(&s.device(p)?, p)?.capture().map_err(err);
    }
    on_main_display(s, p, |d| d.capture().map_err(err))
}

/// The simulators to resolve a `device` across platforms with: what selection reads of each.
pub fn selection_infos(s: &mut Session) -> Result<Vec<sim_core::device::DeviceInfo>, String> {
    s.ctx()?.selection_infos().map_err(err)
}

/// Runs an input method on its device's worker.
fn input(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let udid = s.udid(p)?;
    let (method, params) = (method.to_owned(), p.clone());
    let result = INPUT.run(&udid.clone(), move |input| sim_coresim::autoreleasepool(|| input_method(input, &udid, &method, &params)))?;
    result.map_err(|e| as_typed(e, p))
}

fn input_method(input: &mut Input, udid: &str, method: &str, p: &Value) -> Result<Value, String> {
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "tap" => play_on(input, udid, p, &tap_samples(p)?),
        "swipe" => play_on(input, udid, p, &swipe_samples(p)?),
        "touch" => {
            let (at, phase) = touch_sample(p)?;
            play_on(input, udid, p, &[Sample { point: at, phase, delay: Duration::ZERO }])
        }
        "rotary" => {
            let delta = crown_delta(p)?;
            input.run(udid, false, |hid, _| hid.crown(delta).map_err(err)).map(|()| json!({"delta": delta}))
        }
        "watch_gesture" => {
            let name = need_str(p, "gesture")?;
            let gesture = WatchGesture::parse(name).ok_or_else(|| format!("unknown gesture `{name}` (double-tap, wrist-flick)"))?;
            null(input.run(udid, false, |hid, _| hid.watch_gesture(gesture).map_err(err)))
        }
        "remote_swipe" => {
            let samples = remote_swipe_samples(p)?;
            null(input.run(udid, false, |hid, _| {
                for sample in samples {
                    sleep(sample.delay);
                    hid.remote_touch(sample.point, sample.phase).map_err(err)?;
                }
                Ok(())
            }))
        }
        "button" => {
            let (button, hold) = button(p)?;
            null(input.run(udid, false, |hid, _| {
                hid.button(button, Direction::Down).map_err(err)?;
                sleep(hold);
                hid.button(button, Direction::Up).map_err(err)
            }))
        }
        "type" => {
            let text = need_str(p, "text")?;
            null(input.run(udid, false, |hid, _| type_text(hid, text)))
        }
        "key" => {
            let usage = opt_u64(p, "usage").ok_or("missing number parameter `usage`")? as u32;
            null(input.run(udid, false, |hid, _| press(hid, usage)))
        }
        other => Err(format!("`{other}` isn't an input method")),
    }
}

/// The display a method's `display` parameter picks: the device's own, or its CarPlay screen.
fn display_of(device: &Device, p: &Value) -> Result<sim_coresim::Display, String> {
    if on_carplay(p)? { device.external_display() } else { device.display() }.map_err(err)
}

/// The CarPlay screen's size: the car display simee attaches.
fn carplay_screen() -> Size {
    Size::new(f64::from(sim_core::carplay::WIDTH), f64::from(sim_core::carplay::HEIGHT))
}

fn play(hid: &Hid, screen: Size, samples: &[Sample]) -> Result<(), String> {
    for s in samples {
        sleep(s.delay);
        hid.touch(screen.normalize(s.point), s.phase).map_err(err)?;
    }
    Ok(())
}

/// Plays touch samples on the device's screen, or its CarPlay screen (`display: "carplay"`).
fn play_on(input: &mut Input, udid: &str, p: &Value, samples: &[Sample]) -> Result<Value, String> {
    let carplay = on_carplay(p)?;
    let played = input.run(udid, !carplay, |hid, screen| match screen {
        Some(screen) => play(hid, screen, samples),
        None => {
            for s in samples {
                sleep(s.delay);
                hid.carplay_touch(carplay_screen().normalize(s.point), s.phase).map_err(err)?;
            }
            Ok(())
        }
    });
    played.map(|()| Value::Null)
}

/// The orientation simee last set per device (UDID), so `rotate` without one turns 90° from there.
static ORIENTATIONS: std::sync::Mutex<Option<std::collections::HashMap<String, Orientation>>> = std::sync::Mutex::new(None);

fn rotate(device: &Device, wanted: Option<Orientation>) -> Result<Value, String> {
    let udid = device.info().udid;
    let mut guard = ORIENTATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let known = guard.get_or_insert_with(Default::default);
    let next = wanted.unwrap_or_else(|| known.get(&udid).copied().unwrap_or(Orientation::Portrait).clockwise());
    device.set_orientation(next).map_err(err)?;
    known.insert(udid, next);
    Ok(json!({"orientation": next.as_str()}))
}

/// Presses and releases one key.
fn press(hid: &Hid, usage: u32) -> Result<(), String> {
    hid.key(usage, Direction::Down).map_err(err)?;
    sleep(Duration::from_millis(15));
    hid.key(usage, Direction::Up).map_err(err)?;
    sleep(Duration::from_millis(15));
    Ok(())
}

fn type_text(hid: &Hid, text: &str) -> Result<(), String> {
    for c in text.chars() {
        let stroke = keystroke(c).ok_or_else(|| format!("no key for {c:?} on the US layout"))?;
        if stroke.shift {
            hid.key(KEY_LEFT_SHIFT, Direction::Down).map_err(err)?;
            sleep(Duration::from_millis(10));
        }
        press(hid, stroke.usage)?;
        if stroke.shift {
            hid.key(KEY_LEFT_SHIFT, Direction::Up).map_err(err)?;
            sleep(Duration::from_millis(10));
        }
    }
    Ok(())
}

/// Boots (if needed) and waits until the device reports booted and its display has a frame.
fn boot(device: &Device, timeout: Duration) -> Result<(), String> {
    if device.state() != DeviceState::Booted {
        device.boot().map_err(err)?;
    }
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let finished = device.state() == DeviceState::Booted && device.boot_finished().unwrap_or(true);
        if finished && device.display().is_ok_and(|d| d.surface().is_some()) {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    Err(format!("device did not finish booting within {}s", timeout.as_secs()))
}

/// What a boot waits for and changes beyond the service's own "booted" (`boot` and `boot_many`).
#[derive(Clone, Debug)]
struct BootPlan {
    /// For the boot itself (until the service reports it booted and the display has a frame).
    timeout: Duration,
    env: Vec<(String, String)>,
    /// The network shaper to inject (its environment names the device), resolved once by the
    /// caller: `boot_many`'s workers must not each build it into the same cache.
    shaper: Option<std::path::PathBuf>,
    /// Wait until the device is quiet too (see [`crate::settle`]), for at most `settle_timeout`
    /// after the boot.
    settle: Option<crate::settle::Rule>,
    settle_timeout: Duration,
    /// Daemons to turn off once booted (a named profile).
    profile: Option<&'static Profile>,
}

/// A named set of in-device launchd jobs a farm device runs without.
#[derive(Debug)]
pub struct Profile {
    pub name: &'static str,
    /// launchd labels (in the device's `system` domain) disabled and stopped.
    pub disabled: &'static [&'static str],
}

/// Profiles `boot {profile}` applies. Each job is disabled in the device's own launchd database
/// and booted out (stopped now). `boot` of a shut-down device writes the disables into that
/// database before the boot ([`preset_profile`]), so the jobs never start. A plain reboot keeps
/// the disables and a clone of the device keeps none (a new UDID, a new database): apply the
/// profile on every boot. Again it touches only the jobs that came back, and `profile {undo}`
/// enables the jobs.
///
/// The paired-watch daemons (the runtime's `NanoLaunchDaemons`: companionappd, NPKCompanionAgent,
/// brookcompaniond, addressbooksyncd, bulletindistributord, pairedsyncd and 11 more) are
/// `Disabled` in their own plists; nanoregistrylaunchd enables all 17 each time it runs (one to
/// three minutes after a boot, measured on iOS 27.2), which is why they came back on every boot.
/// The profile disables nanoregistrylaunchd itself, and the ones that run once enabled.
pub const PROFILES: &[Profile] = &[Profile { name: "slim", disabled: SLIM }];

/// The `slim` profile: daemons that burn CPU or memory on an idle booted device and that no
/// AutoMobile tool needs (measured on iOS 27.2, iPhone 17; see docs/farm.md).
///
/// Kept on purpose: SpringBoard, backboardd, InputUI (the keyboard), AccessibilityUIServer and
/// the accessibility daemons (AutoMobile reads the screen through accessibility), installd,
/// lsd and runningboardd (install, launch), tccd (privacy), usernotificationsd, locationd
/// (`location`), sharingd, webbookmarksd (Safari, `open_url`), assetsd (the photo library),
/// syslogd/logd (`logs`), configd_sim and nsurlsessiond (networking), and what simee's own
/// methods reach in the device: carkitd (`carplay`), biometrickitd (`biometric`), apsd (`push`),
/// securityd (`reset_keychain`), cfprefsd (`defaults_*`, `keyboard_language`), notifyd (`notify`,
/// `memory_warning`), mediaserverd (audio).
const SLIM: &[&str] = &[
    // Lock and home screen wallpapers: PosterBoard and the ~15 poster extensions it keeps alive.
    "com.apple.PosterBoard",
    // Widgets.
    "com.apple.chronod",
    // Siri, dictation and suggestions.
    "com.apple.assistantd",
    "com.apple.assistant_cdmd",
    "com.apple.siriactionsd",
    "com.apple.siriinferenced",
    "com.apple.siriknowledged",
    "com.apple.sirittsd",
    "com.apple.siri.context.service",
    "com.apple.corespeechd",
    "com.apple.voicebankingd",
    "com.apple.suggestd",
    "com.apple.textunderstandingd",
    "com.apple.generativeexperiencesd",
    "com.apple.spotlightknowledged.updater",
    "com.apple.parsecd",
    // Photo and media analysis.
    "com.apple.photoanalysisd",
    "com.apple.mediaanalysisd",
    // Health, home, fitness. Not sleepd: without it SpringBoard and mobiletimerd retry
    // `com.apple.sleepd.sleepserver` in a tight loop on every later boot (~350 % of a core, and
    // the device never settles; measured on iOS 27.2).
    "com.apple.healthd",
    "com.apple.homed",
    "com.apple.homeeventsd",
    "com.apple.fitnesscoachingd",
    "com.apple.fitnessintelligenced",
    // Store, iCloud sync, mail, news, weather, wallet, games, finance.
    "com.apple.appstored",
    "com.apple.itunesstored",
    "com.apple.itunescloudd",
    "com.apple.amsengagementd",
    "com.apple.ap.adprivacyd",
    "com.apple.ap.promotedcontentd",
    "com.apple.cloudd",
    "com.apple.email.maild",
    "com.apple.icloudmailagent",
    "com.apple.newsd",
    "com.apple.weatherd",
    "com.apple.passd",
    "com.apple.financed",
    "com.apple.gamed",
    "com.apple.SafariBookmarksSyncAgent",
    "com.apple.followupd",
    "com.apple.remindd",
    // Location history, maps analytics, people and social. Not carkitd: `carplay` needs it.
    "com.apple.routined",
    "com.apple.geoanalyticsd",
    "com.apple.navd",
    "com.apple.peopled",
    "com.apple.sociallayerd",
    // Screen Time.
    "com.apple.ScreenTimeAgent",
    "com.apple.UsageTrackingAgent",
    "com.apple.FamilyControlsAgent",
    // Paired-watch and Find My companions: nanoregistrylaunchd enables the paired-watch daemons
    // (see `PROFILES`); the rest are those that then run on an idle phone.
    "com.apple.nanoregistrylaunchd",
    "com.apple.addressbooksyncd",
    "com.apple.bulletindistributord",
    "com.apple.pairedsyncd",
    "com.apple.NPKCompanionAgent",
    "com.apple.companionappd",
    "com.apple.wcd",
    "com.apple.brook.brookcompaniond",
    "com.apple.findmy.findmylocated",
    // Experiments, ML hosting and asset prefetch.
    "com.apple.triald",
    "com.apple.mlhostd",
    "com.apple.jetpackassetd",
    "com.apple.replicatord",
    "com.apple.diagnosticextensionsd",
];

fn profile(name: &str) -> Result<&'static Profile, String> {
    PROFILES.iter().find(|p| p.name == name).ok_or_else(|| {
        let names: Vec<&str> = PROFILES.iter().map(|p| p.name).collect();
        format!("unknown profile `{name}` ({})", names.join(", "))
    })
}

/// The settle rule from `prefix` + `cpu_pct`, `window_s` and `foreground` (the defaults where
/// absent): `settle_` for `boot` and `boot_many`, none for `settle`.
fn settle_rule(p: &Value, prefix: &str) -> Result<crate::settle::Rule, String> {
    let mut rule = crate::settle::Rule::default();
    let key = |name: &str| format!("{prefix}{name}");
    match p.get(key("cpu_pct")) {
        None | Some(Value::Null) => {}
        Some(v) => {
            rule.cpu_pct =
                v.as_f64().filter(|x| *x >= 0.0).ok_or_else(|| format!("{} must be a number of % of a core, not {v}", key("cpu_pct")))?
        }
    }
    match p.get(key("window_s")) {
        None | Some(Value::Null) => {}
        Some(v) => {
            rule.window = Duration::from_secs(v.as_u64().ok_or_else(|| format!("{} must be whole seconds, not {v}", key("window_s")))?)
        }
    }
    if let Some(fg) = opt_bool(p, &key("foreground"))? {
        rule.foreground = fg;
    }
    Ok(rule)
}

/// The plan for `p`, booting `concurrent` devices at once (1 for `boot`); `settle_default` when
/// `settle` isn't given. `shaper` resolves the network shaper (built on first use), called at
/// most once, here on the caller's thread.
fn boot_plan(
    p: &Value,
    concurrent: usize,
    settle_default: bool,
    shaper: impl FnOnce() -> Result<std::path::PathBuf, String>,
) -> Result<BootPlan, String> {
    let settle = match p.get("settle") {
        None | Some(Value::Null) => settle_default,
        Some(Value::Bool(b)) => *b,
        Some(other) => return Err(format!("settle must be true or false, not {other}")),
    };
    let shaped = p.get("shaped").and_then(Value::as_bool).unwrap_or(false);
    Ok(BootPlan {
        timeout: Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300)),
        env: pairs(p, "env"),
        shaper: if shaped { Some(shaper()?) } else { None },
        settle: if settle { Some(settle_rule(p, "settle_")?) } else { None },
        settle_timeout: opt_u64(p, "settle_timeout_s")
            .map_or_else(|| crate::settle::default_settle_timeout(concurrent), Duration::from_secs),
        profile: str_param(p, "profile").map(profile).transpose()?,
    })
}

/// Boots `device` by `plan`: the boot, then the profile, then the settle wait, each timed. Only
/// a failed boot is an error: a profile or settle wait that fails after it is reported in the
/// timings (`profile.error`; `settled: false` with `settle_error`), the device left booted.
fn boot_device(device: &Device, plan: &BootPlan) -> Result<Value, String> {
    let start = std::time::Instant::now();
    let udid = device.info().udid;
    let mut env = plan.env.clone();
    if let Some(dylib) = &plan.shaper {
        env.extend(sim_netshape::launch_env(&udid, dylib));
    }
    let was_booted = device.state() == DeviceState::Booted;
    // Before the boot, so this boot never starts the profile's jobs (a clone starts with no
    // disables of its own).
    let preset = plan.profile.filter(|_| !was_booted).map(|profile| preset_profile(&udid, profile));
    if !was_booted && !env.is_empty() {
        device.boot_with_env(&env).map_err(err)?;
    }
    boot(device, plan.timeout)?;
    let boot_s = start.elapsed().as_secs_f64();
    let mut result = json!({"device": udid, "boot_s": round(boot_s), "was_booted": was_booted});
    if let Some(profile) = plan.profile {
        let t = std::time::Instant::now();
        let mut r = match apply_profile(device, profile, false) {
            Ok(applied) => applied.to_json(false),
            Err(e) => json!({"error": e}),
        };
        r["name"] = json!(profile.name);
        r["took_s"] = json!(round(t.elapsed().as_secs_f64()));
        match preset {
            Some(Ok(n)) => r["preset"] = json!(n),
            Some(Err(e)) => r["preset_error"] = json!(e),
            None => {}
        }
        result["profile"] = r;
    }
    if let Some(rule) = plan.settle {
        // From the start of the boot call, the profile included.
        let Settled { at, settler } = wait_settled(&udid, rule, plan.settle_timeout, !was_booted);
        if at.is_some() {
            result["settled"] = json!(true);
            result["settle_s"] = json!(round(start.elapsed().as_secs_f64()));
        } else {
            result["settled"] = json!(false);
            result["settle_error"] = json!(not_settled(&settler, rule, plan.settle_timeout));
            result["waited_s"] = json!(round(start.elapsed().as_secs_f64()));
        }
        put_load(&mut result, &settler);
    }
    Ok(result)
}

/// Why a wait that ran out of time didn't settle.
fn not_settled(settler: &crate::settle::Settler, rule: crate::settle::Rule, timeout: Duration) -> String {
    let what = if rule.foreground { " in the foreground" } else { "" };
    let pct = settler.load().map_or_else(|| "no process tree".to_owned(), |l| format!("{:.0} % of a core{what}", l.judged_pct(&rule)));
    format!("device did not settle within {}s ({pct} over the last {}s)", timeout.as_secs(), rule.window.as_secs())
}

/// A settle wait's last figures: processes, and CPU over the window (all of it, and the
/// background-QoS and foreground parts); and the CPU seconds the tree used over the whole wait
/// (from the boot when the wait followed one).
fn put_load(result: &mut Value, settler: &crate::settle::Settler) {
    let load = settler.load();
    let used = settler.used();
    result["cpu_s"] = json!(round(used.total_s));
    result["background_cpu_s"] = json!(round(used.background_s));
    result["procs"] = json!(settler.procs());
    result["cpu_pct"] = json!(load.map(|l| round(l.total_pct)));
    result["background_cpu_pct"] = json!(load.map(|l| round(l.background_pct)));
    result["foreground_cpu_pct"] = json!(load.map(|l| round(l.foreground_pct())));
}

fn round(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// What applying (or undoing) a profile did: the labels now in the wanted state, those that
/// aren't (a `launchctl` that failed, or a label the device's launchd doesn't report so), and
/// those that weren't loaded when it was applied (stopped already, or not in this runtime).
#[derive(Debug, Default, PartialEq)]
struct Applied {
    changed: usize,
    failed: Vec<String>,
    not_loaded: Vec<String>,
    /// Labels that were disabled and not loaded before, so left alone (counted in `changed`).
    already: usize,
}

impl Applied {
    fn to_json(&self, undo: bool) -> Value {
        let key = if undo { "enabled" } else { "disabled" };
        let mut v = json!({(key): self.changed, "failed": self.failed});
        if !undo {
            v["not_loaded"] = json!(self.not_loaded);
            v["already"] = json!(self.already);
        }
        v
    }
}

/// The labels `launchctl print-disabled` reports disabled (`"label" => disabled` or `=> true`).
fn disabled_labels(text: &str) -> std::collections::HashSet<&str> {
    text.lines()
        .filter_map(|l| {
            let (label, state) = l.split_once("=>")?;
            let state = state.trim();
            (state == "disabled" || state == "true").then(|| label.trim().trim_matches('"'))
        })
        .collect()
}

/// Whether launchctl's stderr reports an error. A running job's `disable`/`enable` warns
/// "Warning: Please switch to user/foreground/<label> service identifier" and still works.
fn launchctl_refused(stderr: &str) -> bool {
    stderr.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with("Warning:"))
}

/// What launchctl said while a profile was applied to `labels`: `loaded` is `launchctl list`
/// before (PID, status, label; empty when it couldn't run), `refused` the labels whose
/// `disable`/`enable` failed or printed an error, `disabled` is `launchctl print-disabled system`
/// after. With that listing each label is judged by it, else by `refused` alone.
/// `already` are the labels that were off before and left alone (see [`still_on`]).
fn profile_outcome(labels: &[&str], loaded: &str, already: &[&str], refused: &[&str], disabled: &str, undo: bool) -> Applied {
    let loaded = loaded_labels(loaded);
    let listed = disabled.contains("=>");
    let disabled = disabled_labels(disabled);
    let mut out = Applied { already: already.len(), ..Applied::default() };
    for l in labels {
        if !loaded.is_empty() && !loaded.contains(l) && !already.contains(l) {
            out.not_loaded.push((*l).to_owned());
        }
        if !refused.contains(l) && (!listed || disabled.contains(l) != undo) {
            out.changed += 1;
        } else {
            out.failed.push((*l).to_owned());
        }
    }
    out
}

/// The labels in a `launchctl list` (PID, status, label).
fn loaded_labels(text: &str) -> std::collections::HashSet<&str> {
    text.lines().filter_map(|l| l.split_whitespace().nth(2)).collect()
}

/// The labels of `labels` a profile still has to disable and stop: those `print-disabled`
/// (`disabled`) doesn't list disabled, and those `launchctl list` (`loaded`) shows loaded. The
/// rest are off already and are left alone, so applying a profile again on a later boot costs a
/// spawn or two per job that came back, not two per job. Without either listing, all of them.
fn still_on<'a>(labels: &[&'a str], loaded: &str, disabled: &str) -> Vec<&'a str> {
    let loaded_set = loaded_labels(loaded);
    if loaded_set.is_empty() || !disabled.contains("=>") {
        return labels.to_vec();
    }
    let disabled = disabled_labels(disabled);
    labels.iter().copied().filter(|l| !disabled.contains(l) || loaded_set.contains(l)).collect()
}

/// Disables and stops each of the profile's jobs, or with `undo` enables them again (they run
/// from the next boot: an enabled job isn't loaded until launchd bootstraps it). The disables
/// live in the device's launchd database, so a plain reboot keeps most of them; a few jobs come
/// back enabled on every boot (measured, docs/farm.md), so a farm applies the profile on every
/// boot, and a job that is still off costs nothing here.
fn apply_profile(device: &Device, profile: &Profile, undo: bool) -> Result<Applied, String> {
    // The runtime's launchctl (a bare `launchctl` would be the host's, see AGENTS.md). The runtime
    // has no shell, so each call is its own spawn (~0.1-0.4 s), and `exec` reports no exit
    // status: a failed call is one that printed an error, and the effect is read back from
    // launchd's own lists.
    let run = |args: &[&str]| device.exec("launchctl", &args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(), &[]);
    let loaded = if undo { String::new() } else { run(&["list"]).map(|o| o.stdout).unwrap_or_default() };
    let before = if undo { String::new() } else { run(&["print-disabled", "system"]).map(|o| o.stdout).unwrap_or_default() };
    let todo = if undo { profile.disabled.to_vec() } else { still_on(profile.disabled, &loaded, &before) };
    let already: Vec<&str> = profile.disabled.iter().copied().filter(|l| !todo.contains(l)).collect();
    let mut refused = Vec::new();
    for label in &todo {
        let target = format!("system/{label}");
        let changed = run(&[if undo { "enable" } else { "disable" }, &target]);
        if changed.as_ref().map_or(true, |o| launchctl_refused(&o.stderr)) {
            refused.push(*label);
        }
        if !undo {
            // Reports failure in a simulator even when it stops the job: not read.
            let _ = run(&["bootout", &target]);
        }
    }
    // Nothing changed (a device whose disables were preset before its boot): the listing read
    // before is the one after.
    let disabled = if todo.is_empty() { before } else { run(&["print-disabled", "system"]).map_err(err)?.stdout };
    Ok(profile_outcome(profile.disabled, &loaded, &already, &refused, &disabled, undo))
}

/// Writes `profile`'s disables into the shut-down device `udid`'s launchd overrides file (see
/// [`sim_core::launchd`]) before it boots, so that boot never loads the profile's jobs: a clone's
/// first boot then skips starting ~60 daemons and the disable and bootout spawns that would stop
/// them (measured, docs/farm.md). Keeps the file's other overrides; returns how many labels it
/// newly disabled. [`apply_profile`] still runs after the boot and does whatever this couldn't
/// (a runtime whose launchd keeps the file elsewhere).
fn preset_profile(udid: &str, profile: &Profile) -> Result<usize, String> {
    preset_disables(&sim_core::launchd::disabled_plist_path(udid), profile.disabled)
}

/// Disables `labels` in the launchd overrides file at `path` (made, with its directory, when
/// missing); returns how many weren't disabled there already.
fn preset_disables(path: &Path, labels: &[&str]) -> Result<usize, String> {
    use std::os::unix::fs::DirBuilderExt as _;
    let entries = match std::fs::read_to_string(path) {
        Ok(text) => sim_core::launchd::parse(&text).ok_or_else(|| format!("{} isn't a property list simee reads", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let off: std::collections::HashSet<&str> = sim_core::launchd::disabled(&entries).collect();
    let new = labels.iter().filter(|l| !off.contains(*l)).count();
    if new == 0 {
        return Ok(0);
    }
    let dir = path.parent().ok_or("no directory")?;
    // launchd_sim makes it 0700 when it doesn't exist.
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // The directory is under world-writable /private/var/tmp: a unique name, created exclusively.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!("disabled.plist.simee.{}.{nanos}", std::process::id()));
    write_new(&tmp, dir, sim_core::launchd::render(&entries, labels, true).as_bytes())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })?;
    Ok(new)
}

/// Writes `bytes` to a file that must not exist yet in `dir` (`create_new` is `O_CREAT|O_EXCL`,
/// which never follows a symlink planted at the name), and refuses a `dir` owned by another user:
/// the file just made is ours, so its owner is this process's uid (no libc needed).
fn write_new(path: &Path, dir: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| fail(&e))?;
    let checked = file.metadata().and_then(|m| Ok((m.uid(), std::fs::symlink_metadata(dir)?))).map_err(|e| fail(&e));
    let result = checked.and_then(|(uid, d)| {
        if d.file_type().is_symlink() || d.uid() != uid {
            Err(format!("{} isn't a directory owned by the current user; refusing to write the launchd overrides there", dir.display()))
        } else {
            file.write_all(bytes).map_err(|e| fail(&e))
        }
    });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// A device's processes read through libproc (see [`crate::settle::Procs`]).
struct LibProc;

impl crate::settle::Procs for LibProc {
    fn list(&mut self) -> Vec<crate::settle::Entry> {
        sim_coresim::procs::list()
            .iter()
            .map(|p| crate::settle::Entry { pid: p.pid, ppid: p.ppid, springboard: p.name() == "SpringBoard" })
            .collect()
    }

    fn cpu(&mut self, pid: i32) -> Option<crate::settle::Cpu> {
        sim_coresim::procs::usage(pid)
            .map(|u| crate::settle::Cpu { total_s: u.cpu_ns as f64 / 1e9, background_s: u.background_ns as f64 / 1e9 })
    }
}

/// How a settle wait ended: `at` seconds after it started, or `None` when it ran out of time;
/// the settler holds the last CPU figure and process count either way.
struct Settled {
    at: Option<f64>,
    settler: crate::settle::Settler,
}

/// Waits until the booted device `udid` is settled by `rule`, reading its process tree (its
/// `launchd_sim` and descendants, nothing else on the host) once a second. `since_boot`: the
/// device was just booted, so the CPU its tree used before the first sample counts too.
fn wait_settled(udid: &str, rule: crate::settle::Rule, timeout: Duration, since_boot: bool) -> Settled {
    let start = std::time::Instant::now();
    let mut settler = if since_boot { crate::settle::Settler::since_boot(rule) } else { crate::settle::Settler::new(rule) };
    // Found by a scan of the host's processes, then reused while it is still a launchd_sim.
    let mut root: Option<i32> = None;
    loop {
        if root.is_none_or(|r| !sim_coresim::procs::is_launchd_sim(r)) {
            root = sim_coresim::procs::launchd_sim(udid);
        }
        let t = start.elapsed().as_secs_f64();
        if let Some(tree) = root.and_then(|r| crate::settle::tree(&mut LibProc, r))
            && settler.push(t, tree)
        {
            return Settled { at: Some(t), settler };
        }
        if start.elapsed() >= timeout {
            return Settled { at: None, settler };
        }
        sleep(Duration::from_secs(1));
    }
}

/// Boots `udids` with at most `concurrency` boots (and settle waits) in flight; one result per
/// device, in order: the `boot_device` timings, or `{device, error}`.
fn boot_many(udids: &[String], concurrency: usize, plan: &BootPlan) -> Vec<Value> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(vec![Value::Null; udids.len()]);
    let start = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency.clamp(1, udids.len().max(1)) {
            scope.spawn(|| {
                // Each thread has its own service connection: the service's objects stay on the
                // thread that made them.
                let ctx = Context::new().map_err(err);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(udid) = udids.get(i) else { break };
                    let queued_s = start.elapsed().as_secs_f64();
                    let result = sim_coresim::autoreleasepool(|| {
                        let device = ctx.as_ref().map_err(Clone::clone)?.device(udid).map_err(err)?;
                        boot_device(&device, plan)
                    });
                    let mut value = result.unwrap_or_else(|e| json!({"device": udid, "error": e}));
                    value["started_s"] = json!(round(queued_s));
                    value["done_s"] = json!(round(start.elapsed().as_secs_f64()));
                    results.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[i] = value;
                }
            });
        }
    });
    results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pids(udid: &str) -> Result<Vec<i32>, String> {
    Ok(sim_debug::process::collect(udid)?.iter().map(|p| p.pid).collect())
}

/// Rejects a `create` whose device type isn't the requested form factor. CarPlay (auto) is valid
/// for iOS but isn't a device type of its own.
fn check_form_factor(ctx: &Context, device_type: &str, form_factor: FormFactor) -> Result<(), String> {
    if form_factor == FormFactor::Auto {
        return Err("CarPlay is a display attached to an iPhone simulator, not a device type: create an iPhone, boot it, then call \
                    `carplay` with on=true (iOS 26 runtimes)"
            .into());
    }
    let types = ctx.device_types().map_err(err)?;
    let actual = types.iter().find(|t| t.name == device_type || t.identifier == device_type).and_then(|t| t.form_factor);
    match actual {
        Some(actual) if actual != form_factor => {
            Err(format!("device type `{device_type}` is a {}, not a {}", actual.as_str(), form_factor.as_str()))
        }
        _ => Ok(()),
    }
}

/// Dispatches one device method.
pub fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let ok = |r: sim_coresim::Result<()>| r.map(|()| Value::Null).map_err(err);
    match method {
        "devices" => {
            let booted = p.get("booted").and_then(Value::as_bool).unwrap_or(false);
            let list: Vec<_> =
                s.ctx()?.devices().map_err(err)?.iter().map(Device::info).filter(|d| !booted || d.state == DeviceState::Booted).collect();
            serde_json::to_value(list).map_err(err)
        }
        "runtimes" => serde_json::to_value(s.ctx()?.runtimes().map_err(err)?).map_err(err),
        "device_types" => serde_json::to_value(s.ctx()?.device_types().map_err(err)?).map_err(err),
        "create" => {
            let device_type = need_str(p, "device_type")?;
            if let Some(form_factor) = opt_form_factor(p)? {
                check_form_factor(s.ctx()?, device_type, form_factor)?;
            }
            let d = s.ctx()?.create(need_str(p, "name")?, device_type, need_str(p, "runtime")?).map_err(err)?;
            serde_json::to_value(d.info()).map_err(err)
        }
        "delete" => {
            let d = s.device(p)?;
            let udid = d.info().udid;
            s.ctx()?.delete(&d).map_err(err)?;
            // The service leaves launchd_sim's directory (its log, ~1-3 MB, and the disables)
            // behind; it is keyed by the UDID, so nothing reads it again.
            if let Some(dir) = sim_core::launchd::disabled_plist_path(&udid).parent().filter(|_| !udid.is_empty()) {
                let _ = std::fs::remove_dir_all(dir);
            }
            Ok(Value::Null)
        }
        "boot" => {
            let plan = boot_plan(p, 1, false, sim_netshape::dylib)?;
            let timed = boot_device(&s.device(p)?, &plan)?;
            // Without settle or a profile, `boot` answers as it always has.
            Ok(if plan.settle.is_some() || plan.profile.is_some() { timed } else { Value::Null })
        }
        "boot_many" => {
            let udids = strings(p, "devices");
            if udids.is_empty() {
                return Err("give `devices`: a list of UDIDs or names".into());
            }
            let concurrency = crate::params::concurrency(p)?;
            // Settles by default: the slot is held through the settle wait, so K bounds the boot
            // storms in flight (what the K sweep measured), not just the service's boot calls.
            let plan = boot_plan(p, concurrency.min(udids.len()), true, sim_netshape::dylib)?;
            let mut exact = Vec::with_capacity(udids.len());
            for q in &udids {
                exact.push(s.ctx()?.resolve(q).map(|(_, info)| info.udid).map_err(err)?);
            }
            let start = std::time::Instant::now();
            let results = boot_many(&exact, concurrency, &plan);
            let failed = results.iter().filter(|r| r.get("error").is_some()).count();
            let unsettled = results.iter().filter(|r| r["settled"] == json!(false)).count();
            Ok(json!({"concurrency": concurrency, "all_s": round(start.elapsed().as_secs_f64()), "failed": failed,
                "unsettled": unsettled, "devices": results}))
        }
        "settle" => {
            let d = s.device(p)?;
            if d.state() != DeviceState::Booted {
                return Err(format!("{} isn't booted", d.info().name));
            }
            let rule = settle_rule(p, "")?;
            let timeout = opt_u64(p, "timeout_s").map_or_else(|| crate::settle::default_settle_timeout(1), Duration::from_secs);
            let Settled { at, settler } = wait_settled(&d.info().udid, rule, timeout, false);
            let settle_s = at.ok_or_else(|| not_settled(&settler, rule, timeout))?;
            let mut result = json!({"settle_s": round(settle_s)});
            put_load(&mut result, &settler);
            Ok(result)
        }
        "profile" => {
            let d = s.device(p)?;
            if d.state() != DeviceState::Booted {
                return Err(format!("{} isn't booted", d.info().name));
            }
            let profile = profile(need_str(p, "name")?)?;
            let undo = p.get("undo").and_then(Value::as_bool).unwrap_or(false);
            let mut r = apply_profile(&d, profile, undo)?.to_json(undo);
            r["name"] = json!(profile.name);
            Ok(r)
        }
        "clone" => {
            let template = s.device(p)?;
            if template.state() != DeviceState::Shutdown {
                return Err(format!("{} must be shut down to clone it", template.info().name));
            }
            let d = s.ctx()?.clone_device(&template, need_str(p, "name")?).map_err(err)?;
            serde_json::to_value(d.info()).map_err(err)
        }
        "shutdown" => ok(s.device(p)?.shutdown()),
        "erase" => ok(s.device(p)?.erase()),
        "screen" => {
            let (w, h) = screen_size(s, p)?;
            Ok(json!({"width": w, "height": h}))
        }
        // Captured on the device's worker, encoded here: the worker is free for input meanwhile.
        "screenshot" => screenshot_result(&capture(s, p)?, p),
        m if INPUT_METHODS.contains(&m) => input(s, m, p),
        "rotate" => rotate(&s.device(p)?, opt_orientation(p)?),
        "carplay" => {
            let on = need_bool(p, "on")?;
            s.device(p)?.set_carplay(on).map_err(err)?;
            let size = carplay_screen();
            Ok(if on { json!({"display": "carplay", "width": size.width, "height": size.height}) } else { Value::Null })
        }
        "open_url" => ok(s.device(p)?.open_url(need_str(p, "url")?)),
        "install" => ok(s.device(p)?.install(need_str(p, "path")?)),
        "uninstall" => ok(s.device(p)?.uninstall(need_str(p, "bundle_id")?)),
        "launch" => {
            let d = s.device(p)?;
            let mut env = pairs(p, "env");
            if p.get("shaped").and_then(Value::as_bool).unwrap_or(false) {
                env.extend(sim_netshape::launch_env(&d.info().udid, &sim_netshape::dylib()?));
            }
            let pid = d.launch_with(need_str(p, "bundle_id")?, &strings(p, "args"), &env).map_err(err)?;
            Ok(json!({"pid": pid}))
        }
        "terminate" => ok(s.device(p)?.terminate(need_str(p, "bundle_id")?)),
        "apps" => serde_json::to_value(s.device(p)?.installed_apps().map_err(err)?).map_err(err),
        "appearance" => {
            let mode = match need_str(p, "mode")? {
                "light" => Appearance::Light,
                "dark" => Appearance::Dark,
                other => return Err(format!("unknown mode `{other}` (light or dark)")),
            };
            ok(s.device(p)?.set_appearance(mode))
        }
        "location" => ok(s.device(p)?.set_location(need_f64(p, "latitude")?, need_f64(p, "longitude")?)),
        "privacy" => {
            let granted = match need_str(p, "action")? {
                "grant" => true,
                "revoke" => false,
                other => return Err(format!("unknown action `{other}`")),
            };
            let service = privacy_service(need_str(p, "service")?).ok_or("unknown privacy service")?;
            ok(s.device(p)?.set_privacy(&service, need_str(p, "bundle_id")?, granted))
        }
        "push" => {
            let payload = p.get("payload").ok_or("missing parameter `payload`")?;
            ok(s.device(p)?.push(need_str(p, "bundle_id")?, &payload.to_string()))
        }
        "status_bar" => {
            let pick = |key: &str, table: &[&str]| -> Result<Option<u64>, String> {
                match str_param(p, key) {
                    Some(name) => debug::index_of(table, name).map(Some).ok_or_else(|| format!("unknown {key} `{name}`")),
                    None => Ok(None),
                }
            };
            let bars = |key: &str| opt_u64(p, key).unwrap_or(3);
            let overrides = StatusBar {
                time: str_param(p, "time").map(str::to_owned),
                data_network: pick("data_network", debug::DATA_NETWORKS)?,
                wifi: pick("wifi_mode", debug::RADIO_MODES)?.map(|m| (m, bars("wifi_bars"))),
                cellular: pick("cellular_mode", debug::RADIO_MODES)?.map(|m| (m, bars("cellular_bars"))),
                operator: str_param(p, "operator").map(str::to_owned),
                battery: pick("battery_state", debug::BATTERY_STATES)?.map(|st| (st, opt_u64(p, "battery_level").unwrap_or(100))),
            };
            ok(s.device(p)?.override_status_bar(&overrides))
        }
        "clear_status_bar" => ok(s.device(p)?.clear_status_bar()),
        "content_size" => {
            let size = p.get("size").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).ok_or("missing parameter `size`")?;
            let category = debug::content_size(&size).ok_or_else(|| format!("unknown size `{size}`"))?;
            ok(s.device(p)?.set_content_size(category))
        }
        "increase_contrast" => ok(s.device(p)?.set_increase_contrast(need_bool(p, "on")?)),
        "dynamic_island" => ok(s.device(p)?.set_dynamic_island_suppressed(need_bool(p, "suppressed")?)),
        "backlight" => ok(s.device(p)?.set_backlight(need_bool(p, "on")?)),
        "keyboard_language" => ok(s.device(p)?.set_keyboard_language(need_str(p, "language")?)),
        "hardware_keyboard" => ok(s.device(p)?.set_hardware_keyboard(need_bool(p, "on")?)),
        "ca_debug" => {
            let option =
                p.get("option").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).ok_or("missing parameter `option`")?;
            let id = debug::ca_option(&option).ok_or_else(|| format!("unknown option `{option}`"))?;
            let on = need_bool(p, "on")?;
            ok(s.device(p)?.set_ca_debug(id, on))
        }
        "ca_debug_state" => {
            let d = s.device(p)?;
            Ok(debug::CA_OPTIONS
                .iter()
                .map(|(id, key, label)| json!({"id": id, "option": key, "label": label, "on": d.ca_debug(*id)}))
                .collect())
        }
        "reset_keychain" => ok(s.device(p)?.reset_keychain()),
        "memory_warning" => ok(s.device(p)?.memory_warning()),
        "notify" => {
            let d = s.device(p)?;
            let name = need_str(p, "name")?;
            if let Some(state) = opt_u64(p, "state") {
                d.set_notification_state(name, state).map_err(err)?;
            }
            ok(d.post_notification(name))
        }
        "biometric" => {
            let d = s.device(p)?;
            let face = p.get("face").and_then(Value::as_bool).unwrap_or(true);
            match need_str(p, "action")? {
                "match" => ok(d.post_notification(debug::biometric_notification(face, true))),
                "nomatch" => ok(d.post_notification(debug::biometric_notification(face, false))),
                action @ ("enroll" | "unenroll") => {
                    d.set_notification_state(debug::BIOMETRIC_ENROLLMENT, u64::from(action == "enroll")).map_err(err)?;
                    ok(d.post_notification(debug::BIOMETRIC_ENROLLMENT))
                }
                other => Err(format!("unknown action `{other}`")),
            }
        }
        "exec" => {
            let out = s.device(p)?.exec(need_str(p, "program")?, &strings(p, "args"), &pairs(p, "env")).map_err(err)?;
            Ok(json!({"pid": out.pid, "stdout": out.stdout, "stderr": out.stderr}))
        }
        "logs" => {
            let mut args = vec![
                "show".to_owned(),
                "--style".into(),
                "compact".into(),
                "--last".into(),
                str_param(p, "last").unwrap_or("1m").to_owned(),
            ];
            if let Some(pred) = str_param(p, "predicate") {
                args.extend(["--predicate".to_owned(), pred.to_owned()]);
            }
            let out = s.device(p)?.exec("log", &args, &[]).map_err(err)?;
            Ok(out.stdout.lines().map(str::to_owned).collect())
        }
        "processes" => serde_json::to_value(sim_debug::process::collect(&s.device(p)?.info().udid)?).map_err(err),
        "kill" => {
            let pid = p.get("pid").and_then(Value::as_i64).ok_or("missing number parameter `pid`")?;
            let signal = p.get("signal").and_then(Value::as_i64).unwrap_or(15);
            let udid = s.device(p)?.info().udid;
            if !pids(&udid)?.contains(&(pid as i32)) {
                return Err(format!("pid {pid} is not a process of this device"));
            }
            let status = std::process::Command::new("/bin/kill").arg(format!("-{signal}")).arg(pid.to_string()).status().map_err(err)?;
            if status.success() { Ok(Value::Null) } else { Err(format!("kill {pid} failed")) }
        }
        "connections" => serde_json::to_value(sim_debug::net::connections(&pids(&s.device(p)?.info().udid)?)?).map_err(err),
        "traffic" => serde_json::to_value(sim_debug::net::traffic(&pids(&s.device(p)?.info().udid)?)?).map_err(err),
        "containers" => {
            let root = s.device(p)?.data_path();
            serde_json::to_value(sim_debug::storage::apps(Path::new(&root), p.get("sizes").and_then(Value::as_bool).unwrap_or(false)))
                .map_err(err)
        }
        "ls" => {
            let root = s.device(p)?.data_path();
            serde_json::to_value(sim_debug::storage::list(Path::new(&root), str_param(p, "path").unwrap_or(""))?).map_err(err)
        }
        "read" => {
            let root = s.device(p)?.data_path();
            let bytes = sim_debug::storage::read(Path::new(&root), need_str(p, "path")?)?;
            Ok(
                json!({"bytes": bytes.len(), "base64": base64::engine::general_purpose::STANDARD.encode(&bytes), "text": String::from_utf8(bytes).ok()}),
            )
        }
        "write" => {
            let root = s.device(p)?.data_path();
            let bytes = match (str_param(p, "text"), str_param(p, "base64")) {
                (Some(t), _) => t.as_bytes().to_vec(),
                (None, Some(b)) => base64::engine::general_purpose::STANDARD.decode(b).map_err(err)?,
                (None, None) => return Err("give `text` or `base64`".into()),
            };
            sim_debug::storage::write(Path::new(&root), need_str(p, "path")?, &bytes).map(|()| Value::Null)
        }
        "rm" => {
            let root = s.device(p)?.data_path();
            sim_debug::storage::remove(Path::new(&root), need_str(p, "path")?).map(|()| Value::Null)
        }
        "clear_container" => {
            let root = s.device(p)?.data_path();
            let rel = need_str(p, "path")?;
            if !rel.trim_start_matches('/').starts_with("Containers/") {
                return Err("only paths under Containers/ can be cleared".into());
            }
            for entry in sim_debug::storage::list(Path::new(&root), rel)? {
                sim_debug::storage::remove(Path::new(&root), &format!("{}/{}", rel.trim_end_matches('/'), entry.name))?;
            }
            Ok(Value::Null)
        }
        "du" => {
            let root = s.device(p)?.data_path();
            let path = sim_debug::storage::confine(Path::new(&root), str_param(p, "path").unwrap_or(""))?;
            Ok(json!({"bytes": sim_debug::storage::size(&path)}))
        }
        "defaults_read" => {
            let out = s.device(p)?.exec("defaults", &["read".to_owned(), need_str(p, "domain")?.to_owned()], &[]).map_err(err)?;
            if out.stdout.is_empty() { Err(out.stderr.trim().to_owned()) } else { Ok(Value::String(out.stdout)) }
        }
        "defaults_write" => {
            let ty = match str_param(p, "type").unwrap_or("string") {
                "string" => "-string",
                "int" => "-int",
                "float" => "-float",
                "bool" => "-bool",
                other => return Err(format!("unknown type `{other}`")),
            };
            let value =
                p.get("value").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).ok_or("missing parameter `value`")?;
            let args = ["write".to_owned(), need_str(p, "domain")?.to_owned(), need_str(p, "key")?.to_owned(), ty.to_owned(), value];
            let out = s.device(p)?.exec("defaults", &args, &[]).map_err(err)?;
            if out.stderr.trim().is_empty() || !out.stderr.contains("rror") { Ok(Value::Null) } else { Err(out.stderr.trim().to_owned()) }
        }
        "crash_reports" => serde_json::to_value(sim_debug::storage::crash_reports(&s.device(p)?.info().udid)).map_err(err),
        "info" => {
            let d = s.device(p)?;
            let info = d.info();
            Ok(json!({"device": info, "data_path": d.data_path(), "runtime_root": d.runtime_root().ok()}))
        }
        "network_shape" => {
            let d = s.device(p)?;
            let udid = d.info().udid;
            let profile = profile_from(p, sim_netshape::read_profile(&udid))?;
            let path = sim_netshape::write_profile(&udid, &profile)?;
            let use_proxy = p.get("proxy").and_then(Value::as_bool).unwrap_or(true);
            // inject: false | true (launchd setenv: new processes) | "boot" (reboot: every process).
            let inject_mode = match p.get("inject") {
                Some(Value::Bool(true)) => "launchd",
                Some(Value::String(m)) if m == "boot" || m == "launchd" => m.as_str(),
                Some(Value::String(m)) if m == "none" => "none",
                None | Some(Value::Bool(false)) => "none",
                Some(other) => return Err(format!("inject must be true, false, \"launchd\" or \"boot\", not {other}")),
            };
            let inject = inject_mode != "none";
            let mut port = None;
            if use_proxy {
                let started = with_proxies(|m| -> Result<u16, String> {
                    if let Some(px) = m.get(&udid) {
                        return Ok(px.port);
                    }
                    let px = sim_netshape::proxy::Proxy::start(path.clone()).map_err(err)?;
                    let port = px.port;
                    m.insert(udid.clone(), px);
                    Ok(port)
                })?;
                // Offline points the device at a closed port: clients then fail exactly as with no
                // network ("could not connect to the server"), rather than seeing a proxy error.
                let target = if profile.offline { closed_port()? } else { started };
                device_proxy(&d, &["set", "127.0.0.1", &target.to_string()])?;
                port = Some(started);
            }
            match inject_mode {
                "launchd" => device_inject(&d, true)?,
                "boot" => boot_inject(&d)?,
                _ => {}
            }
            Ok(json!({"profile": profile, "proxy_port": port, "injected": inject, "inject_mode": inject_mode, "profile_path": path}))
        }
        "network_state" => {
            let udid = s.device(p)?.info().udid;
            let port = with_proxies(|m| m.get(&udid).map(|px| px.port));
            let inject = s
                .device(p)?
                .exec("launchctl", &["getenv".to_owned(), "DYLD_INSERT_LIBRARIES".to_owned()], &[])
                .map(|o| o.stdout.contains("netshape"))
                .unwrap_or(false);
            Ok(
                json!({"profile": sim_netshape::read_profile(&udid), "proxy_port": port, "injected": inject, "boot_injected": boot_injected(&udid)}),
            )
        }
        "network_flows" => {
            let udid = s.device(p)?.info().udid;
            with_proxies(|m| m.get(&udid).map(|px| serde_json::to_value(px.flows()).map_err(err)))
                .unwrap_or_else(|| Err("no shaping proxy for this device; call network_shape".into()))
        }
        "network_clear" => {
            let d = s.device(p)?;
            let udid = d.info().udid;
            sim_netshape::write_profile(&udid, &sim_netshape::Profile::default())?;
            let proxy = device_proxy(&d, &["clear"]);
            let inject = device_inject(&d, false);
            with_proxies(|m| m.remove(&udid));
            proxy.and(inject).map(|()| Value::Null)
        }
        "audio_state" => {
            let d = s.device(p)?;
            let settings = sim_audio::settings(Path::new(&d.data_path())).unwrap_or(Value::Null);
            let udid = d.info().udid;
            let muted = with_captures(|m| m.contains_key(&format!("mute-{udid}")));
            Ok(json!({"settings": settings, "muted": muted, "host_devices": sim_audio::host_devices().unwrap_or(Value::Null)}))
        }
        "audio_volume" => {
            let level = opt_u64(p, "level").ok_or("missing number parameter `level`")?.min(100) as u8;
            sim_audio::set_volume(Path::new(&s.device(p)?.data_path()), level).map(|()| Value::Null)
        }
        "audio_route" => {
            let output = match need_str(p, "scope")? {
                "output" => true,
                "input" => false,
                other => return Err(format!("scope must be input or output, not `{other}`")),
            };
            sim_audio::set_route(Path::new(&s.device(p)?.data_path()), output, need_str(p, "uid")?).map(|()| Value::Null)
        }
        "audio_capture_start" => {
            let udid = s.device(p)?.info().udid;
            let pid = sim_debug::process::launchd_sim_pid(&udid)?;
            let path = str_param(p, "path").map(std::path::PathBuf::from);
            let mute = p.get("mute").and_then(Value::as_bool).unwrap_or(false);
            let cap = sim_audio::Capture::start(pid, path.as_deref(), mute)?;
            let id = format!("cap-{}", NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            let started = cap.started.clone();
            with_captures(|m| m.insert(id.clone(), cap));
            Ok(json!({"capture_id": id, "started": started}))
        }
        "audio_capture_stop" => {
            let id = need_str(p, "capture_id")?;
            let cap = with_captures(|m| m.remove(id)).ok_or_else(|| format!("no capture `{id}`"))?;
            cap.stop()
        }
        "audio_mute" => {
            let udid = s.device(p)?.info().udid;
            let key = format!("mute-{udid}");
            if need_bool(p, "on")? {
                if with_captures(|m| m.contains_key(&key)) {
                    return Ok(json!({"muted": true}));
                }
                let pid = sim_debug::process::launchd_sim_pid(&udid)?;
                let cap = sim_audio::Capture::start(pid, None, true)?;
                with_captures(|m| m.insert(key, cap));
            } else if let Some(cap) = with_captures(|m| m.remove(&key)) {
                cap.stop()?;
            }
            Ok(json!({"muted": with_captures(|m| m.contains_key(&format!("mute-{udid}")))}))
        }
        "network_env" => {
            let udid = s.device(p)?.info().udid;
            let dylib = sim_netshape::dylib()?;
            Ok(sim_netshape::launch_env(&udid, &dylib)
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect::<serde_json::Map<_, _>>()
                .into())
        }
        other => Err(format!("unknown method `{other}`; call `help`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slim_profile_keeps_what_automobile_needs() {
        let slim = profile("slim").expect("slim");
        let mut seen = std::collections::HashSet::new();
        for label in slim.disabled {
            assert!(seen.insert(label), "{label} twice");
            assert!(label.starts_with("com.apple.") && !label.contains(['\'', ' ', ';']), "{label}");
        }
        for kept in [
            "com.apple.SpringBoard",
            "com.apple.backboardd",
            "com.apple.InputUI",
            "com.apple.AccessibilityUIServer",
            "com.apple.mobile.installd",
            "com.apple.lsd",
            "com.apple.runningboardd",
            "com.apple.tccd",
            "com.apple.locationd",
            "com.apple.sharingd",
            "com.apple.WebBookmarks.webbookmarksd",
            "com.apple.assetsd",
            "com.apple.syslogd",
            "com.apple.usernotificationsd",
        ] {
            assert!(!slim.disabled.contains(&kept), "{kept} is needed");
        }
        assert_eq!(profile("tiny").expect_err("unknown"), "unknown profile `tiny` (slim)");
    }

    /// The in-device jobs simee's own methods reach (launchd labels of the iOS 27.2 runtime's
    /// LaunchDaemons and LaunchAngels), by method.
    const NEEDED_BY_METHODS: &[(&str, &[&str])] = &[
        ("carplay", &["com.apple.carkitd", "com.apple.CarPlayApp", "com.apple.SpringBoard"]),
        ("location", &["com.apple.locationd"]),
        ("privacy", &["com.apple.tccd"]),
        ("push", &["com.apple.apsd", "com.apple.usernotificationsd", "com.apple.SpringBoard"]),
        ("appearance", &["com.apple.SpringBoard", "com.apple.backboardd", "com.apple.cfprefsd.xpc.daemon"]),
        ("keyboard_language", &["com.apple.TextInput.kbd", "com.apple.InputUI", "com.apple.cfprefsd.xpc.daemon"]),
        ("hardware_keyboard", &["com.apple.InputUI", "com.apple.backboardd"]),
        ("type", &["com.apple.InputUI", "com.apple.TextInput.kbd", "com.apple.backboardd"]),
        ("open_url", &["com.apple.lsd", "com.apple.WebBookmarks.webbookmarksd", "com.apple.SpringBoard"]),
        ("biometric", &["com.apple.notifyd", "com.apple.CoreAuthentication.daemon", "com.apple.SpringBoard"]),
        ("status_bar", &["com.apple.SpringBoard", "com.apple.systemstatusd"]),
        ("clear_status_bar", &["com.apple.SpringBoard", "com.apple.systemstatusd"]),
        ("notify", &["com.apple.notifyd"]),
        ("memory_warning", &["com.apple.notifyd"]),
        ("reset_keychain", &["com.apple.securityd"]),
        ("defaults_read", &["com.apple.cfprefsd.xpc.daemon"]),
        ("defaults_write", &["com.apple.cfprefsd.xpc.daemon"]),
        ("install", &["com.apple.mobile.installd", "com.apple.lsd"]),
        ("launch", &["com.apple.runningboardd", "com.apple.SpringBoard"]),
        ("apps", &["com.apple.mobile.installd", "com.apple.lsd"]),
        ("logs", &["com.apple.syslogd", "com.apple.logd"]),
        ("network_shape", &["com.apple.configd_sim", "com.apple.nsurlsessiond"]),
        ("audio_volume", &["com.apple.audio.systemsoundserver-simd"]),
        ("content_size", &["com.apple.cfprefsd.xpc.daemon", "com.apple.SpringBoard"]),
    ];

    #[test]
    fn the_slim_profile_keeps_what_simee_methods_need() {
        let slim = profile("slim").expect("slim");
        for (method, jobs) in NEEDED_BY_METHODS {
            assert!(crate::methods::METHODS.iter().any(|(m, ..)| m == method), "`{method}` is a method");
            for job in *jobs {
                assert!(!slim.disabled.contains(job), "`{method}` needs {job}");
            }
        }
    }

    #[test]
    fn the_shaper_is_resolved_once_for_the_whole_plan() {
        let calls = std::cell::Cell::new(0);
        let resolve = || {
            calls.set(calls.get() + 1);
            Ok(std::path::PathBuf::from("/cache/netshape.dylib"))
        };
        let plan = boot_plan(&json!({"shaped": true}), 8, true, resolve).expect("plan");
        assert_eq!(calls.get(), 1);
        assert_eq!(plan.shaper.as_deref(), Some(Path::new("/cache/netshape.dylib")), "every worker uses this path");
        let plan = boot_plan(&json!({}), 1, false, || Err("not built".into())).expect("unshaped plans don't build it");
        assert!(plan.shaper.is_none());
        assert_eq!(boot_plan(&json!({"shaped": true}), 1, false, || Err("clang failed".into())).expect_err("build error"), "clang failed");
    }

    #[test]
    fn boot_many_settles_by_default_with_a_timeout_for_its_concurrency() {
        let none = || Err("unused".to_owned());
        let plan = boot_plan(&json!({}), 8, true, none).expect("boot_many");
        assert!(plan.settle.is_some(), "boot_many settles unless told not to");
        assert_eq!(plan.settle_timeout, Duration::from_secs(1200));
        assert!(boot_plan(&json!({"settle": false}), 8, true, none).expect("no settle").settle.is_none());
        let plan = boot_plan(&json!({"timeout_s": 120, "settle": true}), 1, false, none).expect("boot");
        assert_eq!((plan.timeout, plan.settle_timeout), (Duration::from_secs(120), Duration::from_secs(600)), "separate budgets");
        assert_eq!(
            boot_plan(&json!({"settle": true, "settle_timeout_s": 30}), 1, false, none).expect("given").settle_timeout,
            Duration::from_secs(30)
        );
        assert!(boot_plan(&json!({}), 1, false, none).expect("boot").settle.is_none(), "boot settles only when asked");
    }

    #[test]
    fn a_profile_reports_what_launchctl_did() {
        let labels = ["com.apple.a", "com.apple.b", "com.apple.c", "com.apple.gone"];
        // As the iOS 27.2 runtime's launchctl prints them.
        let loaded = "PID\tStatus\tLabel\n12\t0\tcom.apple.a\n-\t0\tcom.apple.b\n13\t0\tcom.apple.c\n";
        let disabled = "\n\tdisabled services = {\n\t\t\"com.apple.a\" => disabled\n\t\t\"com.apple.b\" => enabled\n\
                        \t\t\"com.apple.gone\" => true\n\t\t\"com.apple.other\" => disabled\n\t}\n";
        // b: launchctl refused; c: it said nothing but launchd doesn't list it disabled; gone:
        // disabled (launchctl disables any label), but there was no such job loaded.
        assert_eq!(
            profile_outcome(&labels, loaded, &[], &["com.apple.b"], disabled, false),
            Applied {
                changed: 2,
                failed: vec!["com.apple.b".into(), "com.apple.c".into()],
                not_loaded: vec!["com.apple.gone".into()],
                already: 0
            }
        );
        // Undone: a label is enabled once launchd no longer lists it disabled.
        let undone = "\t\"com.apple.a\" => enabled\n\t\"com.apple.gone\" => disabled\n";
        let r = profile_outcome(&labels, "", &[], &[], undone, true);
        assert_eq!((r.changed, &r.failed), (3, &vec!["com.apple.gone".to_owned()]));
        assert_eq!(r.to_json(true), json!({"enabled": 3, "failed": ["com.apple.gone"]}));
        // No listings from launchctl: judged by what it refused alone.
        let r = profile_outcome(&labels, "", &[], &["com.apple.c"], "", false);
        assert_eq!((r.changed, &r.failed, r.not_loaded.len()), (3, &vec!["com.apple.c".to_owned()], 0));
        assert_eq!(r.to_json(false), json!({"disabled": 3, "failed": ["com.apple.c"], "not_loaded": [], "already": 0}));
        // What the runtime's launchctl prints when disabling a running job: a warning, not a failure.
        assert!(!launchctl_refused(
            "Warning: Please switch to user/foreground/com.apple.translationd service identifier (rdar://78126471)\n"
        ));
        assert!(!launchctl_refused(""));
        assert!(launchctl_refused("Could not find service \"x\" in domain for system\n"));
    }

    #[test]
    fn a_reapplied_profile_touches_only_the_jobs_that_came_back() {
        let labels = ["com.apple.a", "com.apple.companionappd", "com.apple.NPKCompanionAgent", "com.apple.new"];
        // A slimmed device after a reboot (as slim-launchctl-list / print-disabled read on iOS
        // 27.2): a is still off; the companions came back enabled and running; new was never
        // disabled (added to the profile since) and isn't running.
        let loaded = "PID\tStatus\tLabel\n66382\t0\tcom.apple.companionappd\n66387\t0\tcom.apple.NPKCompanionAgent\n\
                      12\t0\tcom.apple.SpringBoard\n";
        let disabled = "\tdisabled services = {\n\t\t\"com.apple.a\" => disabled\n\
                        \t\t\"com.apple.companionappd\" => enabled\n\t\t\"com.apple.NPKCompanionAgent\" => disabled\n\t}\n";
        assert_eq!(
            still_on(&labels, loaded, disabled),
            ["com.apple.companionappd", "com.apple.NPKCompanionAgent", "com.apple.new"],
            "re-enabled, loaded though disabled, and never disabled"
        );
        // A fresh device (nothing disabled) or a listing that couldn't be read: every job.
        assert_eq!(still_on(&labels, loaded, "\tdisabled services = {\n\t}\n"), labels);
        assert_eq!(still_on(&labels, "", disabled), labels);
        assert_eq!(still_on(&labels, loaded, ""), labels);
        // What was left alone counts as disabled and isn't reported as not loaded.
        let after = "\t\"com.apple.a\" => disabled\n\t\"com.apple.companionappd\" => disabled\n\
                     \t\"com.apple.NPKCompanionAgent\" => disabled\n\t\"com.apple.new\" => disabled\n";
        let r = profile_outcome(&labels, loaded, &["com.apple.a"], &[], after, false);
        assert_eq!(r, Applied { changed: 4, failed: vec![], not_loaded: vec!["com.apple.new".into()], already: 1 });
        assert_eq!(r.to_json(false)["already"], json!(1));
        assert_eq!(r.to_json(true).get("already"), None, "undo touches every job");
    }

    #[test]
    fn the_slim_profile_keeps_sleepd() {
        // Without sleepd, SpringBoard and mobiletimerd spin retrying its mach service on every
        // later boot and the device never settles (docs/farm.md).
        let slim = profile("slim").expect("slim");
        assert!(!slim.disabled.contains(&"com.apple.sleepd"));
        assert_eq!(slim.disabled.len(), 62);
        for companion in ["com.apple.companionappd", "com.apple.NPKCompanionAgent", "com.apple.brook.brookcompaniond"] {
            assert!(slim.disabled.contains(&companion), "{companion} stays in: re-applied on every boot");
        }
    }

    #[test]
    fn the_slim_profile_turns_off_what_reenables_the_watch_daemons() {
        // nanoregistrylaunchd enables the 17 NanoLaunchDaemons whenever it runs; without it off,
        // the companions come back one to three minutes after every boot (docs/farm.md).
        let slim = profile("slim").expect("slim");
        for label in
            ["com.apple.nanoregistrylaunchd", "com.apple.addressbooksyncd", "com.apple.bulletindistributord", "com.apple.pairedsyncd"]
        {
            assert!(slim.disabled.contains(&label), "{label}");
        }
        let unique: std::collections::HashSet<_> = slim.disabled.iter().collect();
        assert_eq!(unique.len(), slim.disabled.len(), "no label twice");
    }

    #[test]
    fn a_preset_disables_the_profile_before_boot_and_keeps_other_overrides() {
        let dir = std::env::temp_dir().join(format!("simee-preset-{}", std::process::id()));
        let path = dir.join("com.apple.CoreSimulator.SimDevice.X").join("disabled.plist");
        let _ = std::fs::remove_dir_all(&dir);
        // A clone's first boot: no directory yet.
        assert_eq!(preset_disables(&path, &["a", "b"]), Ok(2));
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(path.parent().expect("dir")).expect("made").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "as launchd_sim makes it");
        }
        // A later boot: launchd (or nanoregistrylaunchd) enabled `b` and kept its own `c`.
        std::fs::write(&path, sim_core::launchd::render(&[("b".into(), false), ("c".into(), false)], &["a"], true)).expect("write");
        assert_eq!(preset_disables(&path, &["a", "b"]), Ok(1), "only b is new");
        let back = sim_core::launchd::parse(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back, [("b".to_owned(), true), ("c".to_owned(), false), ("a".to_owned(), true)]);
        assert_eq!(preset_disables(&path, &["a", "b"]), Ok(0), "nothing to write");
        // A file simee can't read is left alone.
        std::fs::write(&path, "bplist00").expect("write");
        assert!(preset_disables(&path, &["a"]).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "bplist00");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_temp_file_for_the_disables_is_never_a_planted_symlink() {
        let dir = std::env::temp_dir().join(format!("simee-write-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").expect("victim");
        let tmp = dir.join("disabled.plist.simee");
        std::os::unix::fs::symlink(&victim, &tmp).expect("plant");
        assert!(write_new(&tmp, &dir, b"x").is_err(), "an existing name, symlink or not, is refused");
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep", "not written through the link");
        // A dangling link too (O_EXCL fails on the link itself).
        std::fs::remove_file(&tmp).expect("rm");
        std::os::unix::fs::symlink(dir.join("missing"), &tmp).expect("plant dangling");
        assert!(write_new(&tmp, &dir, b"x").is_err());
        assert!(!dir.join("missing").exists());
        std::fs::remove_file(&tmp).expect("rm");
        // A free name works and has the bytes.
        write_new(&tmp, &dir, b"ok").expect("free name");
        assert_eq!(std::fs::read_to_string(&tmp).expect("read"), "ok");
        // A directory that is itself a symlink is refused, and leaves no file behind.
        let link = std::env::temp_dir().join(format!("simee-write-new-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).expect("dir link");
        let via = link.join("new");
        assert!(write_new(&via, &link, b"x").is_err());
        assert!(!dir.join("new").exists());
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_disables_are_not_written_into_another_users_directory() {
        // The sticky, world-writable /private/tmp (macOS) or /tmp is root's: anyone can create a
        // file in it, which is what a hostile directory looks like. Skipped as root (the owner).
        let dir = std::path::Path::new(if std::path::Path::new("/private/tmp").is_dir() { "/private/tmp" } else { "/tmp" });
        let tmp = dir.join(format!("simee-foreign-dir-{}", std::process::id()));
        match write_new(&tmp, dir, b"x") {
            Err(e) => {
                assert!(e.contains("isn't a directory owned by the current user"), "{e}");
                assert!(!tmp.exists(), "nothing left behind");
            }
            Ok(()) => {
                eprintln!("skipped: running as the directory's owner (root), no other uid to test with");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    #[test]
    fn settle_rules_read_their_keys() {
        let none = || -> Result<std::path::PathBuf, String> { Err("unused".into()) };
        let plan = boot_plan(&json!({"settle": true}), 1, false, none).expect("boot");
        assert_eq!(plan.settle, Some(crate::settle::Rule::default()), "foreground CPU by default");
        let p = json!({"settle": true, "settle_cpu_pct": 5.0, "settle_window_s": 20, "settle_foreground": false});
        let rule = boot_plan(&p, 1, false, none).expect("boot").settle.expect("settle");
        assert_eq!(rule, crate::settle::Rule { cpu_pct: 5.0, window: Duration::from_secs(20), foreground: false });
        let rule = settle_rule(&json!({"cpu_pct": 7.0, "window_s": 3, "foreground": false}), "").expect("settle");
        assert_eq!(rule, crate::settle::Rule { cpu_pct: 7.0, window: Duration::from_secs(3), foreground: false });
        assert!(settle_rule(&json!({"settle_foreground": false}), "").expect("settle").foreground, "settle's own keys have no prefix");
        // A mistyped option is refused, not silently replaced by the default.
        for bad in [
            json!({"settle": true, "settle_foreground": "true"}),
            json!({"settle": true, "settle_foreground": 1}),
            json!({"settle": true, "settle_cpu_pct": "5"}),
            json!({"settle": true, "settle_window_s": -1}),
        ] {
            assert!(boot_plan(&bad, 1, false, none).is_err(), "{bad}");
        }
        assert!(settle_rule(&json!({"foreground": "yes"}), "").is_err());
    }

    #[test]
    fn a_settle_result_reports_background_and_foreground_cpu() {
        let mut settler = crate::settle::Settler::new(crate::settle::Rule::default());
        let tree = |total_s, background_s| crate::settle::Tree {
            cpu: [(1, crate::settle::Cpu { total_s, background_s })].into(),
            springboard: true,
        };
        settler.push(0.0, tree(0.0, 0.0));
        settler.push(10.0, tree(3.0, 2.0));
        let mut r = json!({});
        put_load(&mut r, &settler);
        assert_eq!(
            r,
            json!({"procs": 1, "cpu_pct": 30.0, "background_cpu_pct": 20.0, "foreground_cpu_pct": 10.0, "cpu_s": 3.0,
                   "background_cpu_s": 2.0})
        );
        let rule = crate::settle::Rule { foreground: true, ..Default::default() };
        assert_eq!(
            not_settled(&settler, rule, Duration::from_secs(60)),
            "device did not settle within 60s (10 % of a core in the foreground over the last 10s)"
        );
        assert_eq!(
            not_settled(&settler, crate::settle::Rule { foreground: false, ..Default::default() }, Duration::from_secs(60)),
            "device did not settle within 60s (30 % of a core over the last 10s)"
        );
    }

    #[test]
    fn a_missing_udid_is_reported_as_typed() {
        let lower = "0000aaaa-bbbb-cccc-dddd-eeeeffff0000";
        let upper = lower.to_ascii_uppercase();
        let missing = || format!("no device matches `{upper}`");
        assert_eq!(as_typed(missing(), &json!({"device": lower})), format!("no device matches `{lower}`"));
        assert_eq!(as_typed(missing(), &json!({"device": upper})), missing());
        assert_eq!(as_typed("other".into(), &json!({"device": "booted"})), "other");
        assert_eq!(as_typed(missing(), &json!({})), missing());
    }
}
