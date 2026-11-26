//! Parameter parsing and result shaping shared by every backend, so a method means the same on
//! iOS and Android.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use sim_core::form_factor::FormFactor;
#[cfg(target_os = "macos")]
use sim_core::frame::{Frame, encode_png};
use sim_core::geometry::Point;
use sim_core::gesture::{self, Phase, Sample};
use sim_core::hid::{Button, CROWN_UNITS_PER_TICK, Orientation};
use sim_core::platform::Platform;
use sim_core::{stylus, vhal};

pub fn str_param<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

pub fn need_str<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    str_param(p, key).ok_or_else(|| format!("missing string parameter `{key}`"))
}

pub fn need_f64(p: &Value, key: &str) -> Result<f64, String> {
    p.get(key).and_then(Value::as_f64).ok_or_else(|| format!("missing number parameter `{key}`"))
}

pub fn opt_u64(p: &Value, key: &str) -> Option<u64> {
    p.get(key).and_then(Value::as_u64)
}

/// `boot_many`'s `concurrency`: at least 1; by default a quarter of the host's CPUs, at least 2.
pub fn concurrency(p: &Value) -> Result<usize, String> {
    match opt_u64(p, "concurrency") {
        Some(0) => Err("concurrency must be at least 1".into()),
        Some(k) => Ok(k as usize),
        None => Ok(crate::settle::default_concurrency(std::thread::available_parallelism().map_or(4, usize::from))),
    }
}

/// An optional boolean: absent or null is `None`, any other type is refused.
pub fn opt_bool(p: &Value, key: &str) -> Result<Option<bool>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!("`{key}` must be true or false, not {other}")),
    }
}

// Only iOS methods read this so far.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn need_bool(p: &Value, key: &str) -> Result<bool, String> {
    p.get(key).and_then(Value::as_bool).ok_or_else(|| format!("missing boolean parameter `{key}`"))
}

// Only iOS methods read this so far.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn strings(p: &Value, key: &str) -> Vec<String> {
    p.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

// Only iOS methods read this so far.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn pairs(p: &Value, key: &str) -> Vec<(String, String)> {
    p.get(key)
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect())
        .unwrap_or_default()
}

pub fn err<E: ToString>(e: E) -> String {
    e.to_string()
}

/// The optional `platform` parameter.
pub fn opt_platform(p: &Value) -> Result<Option<Platform>, String> {
    str_param(p, "platform").map(|v| Platform::parse(v).ok_or_else(|| format!("unknown platform `{v}` (android or ios)"))).transpose()
}

/// The optional `form_factor` parameter.
pub fn opt_form_factor(p: &Value) -> Result<Option<FormFactor>, String> {
    str_param(p, "form_factor")
        .map(|v| {
            FormFactor::parse(v).ok_or_else(|| {
                let names: Vec<&str> = FormFactor::ALL.iter().map(|f| f.as_str()).collect();
                format!("unknown form factor `{v}` ({})", names.join(", "))
            })
        })
        .transpose()
}

/// `tap`: x, y and hold_ms (default 60) as touch samples in device pixels.
pub fn tap_samples(p: &Value) -> Result<Vec<Sample>, String> {
    let at = Point::new(need_f64(p, "x")?, need_f64(p, "y")?);
    Ok(gesture::tap(at, Duration::from_millis(opt_u64(p, "hold_ms").unwrap_or(60))))
}

/// `swipe`: x1, y1, x2, y2, duration_ms (default 300) and settle as touch samples.
pub fn swipe_samples(p: &Value) -> Result<Vec<Sample>, String> {
    let from = Point::new(need_f64(p, "x1")?, need_f64(p, "y1")?);
    let to = Point::new(need_f64(p, "x2")?, need_f64(p, "y2")?);
    let duration = Duration::from_millis(opt_u64(p, "duration_ms").unwrap_or(300));
    let settle = p.get("settle").and_then(Value::as_bool).unwrap_or(false);
    Ok(gesture::swipe(from, to, duration, Duration::from_millis(17), settle))
}

/// `touch`: one sample's point and phase.
pub fn touch_sample(p: &Value) -> Result<(Point, Phase), String> {
    let phase = match need_str(p, "phase")? {
        "down" => Phase::Down,
        "move" => Phase::Move,
        "up" => Phase::Up,
        other => return Err(format!("unknown phase `{other}`")),
    };
    Ok((Point::new(need_f64(p, "x")?, need_f64(p, "y")?), phase))
}

/// `button`: the button and how long to hold it (default 80 ms).
pub fn button(p: &Value) -> Result<(Button, Duration), String> {
    let name = need_str(p, "button")?;
    let button = Button::parse(name).ok_or_else(|| format!("unknown button `{name}`"))?;
    Ok((button, Duration::from_millis(opt_u64(p, "hold_ms").unwrap_or(80))))
}

/// The optional `orientation` parameter.
pub fn opt_orientation(p: &Value) -> Result<Option<Orientation>, String> {
    str_param(p, "orientation")
        .map(|v| {
            Orientation::parse(v).ok_or_else(|| {
                let names: Vec<&str> = Orientation::ALL.iter().map(|o| o.as_str()).collect();
                format!("unknown orientation `{v}` ({})", names.join(", "))
            })
        })
        .transpose()
}

/// Whether the optional `display` parameter picks the CarPlay screen (`carplay`) rather than the
/// device's own (`main`, the default).
pub fn on_carplay(p: &Value) -> Result<bool, String> {
    match str_param(p, "display").unwrap_or("main") {
        "main" => Ok(false),
        "carplay" | "car" => Ok(true),
        other => Err(format!("unknown display `{other}` (main or carplay)")),
    }
}

/// `remote_swipe` (iOS only, so unused where the iOS backend isn't built): a swipe across the Siri Remote's touch surface in `direction`, as samples
/// normalized to the surface (`0..=1`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn remote_swipe_samples(p: &Value) -> Result<Vec<Sample>, String> {
    let (from, to) = match need_str(p, "direction")? {
        "up" => (Point::new(0.5, 0.8), Point::new(0.5, 0.2)),
        "down" => (Point::new(0.5, 0.2), Point::new(0.5, 0.8)),
        "left" => (Point::new(0.8, 0.5), Point::new(0.2, 0.5)),
        "right" => (Point::new(0.2, 0.5), Point::new(0.8, 0.5)),
        other => return Err(format!("unknown direction `{other}` (up, down, left, right)")),
    };
    let duration = Duration::from_millis(opt_u64(p, "duration_ms").unwrap_or(250));
    Ok(gesture::swipe(from, to, duration, Duration::from_millis(17), false))
}

/// `rotary`'s turn on an Apple Watch, in Digital Crown units: `delta` as given, else `ticks` detents.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn crown_delta(p: &Value) -> Result<f64, String> {
    match (p.get("delta").and_then(Value::as_f64), p.get("ticks").and_then(Value::as_i64)) {
        (Some(delta), _) => Ok(delta),
        (None, Some(ticks)) => Ok(ticks as f64 * CROWN_UNITS_PER_TICK),
        (None, None) => Err("give `ticks` (detents) or `delta` (Digital Crown units)".into()),
    }
}

/// `vehicle`'s `area_id`: a number, or a string in decimal or hex (`"0x31"`); default 0, the
/// global area.
pub fn vehicle_area(p: &Value) -> Result<i32, String> {
    match p.get("area_id") {
        None | Some(Value::Null) => Some(0),
        Some(Value::Number(n)) => n.as_i64().and_then(|v| vhal::parse_id(&v.to_string())),
        Some(Value::String(s)) => vhal::parse_id(s),
        Some(_) => None,
    }
    .ok_or_else(|| "`area_id` must be a 32-bit number".into())
}

/// `vehicle`'s `value` as the console's text: strings as given, numbers and booleans written
/// out, arrays comma-separated.
pub fn vehicle_value(p: &Value) -> Result<Option<String>, String> {
    let text = |v: &Value| match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(_) | Value::Bool(_) => Ok(v.to_string()),
        _ => Err("`value` must be a string, number, boolean or an array of numbers".to_string()),
    };
    match p.get("value") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items.iter().map(text).collect::<Result<Vec<_>, _>>().map(|t| Some(t.join(","))),
        Some(v) => text(v).map(Some),
    }
}

/// `stylus`'s samples and how long to spread them over: a stroke with `to_x`/`to_y` (300 ms by
/// default), else one phase at a point, `tap` (down, up, out of range) by default.
pub fn stylus_samples(p: &Value) -> Result<(Vec<stylus::PenSample>, Duration), String> {
    let coord = |k: &str| -> Result<Option<i32>, String> {
        p.get(k)
            .map(|v| v.as_u64().and_then(|n| i32::try_from(n).ok()).ok_or(format!("`{k}` must be a whole number of pixels")))
            .transpose()
    };
    let at = |k: &str| coord(k)?.ok_or(format!("missing number parameter `{k}`"));
    let (x, y) = (at("x")?, at("y")?);
    let (button, eraser) = (opt_bool(p, "button")?.unwrap_or(false), opt_bool(p, "eraser")?.unwrap_or(false));
    let pressure = match p.get("pressure") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|n| u8::try_from(n).ok()).filter(|n| *n > 0).ok_or("`pressure` must be 1-255")?),
    };
    match (coord("to_x")?, coord("to_y")?) {
        (Some(tx), Some(ty)) => {
            let ms = opt_u64(p, "duration_ms").unwrap_or(300);
            let mut samples = stylus::stroke((x, y), (tx, ty), 10, button, eraser);
            samples.iter_mut().filter(|s| s.phase != stylus::PenPhase::Leave).for_each(|s| s.pressure = pressure);
            Ok((samples, Duration::from_millis(ms)))
        }
        (None, None) => {
            let mut samples = match str_param(p, "phase").unwrap_or("tap") {
                "tap" => stylus::tap(x, y, button, eraser),
                name => {
                    let phase = stylus::PenPhase::parse(name)
                        .ok_or_else(|| format!("unknown phase `{name}` (tap, {})", stylus::PenPhase::NAMES))?;
                    vec![stylus::PenSample { x, y, phase, pressure: None, button, eraser }]
                }
            };
            samples.iter_mut().for_each(|s| s.pressure = pressure);
            Ok((samples, Duration::ZERO))
        }
        _ => Err("a stroke needs both `to_x` and `to_y`".into()),
    }
}

/// A vehicle property reading as JSON, its value typed by the property (booleans, numbers,
/// arrays of numbers; strings and bytes stay text).
pub fn reading_json(r: &vhal::Reading) -> Value {
    use vhal::PropType as T;
    let num = |s: &str| -> Value {
        s.parse::<i64>().map(Value::from).or_else(|_| s.parse::<f64>().map(Value::from)).unwrap_or_else(|_| s.into())
    };
    let value = match r.prop_type() {
        T::Boolean => Value::Bool(r.value == "true"),
        T::Int32 | T::Int64 | T::Float => num(&r.value),
        T::Int32Vec | T::Int64Vec | T::FloatVec => Value::Array(r.value.split(',').filter(|s| !s.is_empty()).map(num).collect()),
        _ => Value::String(r.value.clone()),
    };
    json!({"property": r.property, "id": format!("{:#010x}", r.id as u32), "area_id": r.area_id, "status": r.status, "value": value})
}

/// `screenshot`'s result: written to `path`, else the PNG inline as base64.
#[cfg(target_os = "macos")]
pub fn screenshot_result(frame: &Frame, p: &Value) -> Result<Value, String> {
    png_result(frame.width, frame.height, &encode_png(frame)?, p)
}

/// An encoded screenshot written to `path`, or returned as base64 without one.
pub fn png_result(width: u32, height: u32, png: &[u8], p: &Value) -> Result<Value, String> {
    match str_param(p, "path") {
        Some(path) => std::fs::write(path, png).map(|()| json!({"path": path, "width": width, "height": height})).map_err(err),
        None => {
            // The base64 text moves into the result; `json!` would copy it.
            let mut result = json!({"width": width, "height": height});
            result["png"] = Value::String(base64::engine::general_purpose::STANDARD.encode(png));
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_factor_inputs_parse() {
        assert_eq!(opt_orientation(&json!({})), Ok(None));
        assert_eq!(opt_orientation(&json!({"orientation": "landscape-left"})), Ok(Some(Orientation::LandscapeLeft)));
        assert!(opt_orientation(&json!({"orientation": "diagonal"})).expect_err("bad").contains("portrait, landscape-right"));
        assert_eq!(on_carplay(&json!({})), Ok(false));
        assert_eq!(on_carplay(&json!({"display": "carplay"})), Ok(true));
        assert!(on_carplay(&json!({"display": "hud"})).is_err());
        assert_eq!(crown_delta(&json!({"ticks": -3})), Ok(-3.0 * CROWN_UNITS_PER_TICK));
        assert_eq!(crown_delta(&json!({"delta": 12.5, "ticks": 1})), Ok(12.5));
        assert!(crown_delta(&json!({})).is_err());
    }

    #[test]
    fn stylus_parameters() {
        use stylus::PenPhase::*;
        let phases = |p: Value| stylus_samples(&p).map(|(s, d)| (s.iter().map(|s| s.phase).collect::<Vec<_>>(), d));
        assert_eq!(phases(json!({"x": 1, "y": 2})), Ok((vec![Down, Up, Leave], Duration::ZERO)), "a tap by default");
        assert_eq!(phases(json!({"x": 1, "y": 2, "phase": "hover"})), Ok((vec![Hover], Duration::ZERO)));
        let (stroke, d) = stylus_samples(&json!({"x": 0, "y": 0, "to_x": 100, "to_y": 0, "pressure": 90, "button": true})).expect("stroke");
        assert_eq!((stroke.len(), d), (13, Duration::from_millis(300)));
        assert!(stroke[..12].iter().all(|s| s.pressure == Some(90) && s.button) && stroke[12].phase == Leave);
        assert!(stylus_samples(&json!({"x": 1, "y": 2, "phase": "press"})).expect_err("phase").contains("tap, hover, down"));
        assert_eq!(stylus_samples(&json!({"x": 1, "y": 2, "pressure": 0})), Err("`pressure` must be 1-255".into()));
        assert_eq!(stylus_samples(&json!({"x": 1, "y": 2, "to_x": 3})), Err("a stroke needs both `to_x` and `to_y`".into()));
        assert_eq!(stylus_samples(&json!({"x": -1, "y": 2})), Err("`x` must be a whole number of pixels".into()));
    }

    #[test]
    fn vehicle_parameters_and_readings() {
        assert_eq!(vehicle_area(&json!({})), Ok(0));
        assert_eq!(vehicle_area(&json!({"area_id": 49})), Ok(49));
        assert_eq!(vehicle_area(&json!({"area_id": "0x31"})), Ok(49));
        assert!(vehicle_area(&json!({"area_id": true})).is_err());
        assert_eq!(vehicle_value(&json!({})), Ok(None));
        assert_eq!(vehicle_value(&json!({"value": "drive"})), Ok(Some("drive".into())));
        assert_eq!(vehicle_value(&json!({"value": 12.5})), Ok(Some("12.5".into())));
        assert_eq!(vehicle_value(&json!({"value": false})), Ok(Some("false".into())));
        assert_eq!(vehicle_value(&json!({"value": [2, 0]})), Ok(Some("2,0".into())));
        assert!(vehicle_value(&json!({"value": {"a": 1}})).is_err());
        let r = |id: u32, value: &str| vhal::Reading {
            property: vhal::label(id as i32),
            id: id as i32,
            area_id: 0,
            status: "available".into(),
            value: value.into(),
        };
        assert_eq!(reading_json(&r(0x1140_0400, "8"))["value"], json!(8));
        assert_eq!(reading_json(&r(0x1140_0400, "8"))["id"], json!("0x11400400"));
        assert_eq!(reading_json(&r(0x1160_0207, "12.5"))["value"], json!(12.5));
        assert_eq!(reading_json(&r(0x1120_0407, "true"))["value"], json!(true));
        assert_eq!(reading_json(&r(0x1141_0a01, "2,0"))["value"], json!([2, 0]));
        assert_eq!(reading_json(&r(0x1110_0101, "Toy Car"))["value"], json!("Toy Car"));
    }

    #[test]
    fn remote_swipes_cross_the_touch_surface() {
        let up = remote_swipe_samples(&json!({"direction": "up"})).expect("up");
        assert_eq!((up[0].phase, up.last().map(|s| s.phase)), (Phase::Down, Some(Phase::Up)));
        assert!(up[0].point.y > up.last().map_or(1.0, |s| s.point.y));
        assert!(up.iter().all(|s| (0.0..=1.0).contains(&s.point.x) && (0.0..=1.0).contains(&s.point.y)));
        let right = remote_swipe_samples(&json!({"direction": "right"})).expect("right");
        assert!(right[0].point.x < right.last().map_or(0.0, |s| s.point.x));
        assert!(remote_swipe_samples(&json!({"direction": "diagonal"})).is_err());
    }
}
