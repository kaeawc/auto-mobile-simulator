//! The iOS backend on hosts without the simulator service: no iOS devices exist, so every device
//! method reports that clearly instead of failing to build.

use serde_json::Value;

use crate::params::str_param;

/// Why iOS devices are missing on this host.
pub const REASON: &str = "iOS simulator devices need a macOS host with Xcode";

#[derive(Default)]
pub struct Session;

/// No HID input is sent on this host.
pub fn track_input() {}

/// Nothing is ever pending.
pub fn flush_input(_timeout: std::time::Duration) -> bool {
    true
}

/// No simulators to select from.
pub fn selection_infos(_: &mut Session) -> Result<Vec<sim_core::device::DeviceInfo>, String> {
    Ok(Vec::new())
}

/// Listings are empty; every other method names a device that can't exist here.
pub fn call(_: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "devices" | "runtimes" | "device_types" => Ok(Value::Array(Vec::new())),
        "create" => Err(format!("cannot create an iOS device: {REASON}")),
        "boot_many" => Err(format!("cannot boot iOS devices: {REASON}")),
        _ => {
            let q = str_param(p, "device").unwrap_or("booted");
            Err(format!("no device matches `{q}` ({REASON})"))
        }
    }
}
