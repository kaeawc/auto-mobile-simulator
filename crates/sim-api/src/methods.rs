//! The method table, parameter parsing and dispatch. Device methods are served by the iOS
//! backend (`ios.rs`), which exists only on macOS hosts.

use serde_json::{Value, json};
use sim_core::device::DeviceInfo;
use sim_core::form_factor::{Kind, matrix};
use sim_core::platform::Platform;

use crate::params::{opt_form_factor, opt_platform};

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
    ("runtimes", "", "List installed runtimes"),
    ("device_types", "", "List device types"),
    ("create", "name, device_type, runtime, form_factor?", "Create a device (form_factor: checked against the device type)"),
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
];

/// Per-connection state: each backend's service connection, created on first use.
#[derive(Default)]
pub struct Session {
    pub(crate) ios: crate::ios::Session,
}

/// Methods that work on every platform's devices (or need no device).
const ALL_PLATFORMS: &[&str] = &["help", "form_factors", "devices"];

/// The platforms whose devices `method` works on.
pub fn platforms(method: &str) -> &'static [Platform] {
    if ALL_PLATFORMS.contains(&method) { &Platform::ALL } else { &[Platform::Ios] }
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
            let list: Vec<DeviceInfo> = serde_json::from_value(crate::ios::call(&mut s.ios, method, p)?).map_err(|e| e.to_string())?;
            let keep = |d: &DeviceInfo| platform.is_none_or(|x| x == d.platform) && form_factor.is_none_or(|x| d.form_factor == Some(x));
            serde_json::to_value(list.into_iter().filter(keep).collect::<Vec<_>>()).map_err(|e| e.to_string())
        }
        "create" => {
            // Unsupported pairs are rejected before any backend runs. Devices are created on iOS.
            if let Some(form_factor) = opt_form_factor(p)? {
                Kind::new(Platform::Ios, form_factor).map_err(|e| e.to_string())?;
            }
            crate::ios::call(&mut s.ios, method, p)
        }
        m if METHODS.iter().any(|(name, ..)| *name == m) => crate::ios::call(&mut s.ios, m, p),
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
        assert!(help.as_array().expect("array").iter().all(|m| m["platforms"].as_array().is_some_and(|p| !p.is_empty())));
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
