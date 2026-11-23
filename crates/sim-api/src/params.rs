//! Parameter parsing and result shaping shared by every backend, so a method means the same on
//! iOS and Android.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use sim_core::form_factor::FormFactor;
use sim_core::frame::{Frame, encode_png};
use sim_core::geometry::Point;
use sim_core::gesture::{self, Phase, Sample};
use sim_core::hid::{Button, CROWN_UNITS_PER_TICK, Orientation};
use sim_core::platform::Platform;

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

/// `screenshot`'s result: written to `path`, else the PNG inline as base64.
pub fn screenshot_result(frame: &Frame, p: &Value) -> Result<Value, String> {
    let png = encode_png(frame)?;
    match str_param(p, "path") {
        Some(path) => std::fs::write(path, &png).map(|()| json!({"path": path, "width": frame.width, "height": frame.height})).map_err(err),
        None => Ok(json!({"width": frame.width, "height": frame.height, "png": base64::engine::general_purpose::STANDARD.encode(png)})),
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
