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
    button, crown_delta, err, need_bool, need_f64, need_str, on_carplay, opt_form_factor, opt_orientation, opt_u64, pairs,
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
/// made them), runs that device's input in order, and lets go after five idle minutes.
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

    fn try_run<R>(&mut self, udid: &str, screen: bool, send: impl FnOnce(&Hid, Option<Size>) -> Result<R, String>) -> Result<R, String> {
        let Self { ctx, device, changes, watch, hid, display } = self;
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
        let size = if screen {
            let (w, h) = display.with(key, || device.display().map_err(err), |d| d.size().map_err(err))?;
            Some(Size::new(f64::from(w), f64::from(h)))
        } else {
            None
        };
        hid.with(key, || device.hid().map_err(err), |hid| send(hid, size))
    }
}

/// Runs an input method on its device's worker.
fn input(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let udid = s.udid(p)?;
    let (method, p) = (method.to_owned(), p.clone());
    INPUT.run(&udid.clone(), move |input| sim_coresim::autoreleasepool(|| input_method(input, &udid, &method, &p)))?
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
            ok(s.ctx()?.delete(&d))
        }
        "boot" => {
            let d = s.device(p)?;
            let mut env = pairs(p, "env");
            if p.get("shaped").and_then(Value::as_bool).unwrap_or(false) {
                env.extend(sim_netshape::launch_env(&d.info().udid, &sim_netshape::dylib()?));
            }
            if d.state() != DeviceState::Booted && !env.is_empty() {
                d.boot_with_env(&env).map_err(err)?;
            }
            boot(&d, Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300))).map(|()| Value::Null)
        }
        "shutdown" => ok(s.device(p)?.shutdown()),
        "erase" => ok(s.device(p)?.erase()),
        "screen" => {
            let (w, h) = display_of(&s.device(p)?, p)?.size().map_err(err)?;
            Ok(json!({"width": w, "height": h}))
        }
        "screenshot" => screenshot_result(&display_of(&s.device(p)?, p)?.capture().map_err(err)?, p),
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
