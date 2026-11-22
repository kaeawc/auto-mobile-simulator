//! The iOS backend on hosts without the simulator service: no iOS devices exist, so every device
//! method reports that clearly instead of failing to build.

use serde_json::Value;

use crate::params::str_param;

/// Why iOS devices are missing on this host.
pub const REASON: &str = "iOS simulator devices need a macOS host with Xcode";

#[derive(Default)]
pub struct Session;

/// Every device method: the device can't exist here.
pub fn call(_: &mut Session, _method: &str, p: &Value) -> Result<Value, String> {
    let q = str_param(p, "device").unwrap_or("booted");
    Err(format!("no device matches `{q}` ({REASON})"))
}
