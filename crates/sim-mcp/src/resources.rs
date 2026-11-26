//! Resources: what a client can read without calling a tool. Devices, runtimes and form factors
//! as JSON; for each device its info, its screenshot, and on iOS its logs and apps; and the device
//! view's page. Each is a read of the same API method a tool calls.

use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::app;
use crate::backend::Backend;
use crate::rpc::{INTERNAL_ERROR, RpcError};

const SCHEME: &str = "simee://";
pub const DEVICES: &str = "simee://devices";
pub const RUNTIMES: &str = "simee://runtimes";
pub const DEVICE_TYPES: &str = "simee://device-types";
pub const FORM_FACTORS: &str = "simee://form-factors";

/// What a resource URI names.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Devices,
    Runtimes,
    DeviceTypes,
    FormFactors,
    /// `simee://devices/{device}`: the device's info.
    Device(String),
    /// `simee://devices/{device}/screenshot`
    Screenshot(String),
    /// `simee://devices/{device}/logs`
    Logs(String),
    /// `simee://devices/{device}/apps`
    Apps(String),
    /// The device view's page.
    View,
}

/// Percent-encodes everything but unreserved characters, so a device name is one path segment.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn device_uri(device: &str, leaf: &str) -> String {
    let base = format!("{DEVICES}/{}", encode(device));
    if leaf.is_empty() { base } else { format!("{base}/{leaf}") }
}

/// The target a URI names, or `None` for one that isn't this server's.
pub fn parse(uri: &str) -> Option<Target> {
    if uri == app::URI {
        return Some(Target::View);
    }
    let path = uri.strip_prefix(SCHEME)?;
    match path {
        "devices" => return Some(Target::Devices),
        "runtimes" => return Some(Target::Runtimes),
        "device-types" => return Some(Target::DeviceTypes),
        "form-factors" => return Some(Target::FormFactors),
        _ => {}
    }
    let mut parts = path.strip_prefix("devices/")?.split('/');
    let device = decode(parts.next().filter(|d| !d.is_empty())?)?;
    match (parts.next(), parts.next()) {
        (None, _) => Some(Target::Device(device)),
        (Some("screenshot"), None) => Some(Target::Screenshot(device)),
        (Some("logs"), None) => Some(Target::Logs(device)),
        (Some("apps"), None) => Some(Target::Apps(device)),
        _ => None,
    }
}

/// `resources/templates/list`: the per-device resources as URI templates.
pub fn templates() -> Value {
    json!([
        {"uriTemplate": "simee://devices/{device}", "name": "device", "title": "Device info", "mimeType": "application/json",
         "description": "A device's paths, identity and profile (the `info` method). `device` is a UDID or an AVD name."},
        {"uriTemplate": "simee://devices/{device}/screenshot", "name": "screenshot", "title": "Screenshot", "mimeType": "image/png",
         "description": "The device's screen now, as a PNG (a booted device)."},
        {"uriTemplate": "simee://devices/{device}/logs", "name": "logs", "title": "Device logs", "mimeType": "text/plain",
         "description": "The last minute of the device's unified log (the `logs` method; iOS)."},
        {"uriTemplate": "simee://devices/{device}/apps", "name": "apps", "title": "Installed apps", "mimeType": "application/json",
         "description": "The device's installed bundle ids (the `apps` method; iOS)."},
    ])
}

/// `resources/list`: the fixed resources, then each device's. A screenshot is listed for a booted
/// device; logs and apps for an iOS one (Android's console is the `console` tool).
pub fn list(backend: &dyn Backend) -> Vec<Value> {
    let mut out = vec![
        json!({"uri": DEVICES, "name": "devices", "title": "Devices", "mimeType": "application/json",
            "description": "Every device on both platforms: UDID, name, state, platform, form factor."}),
        json!({"uri": RUNTIMES, "name": "runtimes", "title": "Runtimes", "mimeType": "application/json",
            "description": "Installed runtimes: iOS runtimes and Android system images."}),
        json!({"uri": DEVICE_TYPES, "name": "device-types", "title": "Device types", "mimeType": "application/json",
            "description": "iOS device types a simulator can be created from."}),
        json!({"uri": FORM_FACTORS, "name": "form-factors", "title": "Form factors", "mimeType": "application/json",
            "description": "Every form factor on every platform, with its inputs, or why it is unsupported."}),
        app::listing(),
    ];
    let devices = backend.call("devices", &json!({})).unwrap_or(Value::Null);
    for d in devices.as_array().into_iter().flatten() {
        let (Some(udid), name) = (d["udid"].as_str(), d["name"].as_str().unwrap_or("device")) else { continue };
        let platform = d["platform"].as_str().unwrap_or("");
        let booted = d["state"] == "Booted";
        let label = |what: &str| format!("{name} {what} ({platform})");
        out.push(json!({"uri": device_uri(udid, ""), "name": label("info"), "mimeType": "application/json",
            "description": format!("{name}: state {}", d["state"].as_str().unwrap_or("unknown"))}));
        if booted {
            out.push(json!({"uri": device_uri(udid, "screenshot"), "name": label("screenshot"), "mimeType": "image/png"}));
        }
        if platform == "ios" {
            if booted {
                out.push(json!({"uri": device_uri(udid, "logs"), "name": label("logs"), "mimeType": "text/plain"}));
            }
            out.push(json!({"uri": device_uri(udid, "apps"), "name": label("apps"), "mimeType": "application/json"}));
        }
    }
    out
}

/// A method's failure as a resource error: a device or a resource its platform lacks is not found;
/// anything else (a device that isn't running) is the server's to report.
fn failed(uri: &str, message: String) -> RpcError {
    if message.contains("no device matches") || message.contains("isn't available on") {
        RpcError::invalid_params("Resource not found").with_data(json!({"uri": uri, "reason": message}))
    } else {
        RpcError::new(INTERNAL_ERROR, message).with_data(json!({"uri": uri}))
    }
}

fn json_text(uri: &str, value: &Value) -> Value {
    json!({"uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(value).unwrap_or_default()})
}

/// `resources/read`: the contents of `uri`.
pub fn read(backend: &dyn Backend, uri: &str) -> Result<Vec<Value>, RpcError> {
    let target = parse(uri).ok_or_else(|| RpcError::invalid_params("Resource not found").with_data(json!({"uri": uri})))?;
    let call = |method: &str, params: Value| backend.call(method, &params).map_err(|e| failed(uri, e));
    let content = match target {
        Target::View => app::contents(),
        Target::Devices => json_text(uri, &call("devices", json!({}))?),
        Target::Runtimes => json_text(uri, &call("runtimes", json!({}))?),
        Target::DeviceTypes => json_text(uri, &call("device_types", json!({}))?),
        Target::FormFactors => json_text(uri, &call("form_factors", json!({}))?),
        Target::Device(d) => json_text(uri, &call("info", json!({"device": d}))?),
        Target::Apps(d) => json_text(uri, &call("apps", json!({"device": d}))?),
        Target::Screenshot(d) => {
            let shot = call("screenshot", json!({"device": d}))?;
            let png = shot["png"].as_str().ok_or_else(|| RpcError::new(INTERNAL_ERROR, "the screenshot has no image"))?;
            json!({"uri": uri, "mimeType": "image/png", "blob": png})
        }
        Target::Logs(d) => {
            let logs = call("logs", json!({"device": d}))?;
            let text = match &logs {
                Value::String(s) => s.clone(),
                Value::Array(lines) if lines.iter().all(Value::is_string) => {
                    lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")
                }
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            json!({"uri": uri, "mimeType": "text/plain", "text": text})
        }
    };
    Ok(vec![content])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::Fake;

    #[test]
    fn uris_round_trip_and_junk_is_refused() {
        for (device, leaf, target) in [
            ("phone", "", Target::Device("phone".into())),
            ("0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "screenshot", Target::Screenshot("0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000".into())),
            ("My Phone/2", "logs", Target::Logs("My Phone/2".into())),
            ("é", "apps", Target::Apps("é".into())),
        ] {
            let uri = device_uri(device, leaf);
            assert!(!uri[SCHEME.len()..].contains(' ') && uri.matches('/').count() == 3 + usize::from(!leaf.is_empty()), "{uri}");
            assert_eq!(parse(&uri), Some(target), "{uri}");
        }
        assert_eq!(parse(DEVICES), Some(Target::Devices));
        assert_eq!(parse(RUNTIMES), Some(Target::Runtimes));
        assert_eq!(parse(DEVICE_TYPES), Some(Target::DeviceTypes));
        assert_eq!(parse(FORM_FACTORS), Some(Target::FormFactors));
        assert_eq!(parse(app::URI), Some(Target::View));
        for junk in [
            "",
            "simee://",
            "simee://devices/",
            "simee://devices/x/nope",
            "simee://devices/x/logs/more",
            "simee://devices/%zz",
            "file:///x",
            "simee://other",
        ] {
            assert_eq!(parse(junk), None, "{junk}");
        }
    }

    #[test]
    fn the_list_has_the_fixed_resources_and_each_devices() {
        let fake = Fake::with_devices();
        let list = list(&fake);
        let uris: Vec<&str> = list.iter().filter_map(|r| r["uri"].as_str()).collect();
        for fixed in [DEVICES, RUNTIMES, DEVICE_TYPES, FORM_FACTORS, app::URI] {
            assert!(uris.contains(&fixed), "{fixed}");
        }
        // The booted Android device: info and screenshot, no iOS-only logs or apps.
        assert!(uris.contains(&"simee://devices/phone") && uris.contains(&"simee://devices/phone/screenshot"));
        assert!(!uris.contains(&"simee://devices/phone/logs") && !uris.contains(&"simee://devices/phone/apps"));
        // The shut-down iPhone: info and apps, but nothing that needs it running.
        let udid = "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";
        assert!(
            uris.contains(&format!("simee://devices/{udid}").as_str()) && uris.contains(&format!("simee://devices/{udid}/apps").as_str())
        );
        assert!(
            !uris.contains(&format!("simee://devices/{udid}/screenshot").as_str())
                && !uris.contains(&format!("simee://devices/{udid}/logs").as_str())
        );
        assert!(list.iter().all(|r| r["name"].is_string() && r["uri"].is_string()));
        // No devices (or a failing listing) is just the fixed ones.
        let none = Fake::default();
        none.answer("devices", Err("boom".into()));
        assert_eq!(super::list(&none).len(), 5);
    }

    #[test]
    fn templates_cover_the_per_device_resources() {
        let templates = templates();
        let names: Vec<&str> = templates.as_array().expect("array").iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["device", "screenshot", "logs", "apps"]);
        for t in templates.as_array().expect("array") {
            let uri = t["uriTemplate"].as_str().expect("template").replace("{device}", "d");
            assert!(parse(&uri).is_some(), "{uri}");
        }
    }

    #[test]
    fn reading_calls_the_method_and_shapes_the_contents() {
        let fake = Fake::with_devices();
        fake.answer("runtimes", Ok(json!([{"name": "Android 16"}])));
        fake.answer("info", Ok(json!({"udid": "phone", "profile": {}})));
        fake.answer("screenshot", Ok(json!({"width": 2, "height": 1, "png": "iVBOR"})));
        fake.answer("logs", Ok(json!(["line one", "line two"])));
        fake.answer("apps", Ok(json!(["com.example.app"])));
        let read1 = |uri: &str| read(&fake, uri).expect(uri).remove(0);
        let devices = read1(DEVICES);
        assert_eq!(devices["mimeType"], "application/json");
        assert!(devices["text"].as_str().is_some_and(|t| t.contains("iPhone 17")));
        assert!(read1(RUNTIMES)["text"].as_str().is_some_and(|t| t.contains("Android 16")));
        assert_eq!(fake.called("info").len(), 0);
        let info = read1("simee://devices/phone");
        assert!(info["text"].as_str().is_some_and(|t| t.contains("profile")));
        assert_eq!(fake.called("info"), [json!({"device": "phone"})]);
        assert_eq!(
            read1("simee://devices/phone/screenshot"),
            json!({"uri": "simee://devices/phone/screenshot", "mimeType": "image/png", "blob": "iVBOR"})
        );
        assert_eq!(read1("simee://devices/phone/logs")["text"], "line one\nline two");
        assert!(read1("simee://devices/phone/apps")["text"].as_str().is_some_and(|t| t.contains("com.example.app")));
        let page = read1(app::URI);
        assert_eq!((page["mimeType"].clone(), page["uri"].clone()), (json!(app::MIME), json!(app::URI)));
        assert_eq!(read1(FORM_FACTORS)["mimeType"], "application/json");
        assert_eq!(read1(DEVICE_TYPES)["mimeType"], "application/json");
        // Logs that aren't lines are shown as JSON.
        fake.answer("logs", Ok(json!({"lines": 2})));
        assert!(read1("simee://devices/phone/logs")["text"].as_str().is_some_and(|t| t.contains("\"lines\": 2")));
    }

    #[test]
    fn a_resource_that_is_not_there_is_invalid_params_and_other_failures_internal() {
        let fake = Fake::default();
        fake.answer("info", Err("no device matches `ghost`".into()));
        fake.answer("logs", Err("`logs` isn't available on Android devices (it works on iOS)".into()));
        fake.answer("screenshot", Err("phone isn't running; boot it first".into()));
        let err = read(&fake, "simee://devices/ghost").expect_err("missing");
        assert_eq!((err.code, err.message.as_str()), (crate::rpc::INVALID_PARAMS, "Resource not found"));
        assert_eq!(err.data.as_ref().map(|d| d["uri"].clone()), Some(json!("simee://devices/ghost")));
        assert_eq!(read(&fake, "simee://devices/phone/logs").expect_err("android").code, crate::rpc::INVALID_PARAMS);
        let down = read(&fake, "simee://devices/phone/screenshot").expect_err("stopped");
        assert_eq!((down.code, down.message.as_str()), (INTERNAL_ERROR, "phone isn't running; boot it first"));
        assert_eq!(read(&fake, "simee://nope").expect_err("not ours").code, crate::rpc::INVALID_PARAMS);
        // A screenshot written to a path has no image to return.
        fake.answer("screenshot", Ok(json!({"path": "/tmp/x.png"})));
        assert_eq!(read(&fake, "simee://devices/phone/screenshot").expect_err("no png").code, INTERNAL_ERROR);
    }
}
