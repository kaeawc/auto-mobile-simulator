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
        "Boot and wait until finished (env: the device launchd's environment; shaped: inject the network shaper)",
    ),
    ("shutdown", "device", "Shut down"),
    ("erase", "device", "Erase content and settings (shut down first)"),
    ("screen", "device", "Screen size in pixels"),
    ("screenshot", "device, path?", "PNG to path, else base64 in `png`"),
    ("tap", "device, x, y, hold_ms?", "Tap at pixels"),
    ("swipe", "device, x1, y1, x2, y2, duration_ms?, settle?", "Swipe between pixels"),
    ("touch", "device, x, y, phase(down|move|up)", "One raw touch sample"),
    ("button", "device, button(home|lock|side|siri|apple-pay), hold_ms?", "Press a hardware button"),
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
    ("rotate", "device", "Rotate the device 90° clockwise"),
    ("fold", "device, angle(0-180) | posture(closed|half-opened|opened)", "Set a foldable's hinge"),
    ("rotary", "device, ticks", "Turn a watch's rotary input (crown or bezel)"),
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
];
/// Methods only Android devices have.
const ANDROID_ONLY: &[&str] = &["console", "rotate", "fold", "rotary"];

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
        assert!(help.as_array().expect("array").iter().all(|m| m["platforms"].as_array().is_some_and(|p| !p.is_empty())));
    }

    /// An SDK with a mobile and a Wear OS image, and an empty AVD home.
    fn android_session(tag: &str) -> (Session, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("sim-api-android-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        for (dir, image_tag) in [("android-36/google_apis/arm64-v8a", "google_apis"), ("android-34/android-wear/arm64-v8a", "android-wear")]
        {
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
        assert!(err.expect_err("vision").starts_with("vision is not supported on Android"));

        let devices = call(&mut s, "devices", &json!({"platform": "android"})).expect("devices");
        let labels: Vec<(&str, &str)> = devices
            .as_array()
            .expect("array")
            .iter()
            .map(|d| (d["udid"].as_str().unwrap_or(""), d["sub_platform"].as_str().unwrap_or("")))
            .collect();
        assert_eq!(labels, [("fold", "android"), ("watch", "wearos")]);
        assert_eq!(call(&mut s, "devices", &json!({"form_factor": "watch"})).expect("watches").as_array().map(Vec::len), Some(1));
        assert_eq!(call(&mut s, "devices", &json!({"booted": true, "platform": "android"})).expect("booted"), json!([]));
        let rts = call(&mut s, "runtimes", &json!({"platform": "android"})).expect("runtimes");
        assert_eq!(rts.as_array().map(Vec::len), Some(2));

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
        assert!(
            call(&mut s, "tap", &json!({"device": "no-such-device", "x": 1, "y": 1})).expect_err("missing").contains("no device matches")
        );
        call(&mut s, "delete", &json!({"device": "watch"})).expect("delete");
        assert_eq!(call(&mut s, "devices", &json!({"platform": "android"})).expect("devices").as_array().map(Vec::len), Some(1));
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
        let err = call(&mut s, "form_factors", &json!({"form_factor": "laptop"})).expect_err("unknown");
        assert!(err.contains("phone, tablet, foldable, watch, tv, auto, vision"), "{err}");
        assert!(call(&mut s, "devices", &json!({"platform": "windows"})).expect_err("unknown").contains("android or ios"));
        let err = call(&mut s, "create", &json!({"name": "x", "device_type": "y", "runtime": "z", "form_factor": "foldable"}));
        assert!(err.expect_err("no foldable iPhone").starts_with("foldable is not supported on iOS"));
    }
}
