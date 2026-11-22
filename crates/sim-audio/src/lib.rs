//! Per-device simulator audio, without a host audio driver.
//!
//! A simulated device's processes are ordinary clients of the host's Core Audio server, so:
//! - **Capture / mute:** a Core Audio process tap (public, macOS 14.2+) over the device's processes
//!   captures exactly that device's output, and a muted tap silences it on the host (a per-device
//!   null sink). The tap runs in a small Swift helper (`native/simaudio.swift`), built on demand.
//! - **Route / volume:** each device reads its host devices and volume from
//!   `<data>/var/run/simulatoraudio/audiosettings.plist` when an audio client starts.
//!
//! Capture needs the System Audio Recording permission for the app that runs sim-rs.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::Value;

/// The helper's Swift source.
pub const HELPER_SOURCE: &str = include_str!("../native/simaudio.swift");

/// The UID that means "follow the host's default device".
pub const HOST_DEFAULT_UID: &str = "__sim__hostUseSystemDefaultDeviceUID";

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Builds (or reuses) the helper and returns its path.
pub fn helper() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = home.join("Library/Caches/sim-rs/audio");
    let out = dir.join(format!("simaudio-{:016x}", fnv1a(HELPER_SOURCE)));
    if out.is_file() {
        return Ok(out);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let src = dir.join("simaudio.swift");
    std::fs::write(&src, HELPER_SOURCE).map_err(|e| e.to_string())?;
    let tmp = dir.join("simaudio.building");
    let built = Command::new("/usr/bin/xcrun").args(["swiftc", "-O", "-o"]).arg(&tmp).arg(&src).output().map_err(|e| e.to_string())?;
    if !built.status.success() {
        return Err(format!("building the audio helper failed: {}", String::from_utf8_lossy(&built.stderr).trim()));
    }
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Host audio devices as the helper reports them.
pub fn host_devices() -> Result<Value, String> {
    let out = Command::new(helper()?).arg("devices").output().map_err(|e| e.to_string())?;
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad helper output: {e}"))
}

/// The device's audio settings file.
pub fn settings_path(data_path: &Path) -> PathBuf {
    data_path.join("var/run/simulatoraudio/audiosettings.plist")
}

/// Reads the device's audio settings (UIDs, volume, ringer) as JSON.
pub fn settings(data_path: &Path) -> Result<Value, String> {
    let path = settings_path(data_path);
    let out = Command::new("/usr/bin/plutil").args(["-convert", "json", "-o", "-"]).arg(&path).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

fn plutil_replace(data_path: &Path, key: &str, kind: &str, value: &str) -> Result<(), String> {
    let path = settings_path(data_path);
    if !path.exists() {
        return Err(format!("{} does not exist; is the device booted?", path.display()));
    }
    let out = Command::new("/usr/bin/plutil").args(["-replace", key, kind, value]).arg(&path).output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()) }
}

/// Sets the device's output volume, 0–100 (100 is unity gain). Applies to audio clients started afterwards.
pub fn set_volume(data_path: &Path, level: u8) -> Result<(), String> {
    plutil_replace(data_path, "sim_volume", "-integer", &level.min(100).to_string())
}

/// Pins the device's input or output to a host device UID (or `default`). Unlike the simulator's
/// router, this accepts any device (including loopback devices the router hides). Applies to audio
/// clients started afterwards; the simulator's audio service rewrites the file on boot and when host
/// devices change, so callers re-apply after those.
pub fn set_route(data_path: &Path, output: bool, uid: &str) -> Result<(), String> {
    let uid = if uid == "default" { HOST_DEFAULT_UID } else { uid };
    plutil_replace(data_path, if output { "sim_output_device_uid" } else { "sim_input_device_uid" }, "-string", uid)
}

/// A running tap of one device's audio output.
pub struct Capture {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    pub started: Value,
    pub path: Option<PathBuf>,
}

impl Capture {
    /// Starts tapping the device whose `launchd_sim` is `launchd_pid`. With `mute`, the device is
    /// silent on the host meanwhile. With `out`, samples are written there as WAV on stop. Waits
    /// until the device has an audio client (up to 30 s).
    pub fn start(launchd_pid: i32, out: Option<&Path>, mute: bool) -> Result<Self, String> {
        let mut cmd = Command::new(helper()?);
        cmd.args(["tap", "--launchd", &launchd_pid.to_string()]);
        if let Some(p) = out {
            cmd.arg("--out").arg(p);
        }
        if mute {
            cmd.arg("--mute");
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().ok_or("no helper stdout")?);
        let mut line = String::new();
        stdout.read_line(&mut line).map_err(|e| e.to_string())?;
        let started: Value = serde_json::from_str(&line).map_err(|_| format!("audio helper failed to start: {}", line.trim()))?;
        if let Some(e) = started.get("error").and_then(Value::as_str) {
            let _ = child.wait();
            return Err(e.to_owned());
        }
        Ok(Self { child, stdin, stdout, started, path: out.map(Path::to_path_buf) })
    }

    /// Stops the tap and returns its stats (frames, duration_ms, rms, peak, dbfs, path).
    pub fn stop(mut self) -> Result<Value, String> {
        drop(self.stdin.take()); // EOF tells the helper to stop
        let mut last = String::new();
        for line in (&mut self.stdout).lines().map_while(Result::ok) {
            last = line;
        }
        let _ = self.child.wait();
        serde_json::from_str(&last).map_err(|_| format!("audio helper ended without stats: {last}"))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.flush();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_path_and_default_uid() {
        assert_eq!(settings_path(Path::new("/d")), PathBuf::from("/d/var/run/simulatoraudio/audiosettings.plist"));
        assert!(HOST_DEFAULT_UID.starts_with("__sim__"));
        assert_ne!(fnv1a(HELPER_SOURCE), fnv1a(""));
    }

    #[test]
    fn route_and_volume_edit_a_settings_file() {
        let root = std::env::temp_dir().join(format!("sim-audio-test-{}", std::process::id()));
        let path = settings_path(&root);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>sim_volume</key><integer>100</integer></dict></plist>"#).expect("plist");
        set_volume(&root, 40).expect("volume");
        set_route(&root, true, "default").expect("route");
        let s = settings(&root).expect("read");
        assert_eq!(s["sim_volume"], 40);
        assert_eq!(s["sim_output_device_uid"], HOST_DEFAULT_UID);
        std::fs::remove_dir_all(root).ok();
    }
}
