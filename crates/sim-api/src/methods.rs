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
        "form_factors",
        "platform?, form_factor?",
        "Every form factor on every platform: sub-platform, inputs and reference profile, or why it's unsupported",
    ),
    ("devices", "booted?, platform?(android|ios), form_factor?", "List devices with their platform, form factor and sub-platform"),
    ("runtimes", "platform?", "List installed runtimes (Android: system images)"),
    ("device_types", "", "List device types"),
    (
        "create",
        "name, platform?(ios|android), form_factor?; iOS: device_type, runtime; Android: system_image",
        "Create a device. form_factor is checked against the device type (iOS) or the system image (Android) and shapes a new AVD",
    ),
    ("delete", "device", "Delete a device"),
    (
        "boot",
        "device, timeout_s?, env?, shaped?",
        "Boot and wait until finished (iOS: env is the device launchd's environment, shaped injects the network shaper; Android: starts emu)",
    ),
    ("shutdown", "device", "Shut down"),
    ("erase", "device", "Erase content and settings (shut down first)"),
    ("screen", "device, display?(main|carplay)", "Screen size in pixels"),
    ("screenshot", "device, path?, display?(main|carplay)", "PNG to path, else base64 in `png`"),
    ("tap", "device, x, y, hold_ms?, display?(main|carplay)", "Tap at pixels (display: an iPhone's CarPlay screen, iOS only)"),
    ("swipe", "device, x1, y1, x2, y2, duration_ms?, settle?, display?", "Swipe between pixels"),
    ("touch", "device, x, y, phase(down|move|up), display?", "One raw touch sample"),
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
    ("fold", "device, angle(0-180) | posture(closed|half-opened|opened)", "Set a foldable's hinge"),
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
        "rotary",
        "device, ticks | delta (Digital Crown units; iOS)",
        "Turn a watch's rotary input: the Wear OS crown or bezel, the Apple Watch Digital Crown",
    ),
    (
        "vehicle",
        "device, property?(VHAL name or id: GEAR_SELECTION, gear, speed, 0x11600207, ...), area_id?, value?",
        "Android Automotive's vehicle HAL: list every property's value, read one, or set one when `value` is given \
         (typed by the property: drive, 12.5, true); through the emulator console's `car` command (emu-rs)",
    ),
    ("watch_gesture", "device, gesture(double-tap|wrist-flick)", "An Apple Watch hand gesture"),
    ("remote_swipe", "device, direction(up|down|left|right), duration_ms?", "Swipe on the Siri Remote's touch surface (Apple TV)"),
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
    "form_factors",
    "devices",
    "runtimes",
    "create",
    "delete",
    "boot",
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
    "rotary",
];
/// Methods only Android devices have.
const ANDROID_ONLY: &[&str] = &["console", "fold", "xr", "vehicle", "touchpad"];

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
    let android = if only == Some(Platform::Ios) { Vec::new() } else { s.android.devices() };
    if android.is_empty() && only != Some(Platform::Android) {
        return Ok((Platform::Ios, query.to_owned()));
    }
    let mut all = if only == Some(Platform::Android) { Vec::new() } else { ios_list(s, "devices")? };
    all.extend(android);
    match select(&all, query) {
        Selection::One(i) => Ok((all[i].platform, all[i].udid.clone())),
        Selection::None => Err(format!("no device matches `{query}`")),
        Selection::Ambiguous => Err(format!("`{query}` matches several devices; use a UDID or `platform`")),
    }
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
                    let info = s.android.backend().create(need_str(p, "name")?, need_str(p, "system_image")?, form_factor)?;
                    serde_json::to_value(info).map_err(|e| e.to_string())
                }
            }
        }
        m if METHODS.iter().any(|(name, ..)| *name == m) => {
            let (platform, udid) = resolve(s, p)?;
            if !platforms(m).contains(&platform) {
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
        assert_eq!(find("fold")["platforms"], json!(["android"]));
        assert_eq!(find("xr")["platforms"], json!(["android"]));
        assert_eq!(find("rotary")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("rotate")["platforms"], json!(["android", "ios"]));
        assert_eq!(find("carplay")["platforms"], json!(["ios"]));
        assert_eq!(find("watch_gesture")["platforms"], json!(["ios"]));
        assert_eq!(find("vehicle")["platforms"], json!(["android"]));
        assert!(help.as_array().expect("array").iter().all(|m| m["platforms"].as_array().is_some_and(|p| !p.is_empty())));
    }

    /// An SDK with a mobile, a Wear OS and an Android XR image, and an empty AVD home.
    fn android_session(tag: &str) -> (Session, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("sim-api-android-{tag}-{}", std::process::id()));
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
            "unknown posture `ajar` (closed, half-opened, opened)"
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
        assert_eq!(wait_for(&fake.display, 3)[..3], ["touch 10 20 down", "touch 10 20 up", "touch 5 6 down"]);

        // Form-factor checks happen before anything reaches the emulator.
        let err = |s: &mut Session, method: &str, params: Value| call(s, method, &params).expect_err(method);
        assert!(err(&mut s, "rotary", json!({"device": "fold", "ticks": 1})).starts_with("rotary input needs a device with Rotary"));
        assert!(err(&mut s, "vehicle", json!({"device": "fold"})).contains("vehicle HAL"));
        assert!(err(&mut s, "touchpad", json!({"device": "fold", "x": 1, "y": 1})).contains("no touchpad"));
        assert_eq!(err(&mut s, "rotary", json!({"device": "fold"})), "missing number parameter `ticks`");
        assert_eq!(err(&mut s, "key", json!({"device": "fold"})), "missing number parameter `usage`");
        assert!(err(&mut s, "fold", json!({"device": "fold"})).starts_with("give `angle`"));
        assert!(err(&mut s, "rotate", json!({"device": "fold", "orientation": "portrait"})).contains("iOS-only"));
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
}
