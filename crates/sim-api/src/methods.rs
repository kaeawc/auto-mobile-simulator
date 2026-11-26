//! The method table and dispatch. A `device` parameter is resolved across both platforms (or one,
//! with `platform`), then the method runs on that device's backend: `ios.rs` (macOS hosts only) or
//! `android.rs`. A method a platform doesn't offer fails with the platforms it does work on.

use serde_json::{Value, json};
use sim_core::device::{DeviceInfo, RuntimeInfo, Selection, select};
use sim_core::form_factor::{Kind, matrix};
use sim_core::platform::Platform;

use crate::params::{need_str, opt_form_factor, opt_platform, str_param};

/// (method, parameters, description). `device` is a UDID, a name, or `booted`. [`platforms`]
/// says which platforms' devices each method works on.
pub const METHODS: &[(&str, &str, &str)] = &[
    ("help", "", "List methods with the platforms each supports"),
    (
        "identity",
        "",
        "This process's build, executable and environment (hashed): the CLI forwards a command to a server only when they match its own",
    ),
    (
        "form_factors",
        "platform?, form_factor?",
        "Every form factor on every platform: sub-platform, inputs and reference profile, or why it's unsupported",
    ),
    ("devices", "booted?, platform?(android|ios), form_factor?", "List devices with their platform, form factor and sub-platform"),
    ("runtimes", "platform?", "List installed runtimes (Android: system images)"),
    ("device_types", "", "List device types"),
    (
        "create",
        "name, platform?(ios|android), form_factor?; iOS: device_type, runtime; Android: system_image, device?(pixel_fold|surface_duo)",
        "Create a device. form_factor is checked against the device type (iOS) or the system image (Android) and shapes a new AVD",
    ),
    ("delete", "device", "Delete a device"),
    (
        "boot",
        "device, timeout_s?; iOS: env?, shaped?, settle?, settle_cpu_pct?, settle_window_s?, settle_foreground?(true), settle_timeout_s?, profile?(slim); \
         Android: audio?(false)",
        "Boot and wait until finished (iOS: env is the device launchd's environment, shaped injects the network shaper; settle also waits \
         up to settle_timeout_s (600) until the device's processes use under settle_cpu_pct (10) % of a core for settle_window_s (10) s \
         (settle_foreground, the default: counting only foreground-QoS CPU) and returns the timings with cpu_pct, foreground_cpu_pct, \
         background_cpu_pct and cpu_s (CPU-seconds the tree used since the boot; for a device already booted, since the first sample), settled false if it ran out of time; profile turns off a named set \
         of daemons (written before the boot of a shut-down device, so they never start); Android: starts emu and \
         waits for sys.boot_completed, silent unless audio, which plays the device on the host's default output)",
    ),
    (
        "boot_many",
        "devices, concurrency?, timeout_s?; iOS: env?, shaped?, settle?(default true), settle_cpu_pct?, settle_window_s?, \
         settle_foreground?(true), settle_timeout_s?, profile?; Android: first_port?(5554)",
        "Boot several devices of one platform, at most `concurrency` at once (default: a quarter of the host's CPUs, at least 2), each as \
         `boot` does (iOS: a slot is held until the device settles, unless settle is false); returns each device's timings or error, \
         failed and unsettled counts, and the time until all were done. Android: each on its own console port from first_port up",
    ),
    (
        "settle",
        "device, cpu_pct?, window_s?, foreground?(true), timeout_s?(600)",
        "Wait until a booted simulator is quiet: its processes under cpu_pct (10) % of a core for window_s (10) s (foreground, the default: \
         only foreground-QoS CPU counts), SpringBoard running; returns settle_s, cpu_pct, foreground_cpu_pct and background_cpu_pct, and cpu_s and background_cpu_s (CPU-seconds used since the first sample)",
    ),
    (
        "profile",
        "device, name(slim), undo?",
        "Turn a named set of daemons off on a booted simulator (disabled in its launchd database and stopped: stays off across reboots), \
         or with undo back on (from the next boot); returns how many changed, those that failed and those that weren't loaded",
    ),
    (
        "clone",
        "device, name",
        "Create a device as a copy of a shut-down one. iOS: a simulator (data, settings, apps; APFS clones of its files); Android: an AVD \
         from a golden that has booted under simee (copy-on-write clones of its disks). A booted-once template's copies skip the \
         first-boot work",
    ),
    ("shutdown", "device", "Shut down"),
    ("erase", "device", "Erase content and settings (shut down first)"),
    (
        "screen",
        "device, display?(main|outer|inner|carplay)",
        "Screen size in pixels, the space tap, swipe and touch take (a folded Android foldable: the size its guest lays out for, a Pixel Fold's 1080x2092, as uiautomator's bounds). \
         display (iOS): main is the screen in use (an unfolded iPhone Duo's inner one), outer and inner an iPhone Duo's two, carplay an iPhone's CarPlay screen",
    ),
    (
        "screenshot",
        "device, path?, display?(main|outer|inner|carplay)",
        "PNG to path, else base64 in `png` (a folded Android foldable: its outer screen as the scanout shows it, a Pixel Fold's 949x1840; \
         an iPhone Duo's inner screen as its framebuffer holds it, 2007x2853)",
    ),
    (
        "tap",
        "device, x, y, hold_ms?, display?(main|outer|inner|carplay)",
        "Tap at pixels (display, iOS only: an iPhone Duo's outer or inner screen, an iPhone's CarPlay screen)",
    ),
    (
        "swipe",
        "device, x1, y1, x2, y2, duration_ms?, settle?, display?(main|outer|inner|carplay), edge?(top|bottom|left|right; iOS)",
        "Swipe between pixels; with edge (iOS) the touch comes in from that screen edge, as a drag from Simulator.app's bezel: \
         home and the app switcher from the bottom, Notification Center and Control Center from the top (Android's edge \
         gestures need only the start point)",
    ),
    (
        "touch",
        "device, x, y, phase(down|move|up), display?(main|outer|inner|carplay), edge?(top|bottom|left|right; iOS)",
        "One raw touch sample",
    ),
    (
        "button",
        "device, button(home|lock|side|siri|apple-pay|action | back|volume-up|volume-down|recents|menu|up|down|left|right|select|play-pause), hold_ms?",
        "Press a hardware button, as the device's form factor has it: the Siri Remote on Apple TV (menu/back, home, select, \
         play-pause, arrows) and the D-pad on Android TV; on Apple Watch home is the crown's press",
    ),
    ("type", "device, text", "Type on the hardware keyboard"),
    ("key", "device, usage", "Press a key by HID usage"),
    ("open_url", "device, url", "Open a URL"),
    ("install", "device, path", "Install an .app"),
    ("uninstall", "device, bundle_id", "Uninstall an app"),
    (
        "launch",
        "device, bundle_id, args?, env?, shaped?",
        "Launch with arguments and environment (shaped: inject the network shaper); returns pid",
    ),
    ("terminate", "device, bundle_id", "Terminate an app"),
    ("apps", "device", "Installed bundle ids"),
    ("appearance", "device, mode(light|dark)", "Light or dark UI"),
    ("location", "device, latitude, longitude", "Simulated location"),
    ("privacy", "device, action(grant|revoke), service, bundle_id", "Privacy permission"),
    ("push", "device, bundle_id, payload", "Deliver an APNs payload (object)"),
    (
        "status_bar",
        "device, time?, data_network?, wifi_mode?, wifi_bars?, cellular_mode?, cellular_bars?, operator?, battery_state?, battery_level?",
        "Override status bar",
    ),
    ("clear_status_bar", "device", "Remove status bar overrides"),
    ("content_size", "device, size", "Dynamic Type size (name or 0-11)"),
    ("increase_contrast", "device, on", "Increase Contrast"),
    ("dynamic_island", "device, suppressed", "Hide the Dynamic Island"),
    ("backlight", "device, on", "Display backlight"),
    ("keyboard_language", "device, language", "Software keyboard language"),
    ("hardware_keyboard", "device, on", "Connect the hardware keyboard"),
    ("ca_debug", "device, option, on", "Render overlay (see ca_debug_state for names)"),
    ("ca_debug_state", "device", "Every render overlay and whether it is on"),
    ("reset_keychain", "device", "Delete every keychain item"),
    ("memory_warning", "device", "Simulate a memory warning"),
    ("notify", "device, name, state?", "Post a Darwin notification (optionally setting its state)"),
    ("biometric", "device, action(match|nomatch|enroll|unenroll), face?", "Touch ID / Face ID"),
    ("exec", "device, program, args?, env?", "Run a program inside the device; returns stdout/stderr"),
    ("logs", "device, last?, predicate?", "Unified log lines (default last 1m)"),
    ("processes", "device", "Device processes with CPU and memory"),
    ("kill", "device, pid, signal?", "Signal a device process"),
    ("connections", "device", "Open sockets of device processes"),
    ("traffic", "device", "Bytes in/out per device process"),
    ("containers", "device, sizes?", "App data and bundle containers"),
    ("ls", "device, path?", "List a directory under the device data dir"),
    ("read", "device, path", "Read a file (base64)"),
    ("write", "device, path, text? | base64?", "Write a file"),
    ("rm", "device, path", "Remove a file or directory"),
    ("du", "device, path?", "Size in bytes"),
    ("clear_container", "device, path", "Empty a container (an app's data) without deleting it"),
    ("defaults_read", "device, domain", "Read a preferences domain"),
    ("defaults_write", "device, domain, key, value, type?", "Write a preference (type: string|int|float|bool)"),
    ("crash_reports", "device", "Host crash reports for this device"),
    ("info", "device", "Paths and identity"),
    (
        "network_shape",
        "device, preset?(offline|3g|edge|lte|lossy|very-bad|none), offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?, port_base?, port_count?, rules?([{pattern, offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?}]), proxy?(default true), inject?(false|true|\"launchd\"|\"boot\")",
        "Shape the device's network without root. proxy: a shaping HTTP/SOCKS proxy set in the device's configd_sim. inject: an in-process socket shaper; \"boot\" reboots the device with it in launchd's environment so every process, WebKit included, is shaped live by the profile",
    ),
    ("network_state", "device", "Current profile, proxy port and injection state"),
    ("network_flows", "device", "Connections through the device's shaping proxy (host, bytes, duration, outcome)"),
    ("network_clear", "device", "Remove shaping: clear the device proxy, stop the proxy, remove injection"),
    ("audio_state", "device", "Device audio settings (routed host UIDs, volume) and every host audio device"),
    ("audio_volume", "device, level(0-100)", "Device output volume (100 = unity); applies to audio clients started afterwards"),
    (
        "audio_route",
        "device, scope(input|output), uid | \"default\"",
        "Pin the device's input or output to a host device (any device, including loopback); applies to clients started afterwards",
    ),
    (
        "audio_capture_start",
        "device, path?(.wav), mute?",
        "Capture this device's audio output with a process tap (no driver); mute silences it on the host meanwhile; returns capture_id",
    ),
    ("audio_capture_stop", "capture_id", "Stop a capture: frames, duration_ms, rms, peak, dbfs, path"),
    ("audio_mute", "device, on", "Silence (or restore) this device's audio on the host: a per-device null sink"),
    ("network_env", "device", "Environment that makes one app launch shaped (for simctl: prefix each key with SIMCTL_CHILD_)"),
    (
        "console",
        "device, command | commands",
        "Emulator console commands (`adb emu`'s protocol: sensor, geo, sms, gsm, finger, power, ...); reply lines",
    ),
    (
        "rotate",
        "device, orientation?(portrait|landscape-right|portrait-upside-down|landscape-left; iOS)",
        "Rotate the device 90° clockwise, or to an orientation (iOS)",
    ),
    (
        "fold",
        "device, folded(true|false) | angle(0-180; 0-360 dual-screen) | posture(closed|half-opened|opened|flipped|tent)",
        "Set a foldable's hinge. Android: as the console's fold, unfold and posture do (the hinge sensor, the posture and, folded, \
         the outer screen). iOS: an iPhone Duo's (closed, half-opened is the 100° book posture, opened), which SpringBoard sweeps \
         to, moving the content to the inner screen when it opens past its swap boundary. Returns angle, posture, folded and the \
         size the guest lays out for (iOS: of the screen in use, named in display)",
    ),
    (
        "xr",
        "device, action(recenter | rotate|move: x, y, z | options: environment(day|night), passthrough(0-1) | input: mode(mouse|hand|eye) | viewport: mode(pan|zoom|rotate) | pointer: x, y, phase?(move|down|up|click) | hand|eye: x, y, press?(aim|press) | gesture: gesture(left-pinch|left-grab|left-poke|right-pinch|right-grab|right-poke))",
        "Android XR: turn (radians) or move (metres) the head, recenter, set the room and passthrough, the input mode, aim or pinch the hand ray and look or select with gaze at pixels (stock's gRPC hand and eye events), a hand gesture, or point the dual-mode mouse",
    ),
    (
        "touchpad",
        "device, x, y, phase?(down|move|up|tap) | x, y, to_x, to_y, duration_ms? (a swipe)",
        "Touch or swipe the touchpad beside the screen, in touchpad units (Android XR display glasses: `hw.touchpad0`)",
    ),
    (
        "stylus",
        "device, x, y, phase?(tap|hover|down|move|up|leave), pressure?(1-255), button?, eraser? | x, y, to_x, to_y, duration_ms? (a stroke)",
        "A stylus on the touch screen at pixels. Android: what the stock emulator's gRPC pen events send: hover over it, \
         touch, move, lift or leave, with the barrel button or the eraser end; or draw a stroke. iPad: the Apple Pencil \
         (UITouch type .pencil): tap, down, move, up or a stroke; hover, pressure, button and eraser are refused (the \
         simulator's touch screen has none)",
    ),
    (
        "rotary",
        "device, ticks | delta (Digital Crown units; iOS)",
        "Turn a watch's rotary input: the Wear OS crown or bezel, the Apple Watch Digital Crown",
    ),
    (
        "vehicle",
        "device, property?(VHAL name or id: GEAR_SELECTION, gear, speed, 0x11600207, ...), area_id?, value?",
        "Android Automotive's vehicle HAL: list every property's value, read one, or set one when `value` is given \
         (typed by the property: drive, 12.5, true); through the emulator console's `car` command (simee)",
    ),
    ("watch_gesture", "device, gesture(double-tap|wrist-flick)", "An Apple Watch hand gesture"),
    ("remote_swipe", "device, direction(up|down|left|right), duration_ms?", "Swipe on the Siri Remote's touch surface (Apple TV)"),
    (
        "pointer",
        "device, x, y, action?(move|click|down|up), button?(primary|secondary), hold_ms? | x, y, to_x, to_y, button? (a drag)",
        "The iPad's pointer (a mouse, as Simulator.app's pointer capture connects): move it to pixels and hover there, click, \
         press or release a button there, or drag to to_x, to_y. Exact on iOS 26.5, within ~20 pt on 27.2. A button held by \
         `down` keeps the pointer's place for the next calls on a server (`simee serve`) until a reboot; `down` without a \
         server, `up` with nothing held, and `move`/`up` after a failed call lost the held button are refused",
    ),
    (
        "carplay",
        "device, on",
        "Attach (or detach) the CarPlay display to a booted iPhone (iOS 26 runtimes); then tap/swipe/screenshot with display=carplay",
    ),
];

/// Per-connection state: each backend's service connection, created on first use.
#[derive(Default)]
pub struct Session {
    pub(crate) ios: crate::ios::Session,
    pub(crate) android: crate::android::Session,
}

impl Session {
    /// A session whose Android backend uses this SDK and AVD home instead of the environment's.
    pub fn with_android(android: sim_android::Android) -> Session {
        Session { android: crate::android::Session::with(android), ..Session::default() }
    }
}

/// Methods that work on every platform's devices (or need no device).
const ALL_PLATFORMS: &[&str] = &[
    "help",
    "identity",
    "form_factors",
    "devices",
    "runtimes",
    "create",
    "delete",
    "boot",
    "boot_many",
    "clone",
    "shutdown",
    "screen",
    "screenshot",
    "tap",
    "swipe",
    "touch",
    "button",
    "type",
    "key",
    "location",
    "info",
    "rotate",
    "fold",
    "rotary",
    "stylus",
];
/// Methods only Android devices have.
const ANDROID_ONLY: &[&str] = &["console", "xr", "vehicle", "touchpad"];

/// The platforms whose devices `method` works on.
pub fn platforms(method: &str) -> &'static [Platform] {
    if ALL_PLATFORMS.contains(&method) {
        &Platform::ALL
    } else if ANDROID_ONLY.contains(&method) {
        &[Platform::Android]
    } else {
        &[Platform::Ios]
    }
}

/// Resolves the `device` parameter to a platform and an exact id. With no Android devices around
/// (and no `platform` asking for one), the query goes to the iOS backend unchanged, which resolves
/// it exactly as it always has.
fn resolve(s: &mut Session, p: &Value) -> Result<(Platform, String), String> {
    let query = str_param(p, "device").unwrap_or("booted");
    let only = opt_platform(p)?;
    // An AVD's name is its UDID: resolve it without listing every device on both platforms
    // (AVD names have no spaces, simulator names usually do; `platform` decides otherwise).
    if only != Some(Platform::Ios) && s.android.backend().has_avd(query) {
        return Ok((Platform::Android, query.to_owned()));
    }
    // A simulator's UDID that isn't an AVD's name: the iOS backend looks it up itself. Listing
    // both platforms here (every AVD's config, every simulator's state) cost ~70 ms per call on
    // a host with 48 AVDs and 100 simulators, more than the tap or screenshot it routed.
    if only != Some(Platform::Android) && sim_core::device::is_udid(query) {
        return Ok((Platform::Ios, query.to_owned()));
    }
    let android = if only == Some(Platform::Ios) { Vec::new() } else { s.android.devices() };
    if android.is_empty() && only != Some(Platform::Android) {
        return Ok((Platform::Ios, query.to_owned()));
    }
    // Only what `select` reads of each simulator (UDID, name, state); the iOS backend reads the
    // chosen one's full info itself.
    let mut all = if only == Some(Platform::Android) { Vec::new() } else { crate::ios::selection_infos(&mut s.ios)? };
    all.extend(android);
    match select(&all, query) {
        Selection::One(i) => Ok((all[i].platform, all[i].udid.clone())),
        Selection::None => Err(format!("no device matches `{query}`")),
        Selection::Ambiguous => Err(format!("`{query}` matches several devices; use a UDID or `platform`")),
    }
}

/// Fails with the "no device matches" error when `query` was routed to iOS only by its UDID shape
/// and no such simulator exists. [`resolve`] doesn't look the simulator up (that listing costs more
/// than the call it routes), so a platform error that would be about a mistyped UDID checks first.
fn ios_device_exists(s: &mut Session, platform: Platform, query: &str) -> Result<(), String> {
    if platform != Platform::Ios || !sim_core::device::is_udid(query) {
        return Ok(());
    }
    let found = crate::ios::selection_infos(&mut s.ios)?.iter().any(|d| d.udid.eq_ignore_ascii_case(query));
    if found { Ok(()) } else { Err(format!("no device matches `{query}`")) }
}

/// A listing from the iOS backend (empty where there is none).
fn ios_list<T: serde::de::DeserializeOwned>(s: &mut Session, method: &str) -> Result<Vec<T>, String> {
    serde_json::from_value(crate::ios::call(&mut s.ios, method, &json!({}))?).map_err(|e| e.to_string())
}

/// Dispatches one method.
pub fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let p = if p.is_null() { &Value::Object(Default::default()) } else { p };
    match method {
        "help" => Ok(METHODS
            .iter()
            .map(|(m, params, doc)| {
                let platforms: Vec<&str> = platforms(m).iter().map(|p| p.as_str()).collect();
                json!({"method": m, "params": params, "doc": doc, "platforms": platforms})
            })
            .collect()),
        "identity" => Ok(crate::identity().clone()),
        "form_factors" => {
            let (platform, form_factor) = (opt_platform(p)?, opt_form_factor(p)?);
            Ok(matrix()
                .into_iter()
                .filter(|(f, pl, _)| platform.is_none_or(|x| x == *pl) && form_factor.is_none_or(|x| x == *f))
                .map(|(f, pl, kind)| match kind {
                    Ok(k) => {
                        let inputs: Vec<Value> = k.inputs().iter().map(|i| json!({"input": i, "label": i.label(pl)})).collect();
                        json!({"form_factor": f, "platform": pl, "supported": true, "sub_platform": k.sub_platform, "label": k.label(),
                            "inputs": inputs, "profile": k.profile()})
                    }
                    Err(e) => json!({"form_factor": f, "platform": pl, "supported": false, "reason": e.reason}),
                })
                .collect())
        }
        "devices" => {
            let (platform, form_factor) = (opt_platform(p)?, opt_form_factor(p)?);
            let booted = p.get("booted").and_then(Value::as_bool).unwrap_or(false);
            let mut list: Vec<DeviceInfo> = if platform == Some(Platform::Android) {
                Vec::new()
            } else {
                serde_json::from_value(crate::ios::call(&mut s.ios, method, p)?).map_err(|e| e.to_string())?
            };
            if platform != Some(Platform::Ios) {
                list.extend(s.android.devices().into_iter().filter(|d| !booted || d.state == sim_core::device::DeviceState::Booted));
            }
            let keep = |d: &DeviceInfo| platform.is_none_or(|x| x == d.platform) && form_factor.is_none_or(|x| d.form_factor == Some(x));
            serde_json::to_value(list.into_iter().filter(keep).collect::<Vec<_>>()).map_err(|e| e.to_string())
        }
        "runtimes" => {
            let platform = opt_platform(p)?;
            let mut list: Vec<RuntimeInfo> = if platform == Some(Platform::Android) { Vec::new() } else { ios_list(s, method)? };
            if platform != Some(Platform::Ios) {
                list.extend(s.android.backend().runtimes());
            }
            serde_json::to_value(list).map_err(|e| e.to_string())
        }
        "create" => {
            // Unsupported pairs are rejected before any backend runs.
            let platform = opt_platform(p)?.unwrap_or(Platform::Ios);
            let form_factor = opt_form_factor(p)?;
            if let Some(f) = form_factor {
                Kind::new(platform, f).map_err(|e| e.to_string())?;
            }
            match platform {
                Platform::Ios => crate::ios::call(&mut s.ios, method, p),
                Platform::Android => {
                    let device = crate::params::str_param(p, "device");
                    let info =
                        s.android.backend().create_device(need_str(p, "name")?, need_str(p, "system_image")?, form_factor, device)?;
                    serde_json::to_value(info).map_err(|e| e.to_string())
                }
            }
        }
        // Need no device: resolving the default `booted` would fail with two simulators booted.
        "device_types" | "audio_capture_stop" => crate::ios::call(&mut s.ios, method, p),
        "boot_many" => {
            // One platform per call: each backend boots its own devices its own way.
            let mut udids = Vec::new();
            let mut first: Option<(Platform, String)> = None;
            for q in crate::params::strings(p, "devices") {
                let (platform, udid) = resolve(s, &json!({"device": q, "platform": p.get("platform")}))?;
                match &first {
                    None => first = Some((platform, q.clone())),
                    Some((other, q0)) if *other != platform => {
                        ios_device_exists(s, platform, &q)?;
                        ios_device_exists(s, *other, q0)?;
                        return Err(format!(
                            "`boot_many` boots one platform's devices per call: `{q0}` is an {} device and `{q}` an {} one",
                            other.label(),
                            platform.label()
                        ));
                    }
                    Some(_) => {}
                }
                udids.push(udid);
            }
            match first.map(|(platform, _)| platform) {
                Some(Platform::Android) => crate::android::boot_many(&mut s.android, p, &udids),
                _ => {
                    let mut p = p.clone();
                    p["devices"] = udids.into_iter().map(Value::String).collect();
                    crate::ios::call(&mut s.ios, method, &p)
                }
            }
        }
        m if METHODS.iter().any(|(name, ..)| *name == m) => {
            let (platform, udid) = resolve(s, p)?;
            if !platforms(m).contains(&platform) {
                ios_device_exists(s, platform, str_param(p, "device").unwrap_or("booted"))?;
                let on: Vec<&str> = platforms(m).iter().map(|p| p.label()).collect();
                return Err(format!("`{m}` isn't available on {} devices (it works on {})", platform.label(), on.join(", ")));
            }
            match platform {
                Platform::Ios => {
                    let mut p = p.clone();
                    p["device"] = Value::String(udid);
                    crate::ios::call(&mut s.ios, m, &p)
                }
                Platform::Android => crate::android::call(&mut s.android, m, p, &udid),
            }
        }
        other => Err(format!("unknown method `{other}`; call `help`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_labels_every_method_with_platforms() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        let find = |m: &str| help.as_array().and_then(|a| a.iter().find(|x| x["method"] == m)).cloned().expect(m);
        assert_eq!(find("devices")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("status_bar")["platforms"], json!(["ios"]));
        assert_eq!(find("tap")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("fold")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("xr")["platforms"], json!(["android"]));
        assert_eq!(find("rotary")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("rotate")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("carplay")["platforms"], json!(["ios"]));
        assert_eq!(find("watch_gesture")["platforms"], json!(["ios"]));
        assert_eq!(find("pointer")["platforms"], json!(["ios"]));
        assert_eq!(find("stylus")["platforms"], json!(["android", "ios"]));
        assert!(help.as_array().expect("array").iter().all(|m| m["method"] != "pencil"), "`stylus` is the one pen method");
        assert_eq!(find("vehicle")["platforms"], json!(["android"]));
        assert!(help.as_array().expect("array").iter().all(|m| m["platforms"].as_array().is_some_and(|p| !p.is_empty())));
    }

    /// An SDK with a mobile, a Wear OS and an Android XR image, and an empty AVD home.
    fn android_session(tag: &str) -> (Session, std::path::PathBuf) {
        android_session_in(&std::env::temp_dir(), tag)
    }

    /// [`android_session`] under `/tmp`, for a fake emulator: its display socket's path must fit
    /// a unix socket address (104 bytes on macOS).
    fn short_android_session(tag: &str) -> (Session, std::path::PathBuf) {
        android_session_in(std::path::Path::new("/tmp"), tag)
    }

    fn android_session_in(base: &std::path::Path, tag: &str) -> (Session, std::path::PathBuf) {
        let root = base.join(format!("sim-api-android-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        for (dir, image_tag) in [
            ("android-36/google_apis/arm64-v8a", "google_apis"),
            ("android-34/android-wear/arm64-v8a", "android-wear"),
            ("android-34/google-xr/arm64-v8a", "android-xr"),
        ] {
            let d = sdk.join("system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=36\nSystemImage.TagId={image_tag}\n"))
                .expect("props");
        }
        let env = emu_avd::Env { sdk_root: Some(sdk), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        (Session::with_android(android), root)
    }

    #[test]
    fn android_devices_are_created_listed_and_routed() {
        let (mut s, root) = android_session("route");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        let fold =
            call(&mut s, "create", &json!({"platform": "android", "name": "fold", "system_image": mobile, "form_factor": "foldable"}))
                .expect("foldable");
        assert_eq!((fold["platform"].as_str(), fold["form_factor"].as_str()), (Some("android"), Some("foldable")));
        let wear = "system-images;android-34;android-wear;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "watch", "system_image": wear})).expect("watch");
        let err = call(&mut s, "create", &json!({"platform": "android", "name": "t", "system_image": wear, "form_factor": "tablet"}));
        assert!(err.expect_err("tablet from Wear").contains("boots Wear OS · Watch devices, not tablet"));
        let err = call(&mut s, "create", &json!({"platform": "android", "name": "v", "system_image": mobile, "form_factor": "vision"}));
        assert!(err.expect_err("vision from a phone image").ends_with("boots Android · Phone devices, not vision"));

        let devices = call(&mut s, "devices", &json!({"platform": "android"})).expect("devices");
        let labels: Vec<(&str, &str)> = devices
            .as_array()
            .expect("array")
            .iter()
            .map(|d| (d["udid"].as_str().unwrap_or(""), d["sub_platform"].as_str().unwrap_or("")))
            .collect();
        assert_eq!(labels, [("fold", "android"), ("watch", "wearos")]);
        // Android only: a macOS host lists its real Apple Watch simulators as watches too.
        let watches = call(&mut s, "devices", &json!({"form_factor": "watch", "platform": "android"})).expect("watches");
        assert_eq!(watches.as_array().map(Vec::len), Some(1));
        assert_eq!(call(&mut s, "devices", &json!({"booted": true, "platform": "android"})).expect("booted"), json!([]));
        let rts = call(&mut s, "runtimes", &json!({"platform": "android"})).expect("runtimes");
        assert_eq!(rts.as_array().map(Vec::len), Some(3));

        assert_eq!(call(&mut s, "screen", &json!({"device": "watch"})).expect("screen"), json!({"width": 454, "height": 454}));
        let info = call(&mut s, "info", &json!({"device": "fold"})).expect("info");
        assert_eq!(info["profile"]["fold"]["angle"], 180.0);
        let err = call(&mut s, "status_bar", &json!({"device": "watch"})).expect_err("iOS-only");
        assert_eq!(err, "`status_bar` isn't available on Android devices (it works on iOS)");
        let err = call(&mut s, "fold", &json!({"device": "watch", "posture": "closed"})).expect_err("not a foldable");
        assert!(err.starts_with("folding needs a device with Hinge; watch is Wear OS · Watch"), "{err}");
        assert_eq!(
            call(&mut s, "fold", &json!({"device": "fold", "posture": "ajar"})).expect_err("posture"),
            "unknown posture `ajar` (closed, half-opened, opened, flipped, tent)"
        );
        assert_eq!(call(&mut s, "rotate", &json!({"device": "fold"})).expect_err("stopped"), "fold isn't running; boot it first");
        let err = call(&mut s, "rotate", &json!({"device": "fold", "orientation": "portrait"})).expect_err("iOS-only orientation");
        assert!(err.contains("`orientation` is iOS-only"), "{err}");
        let err = call(&mut s, "tap", &json!({"device": "fold", "x": 1, "y": 1, "display": "carplay"})).expect_err("no CarPlay");
        assert!(err.starts_with("display=carplay is an iPhone's CarPlay screen"), "{err}");
        let err = call(&mut s, "carplay", &json!({"device": "fold", "on": true})).expect_err("iOS-only");
        assert_eq!(err, "`carplay` isn't available on Android devices (it works on iOS)");
        assert!(
            call(&mut s, "tap", &json!({"device": "no-such-device", "x": 1, "y": 1})).expect_err("missing").contains("no device matches")
        );
        call(&mut s, "delete", &json!({"device": "watch"})).expect("delete");
        assert_eq!(call(&mut s, "devices", &json!({"platform": "android"})).expect("devices").as_array().map(Vec::len), Some(1));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_simulator_udid_is_routed_without_listing_either_platform() {
        let (mut s, root) = android_session("udid");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "phone", "system_image": mobile})).expect("phone");
        // With an AVD around, a name is resolved across both platforms; a UDID goes straight to
        // the iOS backend, which looks it up (here it would find none: the routing is the test).
        let udid = "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";
        assert_eq!(resolve(&mut s, &json!({"device": udid})), Ok((Platform::Ios, udid.to_owned())));
        // An AVD whose name has a UDID's shape is still that AVD, and `platform` still decides.
        let avd = "1111aaaa-bbbb-cccc-dddd-eeeeffff1111";
        call(&mut s, "create", &json!({"platform": "android", "name": avd, "system_image": mobile})).expect("uuid-named avd");
        assert_eq!(resolve(&mut s, &json!({"device": avd})), Ok((Platform::Android, avd.to_owned())));
        assert!(resolve(&mut s, &json!({"device": udid, "platform": "android"})).is_err(), "no such AVD");
        // A mistyped UDID with AVDs around says "no device matches", not a platform mismatch.
        let missing = format!("no device matches `{udid}`");
        let err = call(&mut s, "boot_many", &json!({"devices": ["phone", udid]})).expect_err("mixed");
        assert_eq!(err, missing);
        let err = call(&mut s, "boot_many", &json!({"devices": [udid, "phone"]})).expect_err("mixed");
        assert_eq!(err, missing);
        assert_eq!(call(&mut s, "fold", &json!({"device": udid, "posture": "closed"})).expect_err("no such device"), missing);
        assert_eq!(call(&mut s, "console", &json!({"device": udid})).expect_err("no such device"), missing);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn running_android_devices_are_driven_through_every_method() {
        use base64::Engine;
        use sim_android::testing::{fake_emulator, wait_for};
        use sim_core::form_factor::FormFactor;
        // Under /tmp, not $TMPDIR: the fake display socket's path must fit a unix socket address
        // (104 bytes on macOS).
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-live-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("create");
        let fake = fake_emulator(&android, "fold");
        let mut s = Session::with_android(android);
        let ok = |s: &mut Session, method: &str, params: Value| call(s, method, &params).unwrap_or_else(|e| panic!("{method}: {e}"));

        ok(&mut s, "tap", json!({"device": "fold", "x": 10, "y": 20}));
        ok(&mut s, "touch", json!({"device": "fold", "x": 5, "y": 6, "phase": "down"}));
        ok(&mut s, "button", json!({"device": "fold", "button": "home"}));
        ok(&mut s, "key", json!({"device": "fold", "usage": 0x28}));
        ok(&mut s, "location", json!({"device": "fold", "latitude": 37.5, "longitude": -122.25}));
        ok(&mut s, "rotate", json!({"device": "fold"}));
        assert_eq!(ok(&mut s, "fold", json!({"device": "fold", "posture": "half-opened"}))["posture"], "half-opened");
        assert_eq!(ok(&mut s, "fold", json!({"device": "fold", "angle": 170}))["angle"], 170.0);
        assert_eq!(ok(&mut s, "console", json!({"device": "fold", "command": "ping"})), json!(["I am alive!"]));
        assert_eq!(ok(&mut s, "console", json!({"device": "fold", "commands": ["frames", "ping"]})), json!(["3", "I am alive!"]));
        assert_eq!(call(&mut s, "console", &json!({"device": "fold"})).expect_err("no command"), "give `command` or `commands`");

        let shot = ok(&mut s, "screenshot", json!({"device": "fold"}));
        assert_eq!((shot["width"].as_u64(), shot["height"].as_u64()), (Some(2), Some(1)));
        let png = base64::engine::general_purpose::STANDARD.decode(shot["png"].as_str().expect("png")).expect("base64");
        assert_eq!(sim_core::frame::png_size(&png), Some((2, 1)), "the emulator's PNG, passed through");
        let path = root.join("shot.png");
        let saved = ok(&mut s, "screenshot", json!({"device": "fold", "path": path.to_str()}));
        assert_eq!(saved["width"], 2);
        assert!(std::fs::read(&path).expect("written by the emulator").starts_with(b"\x89PNG"));

        let console = wait_for(&fake.console, 13);
        for want in
            ["event send EV_KEY:KEY_HOME:1", "event send EV_KEY:KEY_HOME:0", "event send EV_KEY:28:1", "geo fix -122.25 37.5", "rotate"]
        {
            assert!(console.iter().any(|l| l == want), "{want} not in {console:?}");
        }
        assert!(console.iter().any(|l| l.starts_with("sensor set hinge-angle0 ")), "{console:?}");
        assert!(console.iter().any(|l| l == "posture 2") && console.iter().any(|l| l == "fold status"), "postures as stock: {console:?}");
        assert_eq!(wait_for(&fake.display, 3)[..3], ["touch 10 20 down", "touch 10 20 up", "touch 5 6 down"]);

        // Form-factor checks happen before anything reaches the emulator.
        let err = |s: &mut Session, method: &str, params: Value| call(s, method, &params).expect_err(method);
        assert!(err(&mut s, "rotary", json!({"device": "fold", "ticks": 1})).starts_with("rotary input needs a device with Rotary"));
        assert!(err(&mut s, "vehicle", json!({"device": "fold"})).contains("vehicle HAL"));
        assert!(err(&mut s, "touchpad", json!({"device": "fold", "x": 1, "y": 1})).contains("no touchpad"));
        assert_eq!(err(&mut s, "rotary", json!({"device": "fold"})), "missing number parameter `ticks`");
        assert_eq!(err(&mut s, "key", json!({"device": "fold"})), "missing number parameter `usage`");
        assert!(err(&mut s, "fold", json!({"device": "fold"})).starts_with("give `folded`"));
        assert!(err(&mut s, "fold", json!({"device": "fold", "folded": null})).starts_with("give `folded`"), "null is absent");
        assert_eq!(err(&mut s, "fold", json!({"device": "fold", "folded": "yes"})), "`folded` must be true or false, not \"yes\"");
        assert!(err(&mut s, "rotate", json!({"device": "fold", "orientation": "portrait"})).contains("iOS-only"));
        std::fs::remove_dir_all(root).ok();
    }

    /// One running Pixel Fold AVD named `fold` on a fake emulator (stock's or simee's), under /tmp
    /// (the fake display socket's path must fit a unix socket address).
    fn running_foldable(tag: &str, stock: bool) -> (Session, sim_android::testing::FakeEmulator, std::path::PathBuf) {
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        android.create_device("fold", "system-images;android-36;google_apis;arm64-v8a", None, Some("pixel_fold")).expect("create");
        let fake = match stock {
            true => sim_android::testing::fake_stock_emulator(&android, "fold"),
            false => sim_android::testing::fake_emulator(&android, "fold"),
        };
        (Session::with_android(android), fake, root)
    }

    #[test]
    fn a_stock_run_foldable_folds_without_simees_status() {
        let (mut s, fake, root) = running_foldable("stockfold", true);
        let fold = |s: &mut Session, p: Value| call(s, "fold", &p);
        // Stock refuses simee's `fold status` (`KO: Usage: "fold"`): the answer is what was asked.
        assert_eq!(fold(&mut s, json!({"device": "fold", "folded": true})), Ok(json!({"folded": true})));
        assert_eq!(fold(&mut s, json!({"device": "fold", "posture": "opened"})).expect("posture")["posture"], "opened");
        assert_eq!(fold(&mut s, json!({"device": "fold", "angle": 90})).expect("angle")["angle"], 90.0);
        let console = sim_android::testing::wait_for(&fake.console, 3);
        assert_eq!(console, ["fold", "posture 3", "sensor set hinge-angle0 90"], "stock's own commands only");
        assert_eq!(call(&mut s, "screen", &json!({"device": "fold"})), Ok(json!({"width": 2208, "height": 1840})));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folded_foldable_takes_points_in_the_guests_layout() {
        use sim_android::testing::wait_for;
        let (mut s, fake, root) = running_foldable("folded", false);
        let ok = |s: &mut Session, method: &str, params: Value| call(s, method, &params).unwrap_or_else(|e| panic!("{method}: {e}"));
        // Unfolded (simee's status: the whole screen), points pass through.
        *fake.fold_status.lock().expect("status") = Some(
            "angle: 180\r\nposture: opened (3)\r\nfolded: false\r\nvisible: 2208x1840+0+0\r\nsize: 2208x1840\r\npostures: 1,2,3".into(),
        );
        assert_eq!(ok(&mut s, "screen", json!({"device": "fold"})), json!({"width": 2208, "height": 1840}));
        ok(&mut s, "touch", json!({"device": "fold", "x": 1000, "y": 1000, "phase": "down"}));
        assert_eq!(wait_for(&fake.display, 1), ["touch 1000 1000 down"]);
        // Folded, the guest lays out for the outer screen (uiautomator's bounds) while the frames
        // and the display socket's touches are its 949x1840 region of the scanout.
        *fake.fold_status.lock().expect("status") =
            Some("angle: 0\r\nposture: closed (1)\r\nfolded: true\r\nvisible: 949x1840+629+0\r\nsize: 1080x2092\r\npostures: 1,2,3".into());
        assert_eq!(ok(&mut s, "screen", json!({"device": "fold"})), json!({"width": 1080, "height": 2092}));
        ok(&mut s, "tap", json!({"device": "fold", "x": 1080, "y": 2092, "hold_ms": 0}));
        ok(&mut s, "touch", json!({"device": "fold", "x": 540, "y": 1046, "phase": "up"}));
        assert_eq!(
            wait_for(&fake.display, 4)[1..],
            ["touch 949 1840 down", "touch 949 1840 up", "touch 474 920 up"],
            "whole pixels on the wire"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// Per-call cost of a device method among many AVDs (the API resolves `device` on every call).
    ///     cargo test --release -p sim-api api_call_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn api_call_latency() {
        use sim_android::testing::fake_emulator;
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-bench-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        for i in 0..20 {
            android.create(&format!("avd{i:02}"), "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        }
        let _fake = fake_emulator(&android, "avd07");
        let mut s = Session::with_android(android);
        let n = 200;
        for (method, params) in
            [("screen", json!({"device": "avd07"})), ("touch", json!({"device": "avd07", "x": 1, "y": 2, "phase": "down"}))]
        {
            let start = std::time::Instant::now();
            for _ in 0..n {
                call(&mut s, method, &params).expect(method);
            }
            println!("api {method:<6}: {:.3} ms per call (20 AVDs)", start.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// An `adb` in the session's SDK that says every device has booted (no real adb runs).
    fn fake_adb(root: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join("sdk/platform-tools");
        std::fs::create_dir_all(&dir).expect("platform-tools");
        let adb = dir.join("adb");
        std::fs::write(&adb, "#!/bin/sh\necho 1\n").expect("adb");
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn boot_many_and_clone_work_on_both_platforms_and_settle_on_ios() {
        let (mut s, root) = android_session("boot-many");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        // A listing resolves no device (it resolved the default `booted`, which fails with two booted).
        assert!(call(&mut s, "device_types", &Value::Null).is_ok_and(|t| t.is_array()));
        let help = call(&mut s, "help", &Value::Null).expect("help");
        let row = |m: &str| help.as_array().and_then(|a| a.iter().find(|x| x["method"] == m)).cloned().expect(m);
        for m in ["boot_many", "clone"] {
            assert_eq!(row(m)["platforms"], json!(["android", "ios"]), "{m}");
        }
        for m in ["settle", "profile"] {
            assert_eq!(row(m)["platforms"], json!(["ios"]), "{m}");
        }
        let err = call(&mut s, "settle", &json!({"device": "pixel"})).expect_err("iOS-only");
        assert_eq!(err, "`settle` isn't available on Android devices (it works on iOS)");
        let err = call(&mut s, "profile", &json!({"device": "pixel", "name": "slim"})).expect_err("iOS-only");
        assert_eq!(err, "`profile` isn't available on Android devices (it works on iOS)");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_boot_many_checks_its_parameters_before_starting_anything() {
        let (mut s, root) = android_session("boot-many-params");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let many = |extra: Value| {
            let mut p = json!({"devices": ["pixel"]});
            p.as_object_mut().expect("object").extend(extra.as_object().expect("object").clone());
            p
        };
        for (extra, want) in [
            (json!({"settle": true}), "`boot_many` takes `settle` for iOS devices only (a settle wait reads a simulator's process tree"),
            (json!({"profile": "slim"}), "`boot_many` takes `profile` for iOS devices only"),
            (json!({"env": {"A": "1"}}), "`boot_many` takes `env` for iOS devices only"),
            (json!({"concurrency": 0}), "concurrency must be at least 1"),
            (json!({"first_port": 80}), "first_port must be a console port from 5554 to 5682"),
        ] {
            let err = call(&mut s, "boot_many", &many(extra.clone())).expect_err("refused");
            assert!(err.starts_with(want), "{extra}: {err}");
        }
        let err = call(&mut s, "boot_many", &json!({"devices": ["pixel", "nope"]})).expect_err("unknown");
        assert_eq!(err, "no device matches `nope`");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "settle": true})).expect_err("iOS-only parameter");
        assert!(err.starts_with("`boot` takes `settle` for iOS devices only"), "{err}");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "profile": "slim"})).expect_err("iOS-only parameter");
        assert!(err.starts_with("`boot` takes `profile` for iOS devices only"), "{err}");
        let err = call(&mut s, "boot", &json!({"device": "pixel", "audio": "yes"})).expect_err("not a boolean");
        assert!(err.starts_with("`audio` must be true or false"), "{err}");
        let boot = METHODS.iter().find(|m| m.0 == "boot").expect("boot");
        assert!(boot.1.contains("Android: audio?(false)"), "{}", boot.1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_boot_many_reports_each_device_like_ios() {
        use sim_android::testing::fake_emulator;
        let (mut s, root) = short_android_session("bm-run");
        fake_adb(&root);
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        // Running already: boot_many waits for it (console and adb answer) and starts nothing.
        let _fake = fake_emulator(s.android.backend(), "pixel");
        let r = call(&mut s, "boot_many", &json!({"devices": ["pixel"], "concurrency": 2, "timeout_s": 20})).expect("boot_many");
        assert_eq!((r["concurrency"].as_u64(), r["failed"].as_u64(), r["unsettled"].as_u64()), (Some(2), Some(0), Some(0)), "{r}");
        let d = &r["devices"][0];
        assert_eq!(d["device"], "pixel");
        let port = d["console_port"].as_u64().expect("port");
        assert_eq!(d["serial"], json!(format!("emulator-{port}")));
        assert!(d["boot_s"].as_f64().is_some_and(|s| s < 20.0), "{d}");
        assert!(d.get("error").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn boot_many_takes_one_platform_per_call() {
        let (mut s, root) = android_session("boot-many-mixed");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "pixel", "system_image": mobile})).expect("pixel");
        let ios = crate::ios::selection_infos(&mut s.ios).unwrap_or_default();
        if let Some(sim) = ios.first() {
            let err = call(&mut s, "boot_many", &json!({"devices": ["pixel", sim.udid]})).expect_err("mixed");
            assert_eq!(
                err,
                format!("`boot_many` boots one platform's devices per call: `pixel` is an Android device and `{}` an iOS one", sim.udid)
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_clones_come_from_a_stopped_golden() {
        let (mut s, root) = short_android_session("clone");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "golden", "system_image": mobile})).expect("golden");
        let err = call(&mut s, "clone", &json!({"device": "golden", "name": "farm-1"})).expect_err("never booted");
        assert_eq!(err, "golden has no simee user data yet (boot it once with emu, then stop it)");
        // As if it had booted under simee once and stopped.
        let disks = emu_avd::state_dir(&root.join("avd/golden.avd"));
        std::fs::create_dir_all(&disks).expect("simee");
        std::fs::write(disks.join("userdata.img"), b"settled").expect("userdata");
        let clone = call(&mut s, "clone", &json!({"device": "golden", "name": "farm-1"})).expect("clone");
        assert_eq!((clone["udid"].as_str(), clone["platform"].as_str()), (Some("farm-1"), Some("android")));
        assert_eq!(std::fs::read(emu_avd::state_dir(&root.join("avd/farm-1.avd")).join("userdata.img")).expect("cloned"), b"settled");
        assert!(call(&mut s, "clone", &json!({"device": "golden"})).expect_err("no name").contains("name"));
        // A running golden is refused before anything is copied.
        let _fake = sim_android::testing::fake_emulator(s.android.backend(), "golden");
        let err = call(&mut s, "clone", &json!({"device": "golden", "name": "farm-2"})).expect_err("running");
        assert_eq!(err, "golden must be shut down to clone it");
        call(&mut s, "delete", &json!({"device": "farm-1"})).expect("delete the clone");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn android_xr_devices_take_xr_controls() {
        let (mut s, root) = android_session("xr");
        let xr = "system-images;android-34;google-xr;arm64-v8a";
        let headset = call(&mut s, "create", &json!({"platform": "android", "name": "headset", "system_image": xr})).expect("headset");
        assert_eq!((headset["form_factor"].as_str(), headset["sub_platform"].as_str()), (Some("vision"), Some("androidxr")));
        let wear = "system-images;android-34;android-wear;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "watch", "system_image": wear})).expect("watch");
        assert_eq!(call(&mut s, "screen", &json!({"device": "headset"})).expect("screen"), json!({"width": 2560, "height": 2558}));
        let visions = call(&mut s, "devices", &json!({"form_factor": "vision", "platform": "android"})).expect("visions");
        assert_eq!(visions.as_array().map(Vec::len), Some(1));

        let rotate = json!({"device": "headset", "action": "rotate", "x": 0, "y": 0.5, "z": 0});
        assert_eq!(call(&mut s, "xr", &rotate).expect_err("stopped"), "headset isn't running; boot it first");
        let err = call(&mut s, "xr", &json!({"device": "watch", "action": "recenter"})).expect_err("not XR");
        assert_eq!(err, "XR controls need an Android XR device; watch is Wear OS · Watch");
        let err = call(&mut s, "xr", &json!({"device": "headset", "action": "teleport"})).expect_err("unknown action");
        assert!(err.starts_with("xr: unknown variant `teleport`"), "{err}");
        let err = call(&mut s, "xr", &json!({"device": "headset", "action": "options", "environment": "day"})).expect_err("missing");
        assert!(err.contains("missing field `passthrough`"), "{err}");

        // The headset has no touchpad (the display glasses do); parameters are checked first.
        let mut pad = |p: Value| call(&mut s, "touchpad", &p);
        let err = pad(json!({"device": "headset", "x": 1, "y": 2})).expect_err("no touchpad");
        assert!(err.starts_with("headset has no touchpad"), "{err}");
        assert_eq!(pad(json!({"device": "headset", "x": 1})).expect_err("y"), "missing number parameter `y`");
        assert_eq!(pad(json!({"device": "headset", "x": -1, "y": 2})).expect_err("negative"), "`x` must be a whole number");
        assert_eq!(
            pad(json!({"device": "headset", "x": 1, "y": 2, "to_x": 3})).expect_err("half a swipe"),
            "a swipe needs both `to_x` and `to_y`"
        );
        assert_eq!(
            pad(json!({"device": "headset", "x": 1, "y": 2, "phase": "hold"})).expect_err("phase"),
            "unknown phase `hold` (down, move, up, tap)"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn vehicle_is_for_automotive_devices() {
        let (mut s, root) = android_session("vehicle");
        let car = root.join("sdk/system-images/android-34/android-automotive/arm64-v8a");
        std::fs::create_dir_all(&car).expect("image dir");
        std::fs::write(car.join("system.img"), b"s").expect("system.img");
        std::fs::write(car.join("source.properties"), "AndroidVersion.ApiLevel=34\nSystemImage.TagId=android-automotive\n").expect("props");
        let made = call(
            &mut s,
            "create",
            &json!({"platform": "android", "name": "car", "system_image": "system-images;android-34;android-automotive;arm64-v8a"}),
        )
        .expect("car");
        assert_eq!(made["form_factor"], "auto");
        let mobile = "system-images;android-36;google_apis;arm64-v8a";
        call(&mut s, "create", &json!({"platform": "android", "name": "phone", "system_image": mobile})).expect("phone");

        let err = call(&mut s, "vehicle", &json!({"device": "phone"})).expect_err("not a car");
        assert!(err.starts_with("the vehicle HAL needs a device with Automotive head unit; phone is Android · Phone"), "{err}");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "warp", "value": 1})).expect_err("unknown property");
        assert!(err.contains("unknown vehicle property `warp`"), "{err}");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "gear", "value": "fast"})).expect_err("bad value");
        assert!(err.contains("isn't a Int32 value for GEAR_SELECTION"), "{err}");
        assert_eq!(call(&mut s, "vehicle", &json!({"device": "car", "value": 1})).expect_err("no property"), "give the `property` to set");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "gear", "value": "drive"})).expect_err("stopped");
        assert_eq!(err, "car isn't running; boot it first");
        let err = call(&mut s, "vehicle", &json!({"device": "car", "property": "speed", "area_id": [1]})).expect_err("bad area");
        assert_eq!(err, "`area_id` must be a 32-bit number");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn form_factors_lists_the_matrix_with_reasons() {
        let mut s = Session::default();
        let all = call(&mut s, "form_factors", &Value::Null).expect("matrix");
        assert_eq!(all.as_array().map(Vec::len), Some(14));
        let fold = call(&mut s, "form_factors", &json!({"form_factor": "foldable"})).expect("foldable");
        let ios = fold.as_array().and_then(|a| a.iter().find(|x| x["platform"] == "ios")).expect("ios row");
        assert_eq!(ios["supported"], false);
        assert!(ios["reason"].as_str().is_some_and(|r| r.contains("Android-only")));
        let watch = call(&mut s, "form_factors", &json!({"form_factor": "watch", "platform": "ios"})).expect("watch");
        assert_eq!(watch[0]["sub_platform"], "watchos");
        assert_eq!(watch[0]["inputs"][1]["label"], "Digital Crown");
        assert_eq!(watch[0]["profile"]["shape"], "rounded");
        let xr = call(&mut s, "form_factors", &json!({"form_factor": "vision", "platform": "android"})).expect("Android XR");
        assert_eq!((xr[0]["supported"].as_bool(), xr[0]["sub_platform"].as_str()), (Some(true), Some("androidxr")));
        assert_eq!(xr[0]["inputs"][0]["label"], "Eye tracking");
        assert_eq!((xr[0]["profile"]["width"].as_u64(), xr[0]["profile"]["height"].as_u64()), (Some(2560), Some(2558)));
        let unsupported: Vec<&Value> = all.as_array().expect("rows").iter().filter(|r| r["supported"] == false).collect();
        assert_eq!(unsupported.len(), 1, "a foldable iPhone is the only gap");
        let err = call(&mut s, "form_factors", &json!({"form_factor": "laptop"})).expect_err("unknown");
        assert!(err.contains("phone, tablet, foldable, watch, tv, auto, vision"), "{err}");
        assert!(call(&mut s, "devices", &json!({"platform": "windows"})).expect_err("unknown").contains("android or ios"));
        let err = call(&mut s, "create", &json!({"name": "x", "device_type": "y", "runtime": "z", "form_factor": "foldable"}));
        assert!(err.expect_err("no foldable iPhone").starts_with("foldable is not supported on iOS"));
    }

    /// The parameters a dispatcher's "missing" error names: "missing number parameter `x`",
    /// "xr: missing field `action`", "give `angle` (0-180) or `posture`". Empty for any other error.
    fn missing_names(error: &str) -> Vec<String> {
        let tail = if let Some(at) = error.find("missing ") {
            &error[at..]
        } else if error.starts_with("give `") {
            error
        } else {
            return Vec::new();
        };
        let mut ticks = tail.split('`').skip(1).step_by(2).map(str::to_owned);
        if tail.starts_with("give") { ticks.collect() } else { ticks.next().into_iter().collect() }
    }

    /// A value of the type the schema gives a parameter, to get past its "missing" error.
    fn dummy(schema: &Value) -> Value {
        if let Some(first) = schema["enum"].get(0) {
            return first.clone();
        }
        match schema["type"].as_str() {
            Some("number" | "integer") => json!(1),
            Some("boolean") => json!(true),
            Some("array") => json!(["a"]),
            Some("object") => json!({}),
            _ => json!("a"),
        }
    }

    /// What probing one method on one dispatcher found.
    #[derive(Debug, PartialEq)]
    enum Probe {
        /// The dispatcher checked this method's parameters: every parameter it reported missing is
        /// required or a member of a group of alternatives, a call with the schema's required
        /// fields (and one member of each group it asked for) wasn't refused for a missing one,
        /// and dropping any required field made it say that field was missing (or the call worked).
        Probed,
        /// The dispatcher refused before it read parameters (no such hardware, a platform or form
        /// factor it doesn't serve, a state): nothing is known.
        Unprobed(String),
    }

    /// Drives `method` on the Android dispatcher, on the running fake device `device`, from the
    /// schema's required fields.
    fn probe_android(s: &mut Session, method: &str, device: &str) -> Probe {
        let schema = crate::input_schema(method).expect("schema");
        let members: Vec<String> = crate::schema::one_of(method).into_iter().flatten().collect();
        let required: Vec<String> =
            schema["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
        let mut params = json!({});
        if schema["properties"].get("device").is_some() {
            params["device"] = json!(device);
        }
        for name in required.iter().filter(|n| *n != "device") {
            params[name] = dummy(&schema["properties"][name]);
        }
        // Give the parameters the dispatcher asks for beyond the required ones: each must be a member of
        // a group of alternatives, or the schema leaves out something every call needs.
        let mut added: Vec<String> = Vec::new();
        let mut last = call(s, method, &params);
        for _ in 0..8 {
            let Err(error) = &last else { break };
            let names = missing_names(error);
            let fresh: Vec<&String> = names.iter().filter(|n| params.get(n.as_str()).is_none()).collect();
            let Some(&first) = fresh.first() else { break };
            for name in &names {
                assert!(
                    required.contains(name) || members.contains(name),
                    "`{method}` refuses a call with its required fields {required:?} for a missing `{name}` ({error}): the schema must require it or group it"
                );
            }
            params[first] = dummy(&schema["properties"][first]);
            added.push(first.clone());
            last = call(s, method, &params);
        }
        for group in crate::schema::one_of(method) {
            let given = added.iter().filter(|n| group.contains(n)).count();
            assert!(given <= 1, "`{method}` needed more than one of {group:?}: they are not alternatives");
        }
        assert!(added.iter().all(|n| members.contains(n)), "`{method}`: {added:?}");
        let refused = last
            .as_ref()
            .err()
            .is_some_and(|e| !missing_names(e).is_empty() && missing_names(e).iter().all(|n| params.get(n.as_str()).is_none()));
        assert!(!refused, "`{method}` still refuses {params} for a missing parameter: {last:?}");
        // Drop each required field in turn: the dispatcher must say it is missing, or (when the whole
        // call was refused before parameters) tell nothing.
        let mut confirmed = 0;
        let others: Vec<&String> = required.iter().filter(|n| *n != "device").collect();
        for name in &others {
            let mut without = params.clone();
            without.as_object_mut().map(|o| o.remove(name.as_str()));
            match call(s, method, &without) {
                Ok(_) => panic!("`{method}` works without `{name}`, but its schema requires it"),
                Err(error) if missing_names(&error).contains(name) => confirmed += 1,
                Err(_) => {}
            }
        }
        if last.is_ok() || (!others.is_empty() && confirmed == others.len()) {
            Probe::Probed
        } else {
            Probe::Unprobed(format!("{:?}", last.err()))
        }
    }

    /// Methods the Android dispatcher serves that this test must not run, and why.
    const NOT_RUN_ON_ANDROID: &[(&str, &str)] = &[
        ("boot", "starts or stops emulators: the fake's advertisement names this process"),
        ("boot_many", "starts emulators"),
        ("shutdown", "signals the advertised process, which is this one"),
        ("delete", "refuses a running device before it reads parameters"),
        ("create", "its required parameters depend on `platform`, and it creates a device"),
        ("clone", "creates a device from a golden that has booted"),
    ];

    /// The parameters an iOS dispatcher arm reads with `need_*` or reports missing, found in its
    /// source: the iOS backend only builds on macOS, so on other hosts this is the nearest thing to
    /// running it. Arms that hand `p` to a helper (`tap_samples(p)`) read nothing here; the Android
    /// probe covers the helpers the platforms share.
    fn ios_arm_reads(source: &str, method: &str) -> Vec<String> {
        let quoted = format!("\"{method}\"");
        let starts_arm = |line: &str| line.starts_with("        \"") && line.contains("=>") && !line.starts_with("         ");
        let mut reads = Vec::new();
        let mut in_arm = false;
        for line in source.lines() {
            if starts_arm(line) {
                in_arm = line.split("=>").next().is_some_and(|names| names.split('|').any(|n| n.trim() == quoted));
            } else if line.starts_with("        _ =>") || line.starts_with("        other =>") || line == "    }" {
                in_arm = false;
            }
            if !in_arm {
                continue;
            }
            for marker in ["need_str(p, \"", "need_f64(p, \"", "need_bool(p, \"", "parameter `", "missing parameter `"] {
                let mut rest = line;
                while let Some(at) = rest.find(marker) {
                    rest = &rest[at + marker.len()..];
                    let end = rest.find(['"', '`']).unwrap_or(rest.len());
                    let name = rest[..end].to_owned();
                    if !name.is_empty() && !reads.contains(&name) {
                        reads.push(name);
                    }
                }
            }
        }
        reads
    }

    /// Every method's schema agrees with its dispatchers about what is required:
    ///
    /// - Android: each method its dispatcher serves is driven on a running fake device, in both
    ///   directions ([`Probe::Probed`]). A method the dispatcher refuses before reading parameters
    ///   isn't counted as probed; each such one is listed here with why, and a method that stops
    ///   needing to be listed fails until it is taken off.
    /// - iOS: its dispatcher only builds on macOS, and the Linux stub refuses every device method
    ///   before reading parameters, so the iOS arms' own `need_*` reads are scanned from the source:
    ///   each must be required or in a group of alternatives in the schema.
    #[test]
    fn the_schema_requires_what_the_dispatchers_require() {
        use sim_android::testing::fake_emulator;
        use sim_core::form_factor::FormFactor;
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-schema-{}", std::process::id()));
        let sdk = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sdk).expect("image dir");
        std::fs::write(sdk.join("system.img"), b"s").expect("system.img");
        std::fs::write(sdk.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        // A watch and a car: the methods that need their hardware are refused on a foldable before
        // their parameters are read.
        for (dir, tag, build_prop) in [
            ("android-34/android-wear/arm64-v8a", "android-wear", ""),
            ("android-34-ext9/android-automotive/arm64-v8a", "android-automotive", "ro.product.name=sdk_car_arm64\n"),
        ] {
            let d = root.join("sdk/system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=34\nSystemImage.TagId={tag}\n")).expect("props");
            if !build_prop.is_empty() {
                std::fs::write(d.join("build.prop"), build_prop).expect("build.prop");
            }
        }
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("create");
        android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        android.create("car", "system-images;android-34-ext9;android-automotive;arm64-v8a", None).expect("car");
        // One fake emulator is advertised at a time (its advertisement is named for this process).
        let mut fakes = vec![fake_emulator(&android, "fold")];
        let mut advertised = "fold";
        let backend = android.clone();
        let mut s = Session::with_android(android);

        let (mut probed, mut unprobed) = (Vec::new(), Vec::new());
        for (method, ..) in METHODS {
            if !platforms(method).contains(&Platform::Android) || NOT_RUN_ON_ANDROID.iter().any(|(m, _)| m == method) {
                continue;
            }
            let device = match *method {
                "rotary" => "watch",
                "vehicle" => "car",
                _ => "fold",
            };
            if device != advertised {
                fakes.push(fake_emulator(&backend, device));
                advertised = device;
            }
            match probe_android(&mut s, method, device) {
                Probe::Probed => probed.push(*method),
                Probe::Unprobed(why) => unprobed.push((*method, why)),
            }
        }
        assert!(
            unprobed.is_empty(),
            "the Android dispatcher refused these before reading their parameters, so nothing is known: {unprobed:?}"
        );
        // The probe reaches parameter checks (a method it didn't would be in `unprobed`).
        for method in [
            "tap",
            "swipe",
            "touch",
            "button",
            "type",
            "key",
            "location",
            "rotate",
            "rotary",
            "fold",
            "console",
            "touchpad",
            "stylus",
            "xr",
            "vehicle",
            "screen",
            "screenshot",
            "info",
            "devices",
        ] {
            assert!(probed.contains(&method), "{method} should be probed: {probed:?}");
        }
        for (method, _) in NOT_RUN_ON_ANDROID {
            assert!(METHODS.iter().any(|(m, ..)| m == method), "{method} is not a method");
        }

        // The iOS dispatcher's reads, from its source.
        let source = include_str!("ios.rs");
        let mut scanned = 0;
        for (method, ..) in METHODS {
            if !platforms(method).contains(&Platform::Ios) {
                continue;
            }
            let schema = crate::input_schema(method).expect("schema");
            let members: Vec<String> = crate::schema::one_of(method).into_iter().flatten().collect();
            let reads = ios_arm_reads(source, method);
            scanned += usize::from(!reads.is_empty());
            for name in reads.iter().filter(|n| *n != "device") {
                let required = schema["required"].as_array().is_some_and(|r| r.iter().any(|v| v == name.as_str()));
                // A parameter only iOS reads (`create`'s `device_type`) is optional in the schema and says so.
                let ios_only = schema["properties"][name]["description"].as_str().is_some_and(|d| d.contains("iOS only"));
                assert!(
                    required || members.contains(name) || ios_only,
                    "the iOS `{method}` arm reads `{name}` as required, but its schema doesn't ({reads:?})"
                );
            }
        }
        // The scan finds real reads: `launch`'s bundle id, `defaults_write`'s key.
        assert!(ios_arm_reads(source, "launch").contains(&"bundle_id".to_owned()));
        assert!(ios_arm_reads(source, "defaults_write").contains(&"domain".to_owned()));
        assert!(scanned >= 20, "{scanned} methods with reads");
        std::fs::remove_dir_all(root).ok();
    }
}
