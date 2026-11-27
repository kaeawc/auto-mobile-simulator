//! The Android backend: device methods on AVDs and running simee instances, through
//! `sim-android`. Builds on every host; booting needs an Apple silicon Mac (the emulator does).

use std::thread::sleep;
use std::time::Duration;

use serde_json::{Value, json};
use sim_android::Android;
use sim_core::device::DeviceInfo;
use sim_core::fold::StockPosture;
use sim_core::form_factor::Posture;
use sim_core::hid::Direction;
use sim_core::xr::XrCommand;

use crate::params::{
    button, need_str, opt_bool, opt_u64, png_result, reading_json, str_param, swipe_samples, tap_samples, touch_sample, vehicle_area,
    vehicle_value,
};

/// The SDK and AVD home, read from the environment on first use.
#[derive(Default)]
pub struct Session {
    android: Option<Android>,
}

impl Session {
    pub fn with(android: Android) -> Session {
        Session { android: Some(android) }
    }

    pub fn backend(&mut self) -> &Android {
        self.android.get_or_insert_with(Android::from_env)
    }

    pub fn devices(&mut self) -> Vec<DeviceInfo> {
        self.backend().devices()
    }
}

/// `boot` and `boot_many` parameters only iOS devices take, and why.
const IOS_BOOT_PARAMS: &[(&str, &str)] = &[
    ("settle", "a settle wait reads a simulator's process tree; an Android boot waits for sys.boot_completed"),
    ("settle_cpu_pct", "it tunes the settle wait"),
    ("settle_window_s", "it tunes the settle wait"),
    ("settle_timeout_s", "it tunes the settle wait"),
    ("profile", "profiles are sets of a simulator's launchd jobs"),
    ("shaped", "the network shaper is injected into a simulator's processes"),
    ("env", "it is a simulator launchd's environment"),
];

/// Rejects iOS-only boot parameters that ask for something (a `false`, `null`, empty or absent
/// one asks for nothing), naming the platform that has them.
fn reject_ios_boot_params(method: &str, p: &Value) -> Result<(), String> {
    for (key, why) in IOS_BOOT_PARAMS {
        let asks = match p.get(*key) {
            None | Some(Value::Null | Value::Bool(false)) => false,
            Some(Value::Object(m)) => !m.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(_) => true,
        };
        if asks {
            return Err(format!("`{method}` takes `{key}` for iOS devices only ({why})"));
        }
    }
    Ok(())
}

/// `fold`: `folded` (the console's `fold` or `unfold`), a hinge `angle`, or a `posture` (stock's
/// names or ids: closed, half-opened, opened, flipped, tent); a `null` one is absent. Answers
/// with the hinge as simee's emulator then has it (angle, posture, whether folded, the display
/// size the guest lays out for), or, from one that doesn't say (the stock emulator), what was
/// asked.
fn fold(d: &sim_android::Device, p: &Value) -> Result<Value, String> {
    const POSTURES: &str = "closed, half-opened, opened, flipped, tent";
    let asked = match (opt_bool(p, "folded")?, p.get("angle").and_then(Value::as_f64), str_param(p, "posture")) {
        (Some(folded), ..) => {
            d.fold_display(folded)?;
            json!({"folded": folded})
        }
        (None, Some(a), _) => {
            d.fold(a as f32)?;
            json!({"angle": a as f32, "posture": Posture::from_angle(a as f32)})
        }
        (None, None, Some(name)) => {
            let posture = StockPosture::parse(name).ok_or_else(|| format!("unknown posture `{name}` ({POSTURES})"))?;
            d.posture(posture)?;
            json!({"angle": Posture::parse(name).map(Posture::angle), "posture": posture.name()})
        }
        (None, None, None) => {
            return Err(format!("give `folded` (true|false), `angle` (0-180; 0-360 on a dual-screen) or `posture` ({POSTURES})"));
        }
    };
    Ok(match d.fold_state()? {
        Some(s) => json!({
            "angle": s.angle,
            "posture": s.posture.name(),
            "folded": s.folded,
            "width": s.width,
            "height": s.height,
            "postures": s.postures.iter().map(|p| p.name()).collect::<Vec<_>>(),
        }),
        None => asked,
    })
}

/// The first console port `boot_many` hands out: `first_port` (default 5554), even and in the
/// emulators' range.
fn first_port(p: &Value) -> Result<u16, String> {
    let ports = sim_android::emu_ports();
    match opt_u64(p, "first_port") {
        None => Ok(*ports.start()),
        Some(n) => u16::try_from(n)
            .ok()
            .filter(|n| ports.contains(n))
            .ok_or_else(|| format!("first_port must be a console port from {} to {}", ports.start(), ports.end())),
    }
}

/// One device's [`sim_android::BootOutcome`] in `boot_many`'s result: the same shape as an iOS
/// device's (`device`, `boot_s`, or `error`), with the console port and adb serial.
fn outcome_json(o: &sim_android::BootOutcome) -> Value {
    let serial = format!("emulator-{}", o.console_port);
    match &o.result {
        Ok(took) => {
            json!({"device": o.avd, "boot_s": (took.as_secs_f64() * 10.0).round() / 10.0, "console_port": o.console_port, "serial": serial})
        }
        Err(e) => json!({"device": o.avd, "console_port": o.console_port, "error": e}),
    }
}

/// `boot_many` for AVDs (`udids`, already resolved): [`Android::boot_many`], at most
/// `concurrency` emulators starting at once, each on its own console port.
pub fn boot_many(s: &mut Session, p: &Value, udids: &[String]) -> Result<Value, String> {
    reject_ios_boot_params("boot_many", p)?;
    if udids.is_empty() {
        return Err("give `devices`: a list of AVD names".into());
    }
    let concurrency = crate::params::concurrency(p)?;
    let timeout = Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300));
    let first = first_port(p)?;
    let start = std::time::Instant::now();
    let outcomes = s.backend().boot_many(udids, concurrency, timeout, first)?;
    let devices: Vec<Value> = outcomes.iter().map(outcome_json).collect();
    let failed = outcomes.iter().filter(|o| o.result.is_err()).count();
    Ok(json!({"concurrency": concurrency, "all_s": (start.elapsed().as_secs_f64() * 10.0).round() / 10.0, "failed": failed,
        "unsettled": 0, "devices": devices}))
}

/// Dispatches one device method for the AVD named `udid`.
pub fn call(s: &mut Session, method: &str, p: &Value, udid: &str) -> Result<Value, String> {
    let android = s.backend().clone();
    let d = android.device(udid)?;
    match crate::params::display(p)? {
        sim_core::duo::Display::Active => {}
        sim_core::duo::Display::CarPlay => {
            return Err("display=carplay is an iPhone's CarPlay screen; Android devices have one display".into());
        }
        sim_core::duo::Display::Builtin(s) => {
            return Err(format!("display={} is an iPhone Duo's screen; an Android foldable shows the screen `fold` sets", s.as_str()));
        }
    }
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "delete" => null(android.delete(&d)),
        "boot" => {
            reject_ios_boot_params(method, p)?;
            // Silent unless asked: a tool or farm booting N devices would open N host streams.
            let audio = opt_bool(p, "audio")?.unwrap_or(false);
            null(d.boot(&android, Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300)), audio))
        }
        "clone" => {
            if d.running.is_some() {
                return Err(format!("{} must be shut down to clone it", d.info.name));
            }
            let info = android.clone_avd(&d.info.udid, need_str(p, "name")?)?;
            serde_json::to_value(info).map_err(|e| e.to_string())
        }
        "shutdown" => null(d.shutdown()),
        // The guest's layout, which tap, swipe and touch take (a folded foldable's outer screen).
        "screen" => d.layout_size().map(|(w, h)| json!({"width": w, "height": h})),
        // The emulator's own PNG, passed through: with `path` it writes the file itself.
        "screenshot" => match str_param(p, "path") {
            Some(path) => d.screenshot_png_to(std::path::Path::new(path)).map(|(w, h)| json!({"path": path, "width": w, "height": h})),
            None => d.screenshot_png().and_then(|(w, h, png)| png_result(w, h, png, p)),
        },
        "tap" => null(d.play(&tap_samples(p)?)),
        "swipe" => null(d.play(&swipe_samples(p)?)),
        "touch" => {
            let (at, phase) = touch_sample(p)?;
            null(d.touch(at.x, at.y, phase))
        }
        "button" => {
            let (button, hold) = button(p)?;
            d.button(button, Direction::Down)?;
            sleep(hold);
            null(d.button(button, Direction::Up))
        }
        "type" => null(d.type_text(need_str(p, "text")?)),
        "key" => {
            let usage = opt_u64(p, "usage").ok_or("missing number parameter `usage`")? as u32;
            d.key(usage, Direction::Down)?;
            sleep(Duration::from_millis(15));
            null(d.key(usage, Direction::Up))
        }
        "location" => {
            let (lat, lon) = (crate::params::need_f64(p, "latitude")?, crate::params::need_f64(p, "longitude")?);
            null(d.set_location(lat, lon))
        }
        "info" => {
            let avd = d.avd.as_ref();
            let running = d
                .running
                .as_ref()
                .map(|r| json!({"serial": r.serial(), "console_port": r.console_port, "adb_port": r.adb_port, "pid": r.pid, "emulator": r.emulator()}));
            Ok(json!({
                "device": d.info,
                "avd_dir": avd.map(|a| a.dir.display().to_string()),
                "system_image": avd.map(|a| a.sysdir.display().to_string()),
                "profile": d.profile().ok(),
                "running": running,
            }))
        }
        "console" => {
            let commands: Vec<String> = match (str_param(p, "command"), p.get("commands").and_then(Value::as_array)) {
                (Some(c), _) => vec![c.to_owned()],
                (None, Some(list)) => list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
                (None, None) => return Err("give `command` or `commands`".into()),
            };
            d.console(&commands).map(|lines| json!(lines))
        }
        "rotate" => match crate::params::opt_orientation(p)? {
            None => null(d.rotate()),
            Some(_) => Err("Android's rotate turns 90° clockwise per call; `orientation` is iOS-only".into()),
        },
        "fold" => fold(&d, p),
        "rotary" => {
            let ticks = p.get("ticks").and_then(Value::as_i64).ok_or("missing number parameter `ticks`")?;
            null(d.rotary(i32::try_from(ticks).map_err(|e| e.to_string())?))
        }
        "touchpad" => {
            let coord = |k: &str| -> Result<Option<u32>, String> {
                p.get(k).map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or(format!("`{k}` must be a whole number"))).transpose()
            };
            let at = |k: &str| coord(k)?.ok_or(format!("missing number parameter `{k}`"));
            let (x, y) = (at("x")?, at("y")?);
            match (coord("to_x")?, coord("to_y")?) {
                (Some(tx), Some(ty)) => {
                    let ms = p.get("duration_ms").and_then(Value::as_u64).unwrap_or(300);
                    null(d.touchpad_swipe((x, y), (tx, ty), std::time::Duration::from_millis(ms)))
                }
                (None, None) => {
                    let phase = str_param(p, "phase").unwrap_or("tap");
                    if !["down", "move", "up", "tap"].contains(&phase) {
                        return Err(format!("unknown phase `{phase}` (down, move, up, tap)"));
                    }
                    null(d.touchpad(x, y, phase))
                }
                _ => Err("a swipe needs both `to_x` and `to_y`".into()),
            }
        }
        "stylus" => {
            let (samples, duration) = crate::params::stylus_samples(p)?;
            null(d.stylus(&samples, duration))
        }
        "xr" => {
            let command: XrCommand = serde_json::from_value(p.clone()).map_err(|e| format!("xr: {e} ({})", sim_core::xr::USAGE))?;
            d.xr(&command).map(|lines| json!(lines))
        }
        "vehicle" => {
            let area = vehicle_area(p)?;
            match (str_param(p, "property"), vehicle_value(p)?) {
                (None, None) => d.vehicle_list().map(|all| all.iter().map(reading_json).collect()),
                (None, Some(_)) => Err("give the `property` to set".into()),
                (Some(prop), None) => d.vehicle_get(prop, area).map(|r| reading_json(&r)),
                (Some(prop), Some(value)) => null(d.vehicle_set(prop, area, &value)),
            }
        }
        other => Err(format!("`{other}` isn't implemented for Android devices")),
    }
}
