//! The Android backend: device methods on AVDs and running emu-rs instances, through
//! `sim-android`. Builds on every host; booting needs an Apple silicon Mac (the emulator does).

use std::thread::sleep;
use std::time::Duration;

use serde_json::{Value, json};
use sim_android::Android;
use sim_core::device::DeviceInfo;
use sim_core::form_factor::Posture;
use sim_core::hid::Direction;

use crate::params::{
    button, need_str, opt_u64, reading_json, screenshot_result, str_param, swipe_samples, tap_samples, touch_sample, vehicle_area,
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

/// Dispatches one device method for the AVD named `udid`.
pub fn call(s: &mut Session, method: &str, p: &Value, udid: &str) -> Result<Value, String> {
    let android = s.backend().clone();
    let d = android.device(udid)?;
    if crate::params::on_carplay(p)? {
        return Err("display=carplay is an iPhone's CarPlay screen; Android devices have one display".into());
    }
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "delete" => null(android.delete(&d)),
        "boot" => null(d.boot(&android, Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300)))),
        "shutdown" => null(d.shutdown()),
        "screen" => d.screen_size().map(|(w, h)| json!({"width": w, "height": h})),
        "screenshot" => screenshot_result(&d.screenshot()?, p),
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
                .map(|r| json!({"serial": r.serial(), "console_port": r.console_port, "adb_port": r.adb_port, "pid": r.pid}));
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
        "fold" => {
            let angle = match (p.get("angle").and_then(Value::as_f64), str_param(p, "posture")) {
                (Some(a), _) => a as f32,
                (None, Some(name)) => {
                    Posture::parse(name).ok_or_else(|| format!("unknown posture `{name}` (closed, half-opened, opened)"))?.angle()
                }
                (None, None) => return Err("give `angle` (0-180) or `posture` (closed, half-opened, opened)".into()),
            };
            d.fold(angle).map(|()| json!({"angle": angle, "posture": Posture::from_angle(angle)}))
        }
        "rotary" => {
            let ticks = p.get("ticks").and_then(Value::as_i64).ok_or("missing number parameter `ticks`")?;
            null(d.rotary(i32::try_from(ticks).map_err(|e| e.to_string())?))
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
