//! sim-rs: create, boot and drive iOS simulator devices and Android emulator devices.
//!
//! Every device command is a call into the API's method table (`sim-api`), the same code the
//! socket server and the desktop window run; this file only parses arguments and prints results.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sim_core::device::DeviceInfo;

#[derive(Parser)]
#[command(name = "sim-rs", version, about = "Create, boot and drive iOS simulator and Android emulator devices")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List devices.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Only booted devices.
        #[arg(long)]
        booted: bool,
        /// Only this platform's devices: android or ios.
        #[arg(long)]
        platform: Option<String>,
        /// Only this form factor: phone, tablet, foldable, watch, tv, auto, vision.
        #[arg(long)]
        form_factor: Option<String>,
    },
    /// Every form factor on every platform: sub-platform and inputs, or why it's unsupported.
    FormFactors {
        #[arg(long)]
        json: bool,
        /// Only this platform: android or ios.
        #[arg(long)]
        platform: Option<String>,
    },
    /// List installed runtimes.
    Runtimes {
        #[arg(long)]
        json: bool,
    },
    /// List device types.
    DeviceTypes {
        #[arg(long)]
        json: bool,
    },
    /// Create a device; prints its UDID (an Android device's UDID is its AVD name).
    ///
    /// iOS: `create NAME DEVICE_TYPE RUNTIME`. Android: `create NAME --platform android
    /// --system-image system-images;android-36;google_apis;arm64-v8a`.
    Create {
        name: String,
        /// iOS device type (name or identifier).
        device_type: Option<String>,
        /// iOS runtime (name or identifier).
        runtime: Option<String>,
        /// ios (default) or android.
        #[arg(long)]
        platform: Option<String>,
        /// Android system image, as an SDK package or a path.
        #[arg(long)]
        system_image: Option<String>,
        /// Fail unless the device type or system image is this form factor; shapes a new AVD.
        #[arg(long)]
        form_factor: Option<String>,
    },
    /// Delete a device and its data.
    Delete { device: String },
    /// Boot a device and wait until it finishes booting.
    Boot {
        device: String,
        /// Environment for the device's launchd, `KEY=VALUE` (repeatable).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Inject the network shaper into every process; shape later with `network_shape`.
        #[arg(long)]
        shaped: bool,
        /// Seconds to wait for the first frame.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Shut a device down.
    Shutdown { device: String },
    /// Erase all content and settings (device must be shut down).
    Erase { device: String },
    /// Save the screen as a PNG.
    Screenshot {
        device: String,
        path: String,
        /// `carplay` for an iPhone's CarPlay screen.
        #[arg(long, default_value = "main")]
        display: String,
    },
    /// Tap at a point, in pixels from the top-left of the screen.
    Tap {
        device: String,
        x: f64,
        y: f64,
        /// Hold time in milliseconds.
        #[arg(long, default_value_t = 60)]
        hold: u64,
        /// `carplay` for an iPhone's CarPlay screen.
        #[arg(long, default_value = "main")]
        display: String,
    },
    /// Swipe between two points, in pixels.
    Swipe {
        device: String,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        /// Duration in milliseconds.
        #[arg(long, default_value_t = 300)]
        duration: u64,
        /// Pause at the end before lifting (a drag rather than a flick).
        #[arg(long)]
        settle: bool,
        /// `carplay` for an iPhone's CarPlay screen.
        #[arg(long, default_value = "main")]
        display: String,
    },
    /// Press a hardware button: home, lock (power), side, siri, apple-pay, action (iOS); back,
    /// volume-up, volume-down, recents, menu (Android); up, down, left, right, select, play-pause
    /// (TV remotes: the Siri Remote on Apple TV, where menu and back are its Menu button).
    Button {
        device: String,
        button: String,
        /// Hold time in milliseconds.
        #[arg(long, default_value_t = 80)]
        hold: u64,
    },
    /// Type text on the hardware keyboard (US layout).
    Type { device: String, text: String },
    /// Press a key by HID usage (page 7), e.g. 40 for Return.
    Key { device: String, usage: u32 },
    /// Open a URL.
    OpenUrl { device: String, url: String },
    /// Install an .app bundle.
    Install { device: String, path: String },
    /// Uninstall an app.
    Uninstall { device: String, bundle_id: String },
    /// Launch an app; prints its pid.
    Launch { device: String, bundle_id: String },
    /// Terminate an app.
    Terminate { device: String, bundle_id: String },
    /// List installed bundle identifiers.
    Apps { device: String },
    /// Set light or dark appearance.
    Appearance { device: String, mode: String },
    /// Set the simulated location.
    #[command(allow_negative_numbers = true)]
    Location { device: String, latitude: f64, longitude: f64 },
    /// Grant or revoke a privacy permission for an app (photos, camera, contacts, all, ...).
    Privacy { device: String, action: String, service: String, bundle_id: String },
    /// Simulate a memory warning.
    MemoryWarning { device: String },
    /// Rotate a device 90° clockwise, or (iOS) to an orientation.
    Rotate {
        device: String,
        /// portrait, landscape-right, portrait-upside-down or landscape-left (iOS).
        #[arg(long)]
        orientation: Option<String>,
    },
    /// Set an Android foldable's hinge: a posture (closed, half-opened, opened) or an angle (0-180).
    Fold { device: String, posture: String },
    /// Turn a watch's crown (Wear OS rotary input, Apple Watch Digital Crown) by some ticks
    /// (negative turns back).
    #[command(allow_negative_numbers = true)]
    Rotary {
        device: String,
        ticks: i64,
        /// Apple Watch: the turn in Digital Crown units instead of ticks.
        #[arg(long)]
        delta: Option<f64>,
    },
    /// An Apple Watch hand gesture: double-tap or wrist-flick.
    WatchGesture { device: String, gesture: String },
    /// Swipe on the Siri Remote's touch surface (Apple TV): up, down, left or right.
    RemoteSwipe { device: String, direction: String },
    /// Attach (`on`) or detach (`off`) an iPhone's CarPlay display (iOS 26 runtimes).
    Carplay { device: String, state: String },
    /// Send commands to an Android device's emulator console (`adb emu`'s protocol).
    Console {
        device: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Serve the JSON-lines API on a unix socket (default `$SIM_RS_SOCKET` or `$TMPDIR/sim-rs.sock`).
    Serve {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Call an API method on a running server, e.g. `sim-rs call processes '{"device":"booted"}'`.
    Call {
        method: String,
        /// JSON parameters.
        #[arg(default_value = "{}")]
        params: String,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Run a program inside the device (e.g. `defaults`, `log`, `launchctl`, `notifyutil`, `heap`).
    #[command(disable_help_flag = true)]
    Exec {
        device: String,
        /// Environment for the program, `KEY=VALUE` (repeatable).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Cmd::Serve { socket } => {
            let path = socket.clone().unwrap_or_else(sim_api::default_socket);
            eprintln!("sim-rs API on {}", path.display());
            sim_api::serve(&path)?;
            return Ok(());
        }
        Cmd::Call { method, params, socket } => {
            let path = socket.clone().unwrap_or_else(sim_api::default_socket);
            let params: Value = serde_json::from_str(params).context("params must be JSON")?;
            let result = sim_api::call(&path, method, params).map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        _ => {}
    }
    let mut session = sim_api::Session::default();
    let mut call = |method: &str, params: Value| sim_api::call_method(&mut session, method, &params).map_err(anyhow::Error::msg);
    match cli.command {
        Cmd::List { json, booted, platform, form_factor } => {
            let devices = call("devices", json!({"booted": booted, "platform": platform, "form_factor": form_factor}))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else {
                print!("{}", list_text(&serde_json::from_value::<Vec<DeviceInfo>>(devices)?));
            }
        }
        Cmd::FormFactors { json, platform } => {
            let rows = call("form_factors", json!({"platform": platform}))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", form_factors_text(&rows));
            }
        }
        Cmd::Runtimes { json } => {
            let runtimes = call("runtimes", json!({}))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runtimes)?);
            } else {
                for r in items(&runtimes) {
                    let note = if r["available"] == true { "" } else { " (unavailable)" };
                    println!("{} ({} - {}) - {}{note}", text(&r["name"]), text(&r["version"]), text(&r["build"]), text(&r["identifier"]));
                }
            }
        }
        Cmd::DeviceTypes { json } => {
            let types = call("device_types", json!({}))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&types)?);
            } else {
                for t in items(&types) {
                    println!(
                        "{} ({}) {}x{} @{}x",
                        text(&t["name"]),
                        text(&t["identifier"]),
                        number(&t["screen_width"]),
                        number(&t["screen_height"]),
                        number(&t["screen_scale"])
                    );
                }
            }
        }
        Cmd::Create { name, device_type, runtime, platform, system_image, form_factor } => {
            let params = json!({"name": name, "device_type": device_type, "runtime": runtime, "platform": platform,
                "system_image": system_image, "form_factor": form_factor});
            let d = call("create", params)?;
            println!("{}", text(&d["udid"]));
        }
        Cmd::Delete { device } => drop(call("delete", json!({"device": device}))?),
        Cmd::Boot { device, timeout, env, shaped } => {
            let env: serde_json::Map<String, Value> =
                env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), Value::from(v))).collect();
            call("boot", json!({"device": device, "timeout_s": timeout, "env": env, "shaped": shaped}))?;
        }
        Cmd::Shutdown { device } => drop(call("shutdown", json!({"device": device}))?),
        Cmd::Erase { device } => drop(call("erase", json!({"device": device}))?),
        Cmd::Screenshot { device, path, display } => drop(call("screenshot", json!({"device": device, "path": path, "display": display}))?),
        Cmd::Tap { device, x, y, hold, display } => {
            call("tap", json!({"device": device, "x": x, "y": y, "hold_ms": hold, "display": display}))?;
            flush();
        }
        Cmd::Swipe { device, x1, y1, x2, y2, duration, settle, display } => {
            let params = json!({"device": device, "x1": x1, "y1": y1, "x2": x2, "y2": y2, "duration_ms": duration, "settle": settle,
                "display": display});
            call("swipe", params)?;
            flush();
        }
        Cmd::Button { device, button, hold } => {
            call("button", json!({"device": device, "button": button, "hold_ms": hold}))?;
            flush();
        }
        Cmd::Type { device, text } => {
            call("type", json!({"device": device, "text": text}))?;
            flush();
        }
        Cmd::Key { device, usage } => {
            call("key", json!({"device": device, "usage": usage}))?;
            flush();
        }
        Cmd::OpenUrl { device, url } => drop(call("open_url", json!({"device": device, "url": url}))?),
        Cmd::Install { device, path } => drop(call("install", json!({"device": device, "path": path}))?),
        Cmd::Uninstall { device, bundle_id } => drop(call("uninstall", json!({"device": device, "bundle_id": bundle_id}))?),
        Cmd::Launch { device, bundle_id } => println!("{}", call("launch", json!({"device": device, "bundle_id": bundle_id}))?["pid"]),
        Cmd::Terminate { device, bundle_id } => drop(call("terminate", json!({"device": device, "bundle_id": bundle_id}))?),
        Cmd::Apps { device } => items(&call("apps", json!({"device": device}))?).iter().for_each(|id| println!("{}", text(id))),
        Cmd::Appearance { device, mode } => drop(call("appearance", json!({"device": device, "mode": mode}))?),
        Cmd::Location { device, latitude, longitude } => {
            drop(call("location", json!({"device": device, "latitude": latitude, "longitude": longitude}))?)
        }
        Cmd::Privacy { device, action, service, bundle_id } => {
            drop(call("privacy", json!({"device": device, "action": action, "service": service, "bundle_id": bundle_id}))?)
        }
        Cmd::MemoryWarning { device } => drop(call("memory_warning", json!({"device": device}))?),
        Cmd::Rotate { device, orientation } => {
            let r = call("rotate", json!({"device": device, "orientation": orientation}))?;
            if let Some(o) = r["orientation"].as_str() {
                println!("{o}");
            }
        }
        Cmd::Fold { device, posture } => {
            let params = match posture.parse::<f64>() {
                Ok(angle) => json!({"device": device, "angle": angle}),
                Err(_) => json!({"device": device, "posture": posture}),
            };
            let r = call("fold", params)?;
            println!("{} ({}°)", text(&r["posture"]), r["angle"]);
        }
        Cmd::Rotary { device, ticks, delta } => {
            call("rotary", json!({"device": device, "ticks": ticks, "delta": delta}))?;
            flush();
        }
        Cmd::WatchGesture { device, gesture } => {
            call("watch_gesture", json!({"device": device, "gesture": gesture}))?;
            flush();
        }
        Cmd::RemoteSwipe { device, direction } => {
            call("remote_swipe", json!({"device": device, "direction": direction}))?;
            flush();
        }
        Cmd::Carplay { device, state } => {
            let on = match state.as_str() {
                "on" => true,
                "off" => false,
                other => anyhow::bail!("`{other}`: say on or off"),
            };
            let r = call("carplay", json!({"device": device, "on": on}))?;
            if on {
                println!("CarPlay display {}x{}", number(&r["width"]), number(&r["height"]));
            }
        }
        Cmd::Console { device, command } => {
            items(&call("console", json!({"device": device, "command": command.join(" ")}))?).iter().for_each(|l| println!("{}", text(l)));
        }
        Cmd::Serve { .. } | Cmd::Call { .. } => unreachable!("handled above"),
        Cmd::Exec { device, env, program, args } => {
            let env: serde_json::Map<String, Value> =
                env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), Value::from(v))).collect();
            let out = call("exec", json!({"device": device, "program": program, "args": args, "env": env}))?;
            print!("{}", text(&out["stdout"]));
            eprint!("{}", text(&out["stderr"]));
        }
    }
    Ok(())
}

/// The elements of a JSON array (none for anything else).
fn items(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

/// A JSON string's contents; other values print as JSON.
fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// A JSON number as Rust prints an `f64` (`1206`, not `1206.0`).
fn number(v: &Value) -> f64 {
    v.as_f64().unwrap_or_default()
}

/// `list` without `--json`: devices grouped under their runtime, as `simctl list` prints them, each
/// labelled with its sub-platform and form factor.
fn list_text(devices: &[DeviceInfo]) -> String {
    let mut out = String::new();
    let mut runtime = "";
    for d in devices {
        if d.runtime_id != runtime {
            runtime = &d.runtime_id;
            out.push_str(&format!("-- {runtime} --\n"));
        }
        let note = if d.available { "" } else { " (unavailable)" };
        out.push_str(&format!("    {} ({}) ({}){note} [{}]\n", d.name, d.udid, d.state.as_str(), d.label()));
    }
    out
}

/// `form-factors` without `--json`: one line per form factor and platform.
fn form_factors_text(rows: &Value) -> String {
    let mut out = String::new();
    for r in items(rows) {
        let what = if r["supported"] == true {
            let inputs: Vec<String> = items(&r["inputs"]).iter().map(|i| text(&i["label"])).collect();
            format!("{} (inputs: {})", text(&r["label"]), inputs.join(", "))
        } else {
            format!("unsupported: {}", text(&r["reason"]))
        };
        out.push_str(&format!("{:<9} {:<8} {what}\n", text(&r["form_factor"]), text(&r["platform"])));
    }
    out
}

/// HID messages are delivered asynchronously; give the last one time to leave before exiting.
fn flush() {
    sleep(Duration::from_millis(100));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_groups_devices_by_runtime_with_labels() {
        let devices: Vec<DeviceInfo> = serde_json::from_value(json!([
            {"udid": "A", "name": "iPhone 17", "state": "Booted", "runtime_id": "rt.iOS-26-5", "device_type_id": "t", "available": true,
             "platform": "ios", "form_factor": "phone", "sub_platform": "ios"},
            {"udid": "B", "name": "iPad", "state": "Shutting Down", "runtime_id": "rt.iOS-26-5", "device_type_id": "t", "available": false,
             "platform": "ios", "form_factor": "tablet", "sub_platform": "ipados"},
            {"udid": "C", "name": "Odd", "state": "Shutdown", "runtime_id": "rt.watchOS-26-0", "device_type_id": "t", "available": true,
             "platform": "ios", "form_factor": null, "sub_platform": null},
        ]))
        .expect("devices");
        assert_eq!(
            list_text(&devices),
            "-- rt.iOS-26-5 --\n    iPhone 17 (A) (Booted) [iOS · Phone]\n    iPad (B) (Shutting Down) (unavailable) [iPadOS · Tablet]\n\
             -- rt.watchOS-26-0 --\n    Odd (C) (Shutdown) [iOS]\n"
        );
    }

    #[test]
    fn form_factors_print_support_and_reasons() {
        let mut s = sim_api::Session::default();
        let rows = sim_api::call_method(&mut s, "form_factors", &json!({"platform": "ios"})).expect("rows");
        let out = form_factors_text(&rows);
        assert!(out.contains("watch     ios      watchOS · Watch (inputs: Touch, Digital Crown, Hardware buttons)"), "{out}");
        assert!(out.contains("foldable  ios      unsupported: no Apple device"), "{out}");
        assert_eq!(out.lines().count(), 7);
    }

    #[test]
    fn numbers_print_like_f64() {
        assert_eq!(format!("{}x{} @{}x", number(&json!(1206.0)), number(&json!(2622)), number(&json!(3.0))), "1206x2622 @3x");
    }

    #[test]
    fn every_command_names_a_real_method() {
        for m in ["devices", "runtimes", "device_types", "create", "delete", "boot", "shutdown", "erase", "screenshot", "tap", "swipe"] {
            assert!(sim_api::METHODS.iter().any(|(name, ..)| *name == m), "{m}");
        }
        for m in ["button", "type", "key", "open_url", "install", "uninstall", "launch", "terminate", "apps", "appearance"] {
            assert!(sim_api::METHODS.iter().any(|(name, ..)| *name == m), "{m}");
        }
        for m in [
            "location",
            "privacy",
            "memory_warning",
            "exec",
            "rotate",
            "fold",
            "rotary",
            "console",
            "watch_gesture",
            "remote_swipe",
            "carplay",
        ] {
            assert!(sim_api::METHODS.iter().any(|(name, ..)| *name == m), "{m}");
        }
    }
}
