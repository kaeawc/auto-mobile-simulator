//! Parameter parsing shared by every backend.
// Only the iOS backend reads device parameters so far, and it exists on macOS alone.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde_json::Value;
use sim_core::form_factor::FormFactor;
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

pub fn need_bool(p: &Value, key: &str) -> Result<bool, String> {
    p.get(key).and_then(Value::as_bool).ok_or_else(|| format!("missing boolean parameter `{key}`"))
}

pub fn strings(p: &Value, key: &str) -> Vec<String> {
    p.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

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
