//! Host-side introspection of a simulated device. A simulated device's processes run on the host,
//! under a `launchd_sim` dedicated to that device, and its filesystem is a host directory; so the
//! host can see far more than the device's own tools expose: per-process CPU, memory, sockets and
//! traffic, every app container, and crash reports.
//!
//! Parsers are pure and unit-tested; the `collect_*` functions run host tools (`ps`, `lsof`,
//! `nettop`, `plutil`).

pub mod net;
pub mod process;
pub mod storage;

use std::process::Command;

/// Runs a host tool and returns its stdout, or a readable error.
pub(crate) fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program).args(args).output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() || !out.stdout.is_empty() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}
