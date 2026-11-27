//! Camera links (#323): which camera source feeds each device camera. `camera_sources` lists the
//! host's cameras (real and virtual) and the synthetic kinds from `sim-camera`, with the host's
//! camera permission (read, never prompted for); `camera_link`, `camera_unlink` and
//! `camera_links` set, remove and report each device camera's source.
//!
//! Links are saved with the device (Android: `<avd>/simee/cameras.ini`, `sim_android::save_cameras`),
//! so every boot passes them to emu as `-camera-back` / `-camera-front` and they survive a server
//! restart. A running Android device is relinked live through emu's console (`camera link`).
//! Capture runs in emu, which shows the test card while a linked camera is unplugged; here a
//! webcam link whose camera the host no longer lists reports `source missing`.
//!
//! iOS ([`ios_link`]): the same file under the simulator's data directory
//! (`<data>/simee/cameras.ini`). A booted simulator's camera is an `ios_feed::Feed` this process
//! serves on `<data>/cam.sock`, one per device (the shim's back and front cameras read that one
//! socket, so a link sets both), and the shim is injected device-wide with `launchctl setenv` in
//! the device. The feed lives as long as this process, so only a server (`simee serve`) links a
//! booted simulator; any other process hands the call to a running server or says it needs one.
//! A boot through simee reapplies the saved link ([`ios_booted`]); a shutdown, erase or delete
//! stops the feed ([`ios_stopped`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::{Value, json};
use sim_android::{CAMERA_FACINGS, Device, saved_cameras, update_cameras};
use sim_camera::catalog::SYNTHETIC;
use sim_camera::{Authorization, HostCamera, SourceSpec};
use sim_core::platform::Platform;

use crate::methods::Session as Calls;
use crate::params::{need_str, str_param};

/// The camera methods, dispatched by [`call`].
pub const METHODS: &[&str] = &["camera_sources", "camera_link", "camera_unlink", "camera_links"];

pub fn is_method(method: &str) -> bool {
    METHODS.contains(&method)
}

/// Where the host's cameras and its camera permission come from: the host, or a fake in tests.
pub trait Host: Send {
    /// The host's cameras (never needs the permission, never prompts).
    fn cameras(&self) -> Result<Vec<HostCamera>, String>;
    /// The host's camera permission, read without prompting.
    fn permission(&self) -> Result<Authorization, String>;
}

/// The host's own cameras (AVFoundation on macOS, V4L2 on Linux) and TCC status.
struct HostCameras;

impl Host for HostCameras {
    fn cameras(&self) -> Result<Vec<HostCamera>, String> {
        sim_camera::host_cameras().map_err(|e| e.to_string())
    }

    fn permission(&self) -> Result<Authorization, String> {
        sim_camera::tcc::status().map_err(|e| e.to_string())
    }
}

/// Host cameras and a permission given by the caller instead of the host's (front ends' tests).
struct Fixed {
    cameras: Vec<HostCamera>,
    permission: Authorization,
}

impl Host for Fixed {
    fn cameras(&self) -> Result<Vec<HostCamera>, String> {
        Ok(self.cameras.clone())
    }

    fn permission(&self) -> Result<Authorization, String> {
        Ok(self.permission)
    }
}

/// The camera host a connection reads, made on first use; in tests, fakes of the iOS side too.
#[derive(Default)]
pub struct Session {
    host: Option<Box<dyn Host>>,
    /// The simulators and their launchctl (none: the iOS backend's).
    ios: Option<Box<dyn IosHost>>,
    /// The feeds, the shim, the server, and whether this process serves (none: this process's).
    env: Option<Env>,
}

impl Session {
    #[cfg(test)]
    pub fn with(host: impl Host + 'static) -> Session {
        Session { host: Some(Box::new(host)), ..Session::default() }
    }

    /// A session whose host has these cameras, (id, name, virtual) each, and whose camera
    /// permission is authorized or denied.
    pub fn fixed(cameras: &[(&str, &str, bool)], authorized: bool) -> Session {
        let cameras = cameras
            .iter()
            .map(|&(id, name, is_virtual)| HostCamera {
                id: id.into(),
                name: name.into(),
                kind: if is_virtual { sim_camera::CameraKind::Virtual } else { sim_camera::CameraKind::External },
                is_virtual,
                manufacturer: None,
                model: None,
                transport: None,
                path: None,
                formats: Vec::new(),
            })
            .collect();
        let permission = if authorized { Authorization::Authorized } else { Authorization::Denied };
        Session { host: Some(Box::new(Fixed { cameras, permission })), ..Session::default() }
    }

    fn host(&mut self) -> &dyn Host {
        &**self.host.get_or_insert_with(|| Box::new(HostCameras))
    }
}

/// Dispatches one camera method.
pub fn call(s: &mut Calls, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "camera_sources" => sources(s.camera.host(), str_param(p, "kind")),
        "camera_links" => match str_param(p, "device") {
            None => {
                let android = s.android.backend().clone();
                let mut rows = Vec::new();
                for info in android.devices() {
                    let d = android.device(&info.udid)?;
                    rows.extend(link_rows(s.camera.host(), &d));
                }
                // Simulators too, where the host has them (none on a host without the service).
                rows.extend(with_ios(s, |host, ios, env| {
                    Ok(ios.targets().unwrap_or_default().iter().flat_map(|t| ios_rows(host, env, t)).collect::<Vec<_>>())
                })?);
                Ok(Value::Array(rows))
            }
            Some(_) => match target(s, p)? {
                Target::Android(d) => Ok(Value::Array(link_rows(s.camera.host(), &d))),
                Target::Ios(q) => with_ios(s, |host, ios, env| Ok(Value::Array(ios_rows(host, env, &ios.target(&q)?)))),
            },
        },
        "camera_link" => {
            let facings = facings(Some(need_str(p, "facing")?))?;
            let source = need_str(p, "source")?;
            match target(s, p)? {
                Target::Android(d) => link(s.camera.host(), &d, &facings, source),
                Target::Ios(q) => with_ios(s, |host, ios, env| ios_link(host, ios, env, &q, &facings, source, p)),
            }
        }
        "camera_unlink" => {
            let facings = facings(str_param(p, "facing"))?;
            match target(s, p)? {
                Target::Android(d) => unlink(s.camera.host(), &d, &facings),
                Target::Ios(q) => with_ios(s, |_, ios, env| ios_unlink(ios, env, &q, &facings, p)),
            }
        }
        other => Err(format!("unknown camera method `{other}`")),
    }
}

/// `boot`'s `camera_back` / `camera_front`: linked (and saved) before the boot, so the boot
/// passes them to emu (Android) or reapplies them ([`ios_booted`]). Nothing when neither is given.
pub fn before_boot(s: &mut Calls, p: &Value) -> Result<(), String> {
    let asked: Vec<(&str, &str)> =
        [("back", "camera_back"), ("front", "camera_front")].into_iter().filter_map(|(f, k)| Some((f, str_param(p, k)?))).collect();
    if asked.is_empty() {
        return Ok(());
    }
    match target(s, p)? {
        Target::Android(d) => {
            for (facing, source) in asked {
                link(s.camera.host(), &d, &[facing], source)?;
            }
            Ok(())
        }
        Target::Ios(q) => with_ios(s, |host, ios, env| ios_before_boot(host, ios, env, &q, &asked)),
    }
}

/// The device a call names: an Android device, or a simulator's query for the iOS side.
enum Target {
    Android(Box<Device>),
    Ios(String),
}

fn target(s: &mut Calls, p: &Value) -> Result<Target, String> {
    let (platform, udid) = crate::methods::resolve(s, p)?;
    match platform {
        Platform::Ios => Ok(Target::Ios(udid)),
        Platform::Android => s.android.backend().device(&udid).map(|d| Target::Android(Box::new(d))),
    }
}

/// `facing`: back, front or both (none: both).
fn facings(facing: Option<&str>) -> Result<Vec<&'static str>, String> {
    match facing {
        None | Some("both") => Ok(CAMERA_FACINGS.to_vec()),
        Some(f) => CAMERA_FACINGS
            .iter()
            .find(|c| **c == f)
            .map(|c| vec![*c])
            .ok_or_else(|| format!("unknown facing `{f}`: a device camera is back, front or both")),
    }
}

/// `camera_sources`: every source a camera can link to, and the host's camera permission.
fn sources(host: &dyn Host, kind: Option<&str>) -> Result<Value, String> {
    let (want_host, want_virtual_only, want_synthetic) = match kind {
        None => (true, false, true),
        Some("host") => (true, false, false),
        Some("virtual") => (true, true, false),
        Some("synthetic") => (false, false, true),
        Some(other) => {
            return Err(format!("unknown kind `{other}`: host (the host's cameras), virtual (its virtual cameras) or synthetic"));
        }
    };
    let mut out = json!({"permission": permission_json(host)});
    let mut list = Vec::new();
    if want_host {
        match host.cameras() {
            Ok(cameras) => list.extend(cameras.iter().filter(|c| !want_virtual_only || c.is_virtual).map(|c| {
                json!({"source": c.source_id(), "name": c.name, "kind": c.kind, "virtual": c.is_virtual, "manufacturer": c.manufacturer,
                    "model": c.model, "transport": c.transport, "formats": c.formats})
            })),
            Err(e) => out["cameras_error"] = json!(e),
        }
    }
    if want_synthetic {
        list.extend(SYNTHETIC.iter().map(|k| {
            json!({"source": k.example, "name": k.prefix, "kind": "synthetic", "virtual": false, "description": k.description,
                "supported": k.supported})
        }));
    }
    out["sources"] = Value::Array(list);
    Ok(out)
}

/// The host's camera permission: its status, and the fix when it blocks a capture.
fn permission_json(host: &dyn Host) -> Value {
    match host.permission() {
        Ok(status) => json!({"status": status.as_str(), "fix": status.fix()}),
        Err(e) => json!({"status": "unknown", "fix": e}),
    }
}

/// A source to save: its canonical id (a host camera by its stable id) and, for a host camera, its
/// name.
struct Resolved {
    id: String,
    name: Option<String>,
}

/// Resolves `source` to what `camera_link` saves: a host camera found by id, name, index or part
/// of its name (and the permission to capture it checked), a synthetic source checked, `none`
/// (camera off), or `emulated` (the test card, as the stock emulator says).
fn resolve_source(host: &dyn Host, source: &str) -> Result<Resolved, String> {
    let source = source.trim();
    if source.contains(['\n', '\r']) {
        return Err("a source id is one line".into());
    }
    // emu's console splits a command on whitespace and joins it with single spaces, so a source
    // with a run of whitespace (or a tab, or a control character) would link differently live
    // than from the saved file at boot.
    if source.chars().any(char::is_control) || source.chars().any(|c| c.is_whitespace() && c != ' ') || source.contains("  ") {
        return Err(
            "a source id can't contain control characters, or whitespace other than single spaces (the device console would change it, so the live link and the next boot would differ)"
                .into(),
        );
    }
    let plain = |id: &str| Ok(Resolved { id: id.to_owned(), name: None });
    match source {
        "none" => return plain("none"),
        "emulated" | "virtualscene" => return plain("pattern:testcard"),
        _ => {}
    }
    let spec = SourceSpec::parse(source).map_err(|e| e.to_string())?;
    match &spec {
        SourceSpec::Video(_) => {
            Err("video: sources aren't supported yet (they need the VideoToolbox decoder, #335); use image:<path> for a still".into())
        }
        SourceSpec::Image(path) if !path.is_absolute() => {
            Err(format!("image:{}: give an absolute path (the device's emulator opens it, not this shell)", path.display()))
        }
        SourceSpec::Image(path) if !path.is_file() => Err(format!("image:{}: no such file", path.display())),
        SourceSpec::Webcam(query) => {
            let cameras = host.cameras()?;
            let camera = sim_camera::resolve_webcam(query, &cameras).map_err(|e| match e {
                sim_camera::Error::NotFound { .. } => {
                    let synthetic: Vec<&str> = SYNTHETIC.iter().filter(|k| k.supported).map(|k| k.example).collect();
                    format!("{e}; or link a synthetic source: {} (`camera_sources` lists them)", synthetic.join(", "))
                }
                e => e.to_string(),
            })?;
            let status = host.permission()?;
            if let Some(fix) = status.fix() {
                return Err(format!("{} ({}) can't be captured: camera permission is {}: {fix}", camera.name, camera.id, status.as_str()));
            }
            Ok(Resolved { id: camera.source_id(), name: Some(camera.name.clone()) })
        }
        _ => plain(&spec.to_string()),
    }
}

/// What a running device's emulator links each camera to now (`camera list`): `None` for a
/// device that isn't running under simee's emu, or an emulator that doesn't say.
fn live(d: &Device) -> Option<Vec<(String, String)>> {
    d.running.as_ref().filter(|r| r.simee)?;
    let lines = d.console(&["camera list".to_owned()]).ok()?;
    let links: Vec<(String, String)> = lines
        .iter()
        .filter_map(|l| l.split_once(':'))
        .map(|(f, s)| (f.trim().to_owned(), s.trim().to_owned()))
        .filter(|(f, _)| CAMERA_FACINGS.contains(&f.as_str()))
        .collect();
    (!links.is_empty()).then_some(links)
}

/// Relinks camera `facing` of a running device live, through emu's console.
fn relink(d: &Device, facing: &str, source: &str) -> Result<(), String> {
    d.console(&[format!("camera link {facing} {source}")]).map(drop).map_err(|e| {
        if e.contains("unknown command") {
            format!(
                "{}'s emu has no camera console (`camera link`, #321): rebuild it (`cargo build --release -p simee-emu`), \
                 reboot the device with `simee boot`, then link again",
                d.info.name
            )
        } else {
            format!("camera link {facing} {source}: {e}")
        }
    })
}

/// The saved links of `d`'s AVD.
fn saved(d: &Device) -> Result<(std::path::PathBuf, Vec<(String, String)>), String> {
    let avd = d.avd.as_ref().ok_or_else(|| format!("{}: its config.ini or system image can't be read", d.info.name))?;
    Ok((avd.dir.clone(), saved_cameras(&avd.dir)))
}

/// Refuses a device the stock emulator runs: it has no `camera link`.
fn simee_or_off(d: &Device) -> Result<bool, String> {
    match &d.running {
        None => Ok(false),
        Some(r) if r.simee => Ok(true),
        Some(r) => Err(format!(
            "{} is running under the stock emulator (PID {}), which can't relink cameras; shut it down, link, and boot it with `simee boot {}`",
            d.info.name, r.pid, d.info.udid
        )),
    }
}

/// `camera_link`: checks the source, relinks a running device live, then saves the link.
fn link(host: &dyn Host, d: &Device, facings: &[&str], source: &str) -> Result<Value, String> {
    let resolved = resolve_source(host, source)?;
    let (dir, links) = saved(d)?;
    let running = simee_or_off(d)?;
    let mut note = None;
    if running {
        let before = live(d);
        let mut done: Vec<(&str, String)> = Vec::new();
        for facing in facings {
            if let Err(e) = relink(d, facing, &resolved.id) {
                // Put back what was relinked before the failure, and save nothing.
                let mut restored = Vec::new();
                for (done_facing, previous) in done.iter().rev() {
                    match relink(d, done_facing, previous) {
                        Ok(()) => restored.push(format!("the {done_facing} camera was put back on {previous}")),
                        Err(re) => restored.push(format!("the {done_facing} camera is still on {} ({re})", resolved.id)),
                    }
                }
                return Err(if restored.is_empty() { e } else { format!("{e}; {}", restored.join("; ")) });
            }
            let previous = before
                .as_ref()
                .and_then(|b| b.iter().find(|(f, _)| f == facing))
                .map(|(_, s)| s.clone())
                .or_else(|| links.iter().find(|(f, _)| f == facing).map(|(_, s)| s.clone()))
                .unwrap_or_else(|| default_link(d, facing));
            done.push((facing, previous));
            let was_off = before.as_ref().is_some_and(|b| b.iter().any(|(f, s)| f == facing && s == "none"));
            if was_off && resolved.id != "none" {
                note = Some(format!(
                    "the guest lists its cameras once, at boot: reboot {} for its new {facing} camera to appear (the link is saved for that boot)",
                    d.info.name
                ));
            }
        }
    }
    update_cameras(&dir, |links| {
        for facing in facings {
            links.retain(|(f, _)| f != facing);
            links.push(((*facing).to_owned(), resolved.id.clone()));
        }
    })?;
    let mut out = json!({"device": d.info.udid, "applied": if running { "live" } else { "at_boot" }, "links": link_rows(host, d)});
    if let Some(name) = resolved.name {
        out["name"] = json!(name);
    }
    if let Some(note) = note {
        out["note"] = json!(note);
    }
    Ok(out)
}

/// What camera `facing` is without a link: the AVD's own `hw.camera.<facing>`, as emu reads it
/// (none or absent: no camera; a source id as it is; any other stock value: the test card).
fn avd_default(d: &Device, facing: &str) -> String {
    match d.avd.as_ref().and_then(|a| a.get(&format!("hw.camera.{facing}"))).map(str::trim) {
        None | Some("" | "none") => "none".into(),
        Some(v) if v.contains(':') => v.into(),
        Some(_) => "emulated".into(),
    }
}

/// [`avd_default`] as emu reports it live: the stock test-card values are `pattern:testcard`.
fn default_link(d: &Device, facing: &str) -> String {
    match avd_default(d, facing).as_str() {
        "emulated" => "pattern:testcard".into(),
        other => other.into(),
    }
}

/// `camera_unlink`: removes the links; a running device goes back to the AVD's own cameras live.
fn unlink(host: &dyn Host, d: &Device, facings: &[&str]) -> Result<Value, String> {
    let (dir, links) = saved(d)?;
    let running = simee_or_off(d)?;
    let mut now = Vec::new();
    for facing in facings {
        let default = avd_default(d, facing);
        if running {
            relink(d, facing, &default)?;
        }
        now.push(json!({"facing": facing, "source": default}));
    }
    let unlinked: Vec<String> = links.iter().filter(|(f, _)| facings.contains(&f.as_str())).map(|(f, _)| f.clone()).collect();
    update_cameras(&dir, |links| links.retain(|(f, _)| !facings.contains(&f.as_str())))?;
    Ok(json!({"device": d.info.udid, "unlinked": unlinked, "now": now, "applied": if running { "live" } else { "at_boot" },
        "links": link_rows(host, d)}))
}

/// One row per linked camera of `d`: the saved link and, on a running device, what emu reports.
fn link_rows(host: &dyn Host, d: &Device) -> Vec<Value> {
    let saved = d.avd.as_ref().map(|a| saved_cameras(&a.dir)).unwrap_or_default();
    let live = live(d);
    let running = d.running.as_ref().is_some_and(|r| r.simee);
    let mut cameras: Option<Option<Vec<HostCamera>>> = None;
    let mut rows = Vec::new();
    for facing in CAMERA_FACINGS {
        let find = |list: &[(String, String)]| list.iter().find(|(f, _)| f == facing).map(|(_, s)| s.clone());
        let (saved, live) = (find(&saved), live.as_deref().and_then(find));
        let (source, mut state) = match (&saved, &live) {
            (None, None) => continue,
            (None, Some(l)) if l == "none" => continue,
            // The AVD's own camera (emu maps the stock values to the test card): nobody linked it.
            (None, Some(l)) if *l == default_link(d, facing) => continue,
            (None, Some(l)) => (l.clone(), "unsaved"),
            (Some(s), Some(l)) if s != l => (s.clone(), "relinked"),
            (Some(s), _) => (s.clone(), if running { "linked" } else { "saved" }),
        };
        let effective = live.clone().unwrap_or_else(|| source.clone());
        let mut row = json!({"device": d.info.udid, "platform": "android", "facing": facing, "source": source});
        if live.is_some() {
            row["live"] = json!(live);
        }
        if effective == "none" {
            state = "off";
        } else {
            webcam_state(host, &effective, &mut cameras, &mut row, &mut state, "the device shows the test card until it is back");
        }
        row["state"] = json!(state);
        rows.push(row);
    }
    rows
}

/// For a webcam link: the camera's name, or `source missing` / `permission missing` with the
/// fix, into `row` and `state` (`meanwhile`: what the device shows while the camera is missing).
/// `cameras` keeps the host's listing across rows.
fn webcam_state(
    host: &dyn Host,
    effective: &str,
    cameras: &mut Option<Option<Vec<HostCamera>>>,
    row: &mut Value,
    state: &mut &'static str,
    meanwhile: &str,
) {
    let Some(query) = effective.strip_prefix("webcam:") else { return };
    let cams = cameras.get_or_insert_with(|| host.cameras().ok());
    match cams.as_deref().map(|c| sim_camera::resolve_webcam(query, c)) {
        Some(Ok(c)) => {
            row["name"] = json!(c.name);
            if let Ok(status) = host.permission()
                && let Some(fix) = status.fix()
            {
                *state = "permission missing";
                row["fix"] = json!(fix);
            }
        }
        Some(Err(sim_camera::Error::NotFound { .. })) => {
            *state = "source missing";
            row["fix"] =
                json!(format!("the host doesn't list this camera now (unplugged, or its app quit): {meanwhile}, or link another source"));
        }
        _ => {}
    }
}

// ---- iOS: the simulator side of a link (#323, on #322's feed and shim) ----

/// The shim the device-wide injection inserts (`shims/ios-camera/build.sh` builds it), and the
/// hooks it loads from beside itself.
const SHIM: &str = "libsimee-camera.dylib";
const SHIM_HOOKS: &str = "libsimee-camera-hooks.dylib";

/// Names the shim to insert instead of the one found beside the simee executable.
pub const SHIM_ENV: &str = "SIMEE_CAMERA_SHIM";

const DYLD_INSERT: &str = "DYLD_INSERT_LIBRARIES";

/// What a feed serves when a reader doesn't ask for a size: a portrait phone camera at 30 fps, as
/// #322 measured.
const FEED_SPEC: (u32, u32, u32) = (720, 960, 30);

const SHARED: &str = "an iOS device's back and front cameras read one feed (the shim reads one socket), so both are linked";

const RELAUNCH: &str = "apps that were already running get the camera when relaunched (`simee terminate`, then `simee launch`), and an app \
     needs its camera permission (`simee privacy <udid> grant camera <bundle id>`)";

/// A simulator as the camera links see it.
#[derive(Clone, Debug)]
pub(crate) struct IosTarget {
    pub udid: String,
    pub name: String,
    pub data_dir: PathBuf,
    pub booted: bool,
}

/// The simulators, and `launchctl` inside one: the iOS backend, or a fake in tests.
pub(crate) trait IosHost {
    /// The simulator `query` (a UDID or name) names.
    fn target(&mut self, query: &str) -> Result<IosTarget, String>;
    /// Every simulator.
    fn targets(&mut self) -> Result<Vec<IosTarget>, String>;
    /// Runs the device's own launchctl with `args`: its stdout and stderr.
    fn launchctl(&mut self, udid: &str, args: &[String]) -> Result<(String, String), String>;
}

/// The iOS backend's simulators.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Backend<'a>(&'a mut crate::ios::Session);

#[cfg(target_os = "macos")]
impl IosHost for Backend<'_> {
    fn target(&mut self, query: &str) -> Result<IosTarget, String> {
        crate::ios::camera_device(self.0, query)
    }

    fn targets(&mut self) -> Result<Vec<IosTarget>, String> {
        crate::ios::camera_devices(self.0)
    }

    fn launchctl(&mut self, udid: &str, args: &[String]) -> Result<(String, String), String> {
        crate::ios::camera_launchctl(self.0, udid, args)
    }
}

#[cfg(not(target_os = "macos"))]
impl IosHost for Backend<'_> {
    fn target(&mut self, query: &str) -> Result<IosTarget, String> {
        Err(format!("no device matches `{query}` ({})", crate::ios::REASON))
    }

    fn targets(&mut self) -> Result<Vec<IosTarget>, String> {
        Ok(Vec::new())
    }

    fn launchctl(&mut self, _udid: &str, _args: &[String]) -> Result<(String, String), String> {
        Err(crate::ios::REASON.into())
    }
}

/// A running feed of one simulator: `sim_camera::ios_feed::Feed`, or a fake in tests. Dropping it
/// stops it (and removes its socket).
pub(crate) trait LiveFeed: Send {
    /// The source id it serves now.
    fn source(&self) -> String;
    fn set_source(&self, source: SourceSpec);
    fn path(&self) -> PathBuf;
}

#[cfg(unix)]
impl LiveFeed for sim_camera::ios_feed::Feed {
    fn source(&self) -> String {
        sim_camera::ios_feed::Feed::source(self).to_string()
    }

    fn set_source(&self, source: SourceSpec) {
        sim_camera::ios_feed::Feed::set_source(self, source);
    }

    fn path(&self) -> PathBuf {
        sim_camera::ios_feed::Feed::path(self).to_owned()
    }
}

type Serve = dyn Fn(&Path, SourceSpec) -> Result<Box<dyn LiveFeed>, String> + Send + Sync;

/// The feeds this process serves, one per simulator (keyed by UDID).
pub(crate) struct Feeds {
    serve: Box<Serve>,
    live: Mutex<HashMap<String, Box<dyn LiveFeed>>>,
}

/// This process's feeds, made on first use (a process that never links serves none).
static FEEDS: OnceLock<Arc<Feeds>> = OnceLock::new();

impl Feeds {
    pub(crate) fn new(serve: impl Fn(&Path, SourceSpec) -> Result<Box<dyn LiveFeed>, String> + Send + Sync + 'static) -> Feeds {
        Feeds { serve: Box::new(serve), live: Mutex::new(HashMap::new()) }
    }

    /// This process's feeds: real ones, all reading one capture hub (so two simulators linked to
    /// one source share one capture).
    fn host() -> Arc<Feeds> {
        FEEDS.get_or_init(|| Arc::new(Feeds::new(serve_host_feed))).clone()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Box<dyn LiveFeed>>> {
        self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serves `source` to `udid`'s shim on `socket`: relinks its running feed live (every reader
    /// moves without reconnecting), or starts one. True when a feed was started.
    fn link(&self, udid: &str, socket: &Path, source: SourceSpec) -> Result<bool, String> {
        let mut live = self.lock();
        // A feed whose socket is gone (the device was erased under it) or moved is started anew.
        if let Some(feed) = live.get(udid).filter(|f| f.path() == socket && socket.exists()) {
            feed.set_source(source);
            return Ok(false);
        }
        drop(live.remove(udid));
        let feed = (self.serve)(socket, source)?;
        live.insert(udid.to_owned(), feed);
        Ok(true)
    }

    /// Stops `udid`'s feed; whether one ran.
    fn stop(&self, udid: &str) -> bool {
        let feed = self.lock().remove(udid);
        feed.is_some()
    }

    /// The source `udid`'s feed serves now.
    fn source(&self, udid: &str) -> Option<String> {
        self.lock().get(udid).map(|f| f.source())
    }
}

#[cfg(unix)]
fn serve_host_feed(socket: &Path, source: SourceSpec) -> Result<Box<dyn LiveFeed>, String> {
    static HUB: OnceLock<Arc<sim_camera::Hub>> = OnceLock::new();
    let hub = HUB.get_or_init(|| Arc::new(sim_camera::Hub::new(sim_camera::HostOpener::new()))).clone();
    let (w, h, fps) = FEED_SPEC;
    let spec = sim_camera::FrameSpec::new(w, h, fps).map_err(|e| e.to_string())?;
    let feed = sim_camera::ios_feed::Feed::serve(hub, socket, source, spec).map_err(|e| e.to_string())?;
    Ok(Box::new(feed))
}

#[cfg(not(unix))]
fn serve_host_feed(_socket: &Path, _source: SourceSpec) -> Result<Box<dyn LiveFeed>, String> {
    Err("the iOS camera feed needs Unix sockets".into())
}

/// The socket a simulator's feed listens on (`sim_camera::ios_feed::socket_path`).
#[cfg(unix)]
fn socket_path(data_dir: &Path) -> Result<PathBuf, String> {
    sim_camera::ios_feed::socket_path(data_dir).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn socket_path(data_dir: &Path) -> Result<PathBuf, String> {
    Ok(data_dir.join("cam.sock"))
}

/// The socket variable the shim reads.
#[cfg(unix)]
const SOCKET_ENV: &str = sim_camera::ios_feed::SOCKET_ENV;
#[cfg(not(unix))]
const SOCKET_ENV: &str = "SIMEE_CAMERA_SOCKET";

type Shim = dyn Fn() -> Result<PathBuf, String>;
type Server = dyn Fn(&str, &Value) -> Option<Result<Value, String>>;

/// What an iOS link needs of this process: its feeds, the shim, whether it is a server (whose
/// feeds outlive the call), and the server to hand a call to when it isn't one.
pub(crate) struct Env {
    feeds: Arc<Feeds>,
    shim: Box<Shim>,
    serving: bool,
    /// Calls a running server of this build (`None`: none answers).
    server: Box<Server>,
}

impl Env {
    /// This process's.
    fn host() -> Env {
        Env {
            feeds: Feeds::host(),
            shim: Box::new(|| find_shim(std::env::var_os(SHIM_ENV).map(PathBuf::from), std::env::current_exe().ok())),
            serving: crate::serving(),
            server: Box::new(|method, params| {
                let mut client = crate::Client::handshake(&crate::default_socket(), crate::HANDSHAKE_TIMEOUT).ok()?;
                Some(client.call(method, params))
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(
        feeds: Arc<Feeds>,
        shim: impl Fn() -> Result<PathBuf, String> + 'static,
        serving: bool,
        server: impl Fn(&str, &Value) -> Option<Result<Value, String>> + 'static,
    ) -> Env {
        Env { feeds, shim: Box::new(shim), serving, server: Box::new(server) }
    }

    /// Whether a server answers that a call can be handed to.
    fn server_answers(&self) -> bool {
        (self.server)("identity", &Value::Null).is_some_and(|r| r.is_ok())
    }
}

/// Runs `f` with the camera host, the simulators (the iOS backend's, or the session's fake) and
/// this process's environment (or the session's fake).
fn with_ios<T>(s: &mut Calls, f: impl FnOnce(&dyn Host, &mut dyn IosHost, &Env) -> Result<T, String>) -> Result<T, String> {
    let Calls { ios, camera, .. } = s;
    let own;
    let env = match &camera.env {
        Some(env) => env,
        None => {
            own = Env::host();
            &own
        }
    };
    let host: &dyn Host = &**camera.host.get_or_insert_with(|| Box::new(HostCameras));
    let mut backend;
    let sims: &mut dyn IosHost = match camera.ios.as_deref_mut() {
        Some(fake) => fake,
        None => {
            backend = Backend(ios);
            &mut backend
        }
    };
    f(host, sims, env)
}

/// The shim to insert: `explicit` (`SIMEE_CAMERA_SHIM`), else `ios-camera/` beside the simee
/// executable or one directory up (`target/ios-camera`, where `shims/ios-camera/build.sh` puts it
/// for `target/{debug,release}/simee`). Its hooks must be beside it. A missing shim names the
/// build command.
pub(crate) fn find_shim(explicit: Option<PathBuf>, exe: Option<PathBuf>) -> Result<PathBuf, String> {
    let candidates: Vec<PathBuf> = match explicit {
        Some(path) => vec![std::path::absolute(&path).unwrap_or(path)],
        None => {
            let dir = exe.as_deref().and_then(Path::parent);
            dir.into_iter().flat_map(|d| [d.join("ios-camera").join(SHIM), d.parent().unwrap_or(d).join("ios-camera").join(SHIM)]).collect()
        }
    };
    for shim in &candidates {
        if shim.is_file() && shim.with_file_name(SHIM_HOOKS).is_file() {
            return Ok(std::fs::canonicalize(shim).unwrap_or_else(|_| shim.clone()));
        }
    }
    let looked: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    Err(format!(
        "the iOS camera shim isn't built: run `shims/ios-camera/build.sh` (it writes target/ios-camera/{SHIM} and {SHIM_HOOKS}), or set \
         {SHIM_ENV} to a {SHIM} with {SHIM_HOOKS} beside it; looked for {}",
        if looked.is_empty() { "nothing (this executable's path is unknown)".to_owned() } else { looked.join(", ") }
    ))
}

/// `DYLD_INSERT_LIBRARIES` with the camera shim (any build of it, by file name) taken out, and
/// `shim` put in: another inserted library, the network shaper's, is kept.
fn with_shim(current: &str, shim: Option<&Path>) -> String {
    let mut parts: Vec<String> =
        current.split(':').filter(|p| !p.is_empty() && Path::new(p).file_name().is_none_or(|n| n != SHIM)).map(str::to_owned).collect();
    parts.extend(shim.map(|s| s.display().to_string()));
    parts.join(":")
}

fn getenv(ios: &mut dyn IosHost, udid: &str, key: &str) -> Result<String, String> {
    ios.launchctl(udid, &["getenv".to_owned(), key.to_owned()]).map(|(out, _)| out.trim().to_owned())
}

/// Injects the shim device-wide (launchd's environment, which every process started from now on
/// inherits), keeping another inserted library. True when the environment changed (apps already
/// running don't have the camera until relaunched); false when it was already so.
fn inject(ios: &mut dyn IosHost, udid: &str, shim: &Path, socket: &Path) -> Result<bool, String> {
    let dyld = getenv(ios, udid, DYLD_INSERT)?;
    let want = with_shim(&dyld, Some(shim));
    let want_socket = socket.display().to_string();
    if dyld == want && getenv(ios, udid, SOCKET_ENV)? == want_socket {
        return Ok(false);
    }
    let args = ["setenv", DYLD_INSERT, want.as_str(), SOCKET_ENV, want_socket.as_str()].map(str::to_owned);
    let (_, stderr) = ios.launchctl(udid, &args)?;
    // `exec` reports no exit status: read the environment back.
    if getenv(ios, udid, DYLD_INSERT)? != want || getenv(ios, udid, SOCKET_ENV)? != want_socket {
        return Err(format!("launchctl setenv in the device didn't take: {}", stderr.trim()));
    }
    Ok(true)
}

/// Takes the shim out of the device's launchd environment (another inserted library stays).
fn uninject(ios: &mut dyn IosHost, udid: &str) -> Result<(), String> {
    let dyld = getenv(ios, udid, DYLD_INSERT)?;
    let rest = with_shim(&dyld, None);
    if rest != dyld {
        let args: Vec<String> =
            if rest.is_empty() { vec!["unsetenv".into(), DYLD_INSERT.into()] } else { vec!["setenv".into(), DYLD_INSERT.into(), rest] };
        ios.launchctl(udid, &args)?;
    }
    if !getenv(ios, udid, SOCKET_ENV)?.is_empty() {
        ios.launchctl(udid, &["unsetenv".to_owned(), SOCKET_ENV.to_owned()])?;
    }
    Ok(())
}

/// The error for a booted simulator's link in a process whose feed would end with the call.
fn needs_server(t: &IosTarget) -> String {
    format!(
        "{} is booted, and its camera feed is served by the simee server for as long as it runs, but this call runs in its own \
         process and no server of this build answers: start `simee serve`, then link again (nothing was linked or saved)",
        t.name
    )
}

/// Serves `source` (an id [`resolve_source`] gave, not `none`) to booted `t` and injects the shim:
/// whether the injection changed (running apps need a relaunch). A feed started here is stopped
/// again when the injection fails.
fn serve_live(ios: &mut dyn IosHost, env: &Env, t: &IosTarget, source: &str) -> Result<bool, String> {
    let spec = SourceSpec::parse(source).map_err(|e| e.to_string())?;
    let shim = (env.shim)()?;
    let socket = socket_path(&t.data_dir)?;
    let started = env.feeds.link(&t.udid, &socket, spec)?;
    inject(ios, &t.udid, &shim, &socket).inspect_err(|_| {
        if started {
            env.feeds.stop(&t.udid);
        }
    })
}

/// `camera_link` on a simulator: checks the source (and that the shim is built), serves it live
/// on a booted device (a server only: elsewhere the call is handed to a running server), then
/// saves it for both cameras.
fn ios_link(
    host: &dyn Host,
    ios: &mut dyn IosHost,
    env: &Env,
    query: &str,
    facings: &[&str],
    source: &str,
    p: &Value,
) -> Result<Value, String> {
    let t = ios.target(query)?;
    let resolved = resolve_source(host, source)?;
    let on = resolved.id != "none";
    if on {
        (env.shim)()?;
    }
    let mut notes: Vec<&str> = Vec::new();
    if facings.len() == 1 {
        notes.push(SHARED);
    }
    let applied = if t.booted {
        if !env.serving {
            let mut p = p.clone();
            p["device"] = json!(t.udid);
            return (env.server)("camera_link", &p).unwrap_or_else(|| Err(needs_server(&t)));
        }
        if on {
            if serve_live(ios, env, &t, &resolved.id)? {
                notes.push(RELAUNCH);
            }
        } else {
            env.feeds.stop(&t.udid);
            uninject(ios, &t.udid)?;
        }
        "live"
    } else {
        if !env.serving {
            notes.push(
                "the boot hands the link to the simee server, which serves the camera feed: have `simee serve` running when it boots",
            );
        }
        "at_boot"
    };
    update_cameras(&t.data_dir, |links| {
        links.clear();
        links.extend(CAMERA_FACINGS.iter().map(|f| ((*f).to_owned(), resolved.id.clone())));
    })?;
    let mut out = json!({"device": t.udid, "applied": applied, "links": ios_rows(host, env, &t)});
    if let Some(name) = resolved.name {
        out["name"] = json!(name);
    }
    if !notes.is_empty() {
        out["note"] = json!(notes.join("; "));
    }
    Ok(out)
}

/// `camera_unlink` on a simulator: stops its feed and takes the shim out of its environment
/// (both cameras: they share the feed), then removes the saved links.
fn ios_unlink(ios: &mut dyn IosHost, env: &Env, query: &str, facings: &[&str], p: &Value) -> Result<Value, String> {
    let t = ios.target(query)?;
    if t.booted {
        if env.serving {
            env.feeds.stop(&t.udid);
        } else {
            // The server holds the feed: it stops it. With none, there is no feed to stop.
            let mut p = p.clone();
            p["device"] = json!(t.udid);
            if let Some(r) = (env.server)("camera_unlink", &p) {
                return r;
            }
        }
        uninject(ios, &t.udid)?;
    }
    let unlinked: Vec<String> = saved_cameras(&t.data_dir).into_iter().map(|(f, _)| f).collect();
    update_cameras(&t.data_dir, Vec::clear)?;
    let now: Vec<Value> = CAMERA_FACINGS.iter().map(|f| json!({"facing": f, "source": "none"})).collect();
    let applied = if t.booted { "live" } else { "at_boot" };
    // No camera rows are left; `links` stays in the shape the method returns.
    let mut out = json!({"device": t.udid, "unlinked": unlinked, "now": now, "applied": applied, "links": []});
    if facings.len() == 1 {
        out["note"] = json!("an iOS device's back and front cameras read one feed, so both are unlinked");
    }
    Ok(out)
}

/// `boot`'s camera flags on a simulator: one source (its cameras share one feed), checked and
/// saved like `camera_link`, for the boot to reapply. A boot outside a server needs a running
/// one to hand the link to, so without one the boot is refused before it starts.
fn ios_before_boot(host: &dyn Host, ios: &mut dyn IosHost, env: &Env, query: &str, asked: &[(&str, &str)]) -> Result<(), String> {
    let source = asked[0].1;
    if asked.iter().any(|(_, s)| *s != source) {
        return Err(format!("{SHARED}: give camera_back and camera_front the same source, or one of them"));
    }
    if !env.serving && !env.server_answers() {
        return Err(
            "an iOS device's camera feed is served by the simee server, and no server of this build answers: start `simee serve`, \
             then boot again (nothing was linked or saved)"
                .into(),
        );
    }
    let p = json!({"device": query, "facing": "both", "source": source});
    ios_link(host, ios, env, query, CAMERA_FACINGS.as_slice(), source, &p).map(drop)
}

/// After a simulator booted through simee: serves its saved link and injects the shim, here on a
/// server, or by handing the link to a running server. A failure is reported on stderr (the boot
/// itself succeeded; `camera_links` shows the link as not served).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn ios_booted(ios: &mut crate::ios::Session, udid: &str) {
    if let Err(e) = reapply(&mut Backend(ios), &Env::host(), udid) {
        eprintln!("simee: {udid}'s saved camera link wasn't applied: {e}");
    }
}

/// [`ios_booted`]'s work: what was applied (`None`: no saved link).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn reapply(ios: &mut dyn IosHost, env: &Env, udid: &str) -> Result<Option<Value>, String> {
    let t = ios.target(udid)?;
    let Some((_, source)) = saved_cameras(&t.data_dir).into_iter().next() else { return Ok(None) };
    if source == "none" || !t.booted {
        return Ok(None);
    }
    if env.serving {
        let injected = serve_live(ios, env, &t, &source)?;
        return Ok(Some(json!({"device": t.udid, "source": source, "injected": injected})));
    }
    let p = json!({"device": t.udid, "facing": "both", "source": source});
    match (env.server)("camera_link", &p) {
        Some(r) => r.map(Some),
        None => Err("no simee server (`simee serve`) of this build answers to serve its feed: start one, then link again".into()),
    }
}

/// After a simulator shut down, was erased or deleted: its feed (if this process serves one) stops.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn ios_stopped(udid: &str) {
    if let Some(feeds) = FEEDS.get() {
        feeds.stop(udid);
    }
}

/// One row per saved camera link of simulator `t`, with what its feed serves when this process
/// serves one.
fn ios_rows(host: &dyn Host, env: &Env, t: &IosTarget) -> Vec<Value> {
    let saved = saved_cameras(&t.data_dir);
    let live = if t.booted { env.feeds.source(&t.udid) } else { None };
    let mut cameras: Option<Option<Vec<HostCamera>>> = None;
    let mut rows = Vec::new();
    for facing in CAMERA_FACINGS {
        let Some((_, source)) = saved.iter().find(|(f, _)| f == facing) else { continue };
        let mut row = json!({"device": t.udid, "platform": "ios", "facing": facing, "source": source});
        let mut state = if source == "none" {
            "off"
        } else if !t.booted {
            "saved"
        } else {
            match &live {
                Some(l) => {
                    row["live"] = json!(l);
                    if l == source { "linked" } else { "relinked" }
                }
                None => {
                    row["fix"] = json!(if env.serving {
                        "no feed is served for it now (the device booted outside simee, or the server restarted): link it again, or \
                         reboot it with `simee boot`"
                    } else {
                        "its feed is served by `simee serve`, and this call didn't run on one: start the server, then link again"
                    });
                    "saved"
                }
            }
        };
        if state != "off" {
            let effective = row["live"].as_str().unwrap_or(source.as_str()).to_owned();
            webcam_state(host, &effective, &mut cameras, &mut row, &mut state, "its apps get no frames until it is back");
        }
        row["state"] = json!(state);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use sim_android::testing::{FakeEmulator, fake_emulator};
    use sim_camera::{Authorization, CameraKind, HostCamera};

    use super::Host;
    use crate::methods::{Session, call};

    /// A host whose cameras and permission the test sets.
    #[derive(Clone)]
    struct FakeHost {
        cameras: Arc<Mutex<Vec<HostCamera>>>,
        permission: Arc<Mutex<Authorization>>,
    }

    impl Host for FakeHost {
        fn cameras(&self) -> Result<Vec<HostCamera>, String> {
            Ok(self.cameras.lock().expect("cameras").clone())
        }
        fn permission(&self) -> Result<Authorization, String> {
            Ok(*self.permission.lock().expect("permission"))
        }
    }

    fn camera(id: &str, name: &str, is_virtual: bool) -> HostCamera {
        HostCamera {
            id: id.into(),
            name: name.into(),
            kind: if is_virtual { CameraKind::Virtual } else { CameraKind::BuiltIn },
            is_virtual,
            manufacturer: None,
            model: None,
            transport: None,
            path: None,
            formats: Vec::new(),
        }
    }

    fn host() -> FakeHost {
        FakeHost {
            cameras: Arc::new(Mutex::new(vec![
                camera("FACETIME-1", "FaceTime HD Camera", false),
                camera("OBS-VCAM", "OBS Virtual Camera", true),
                camera("CAMO-1", "Camo Camera", true),
            ])),
            permission: Arc::new(Mutex::new(Authorization::Authorized)),
        }
    }

    struct Fixture {
        root: std::path::PathBuf,
        android: sim_android::Android,
        host: FakeHost,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let root = std::path::PathBuf::from("/tmp").join(format!("sa-cam-{tag}-{}", std::process::id()));
            std::fs::remove_dir_all(&root).ok();
            let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
            std::fs::create_dir_all(&image).expect("image dir");
            std::fs::write(image.join("system.img"), b"s").expect("system.img");
            std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
            let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
            android.running_dir = Some(root.join("running"));
            android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
            Fixture { root, android, host: host() }
        }

        /// A fresh session (a server restart: nothing in memory carries over).
        fn session(&self) -> Session {
            Session { camera: super::Session::with(self.host.clone()), ..Session::with_android(self.android.clone()) }
        }

        fn running(&self) -> FakeEmulator {
            fake_emulator(&self.android, "phone")
        }

        fn saved(&self) -> String {
            std::fs::read_to_string(self.root.join("avd/phone.avd/simee/cameras.ini")).unwrap_or_default()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn run(s: &mut Session, method: &str, p: Value) -> Result<Value, String> {
        call(s, method, &p)
    }

    #[test]
    fn sources_list_host_cameras_with_the_virtual_flag_synthetics_and_the_permission() {
        let f = Fixture::new("sources");
        let mut s = f.session();
        let all = run(&mut s, "camera_sources", json!({})).expect("sources");
        assert_eq!(all["permission"], json!({"status": "authorized", "fix": null}));
        let sources: Vec<&str> = all["sources"].as_array().expect("list").iter().map(|r| r["source"].as_str().expect("id")).collect();
        assert_eq!(sources[..3], ["webcam:FACETIME-1", "webcam:OBS-VCAM", "webcam:CAMO-1"]);
        assert!(sources.contains(&"pattern:testcard") && sources.contains(&"qr:hello"));
        assert_eq!(all["sources"][1]["virtual"], true);
        assert_eq!(all["sources"][0]["virtual"], false);
        let virt = run(&mut s, "camera_sources", json!({"kind": "virtual"})).expect("virtual");
        assert_eq!(virt["sources"].as_array().map(Vec::len), Some(2));
        let synth = run(&mut s, "camera_sources", json!({"kind": "synthetic"})).expect("synthetic");
        assert!(synth["sources"].as_array().expect("list").iter().all(|r| r["kind"] == "synthetic"));
        assert!(run(&mut s, "camera_sources", json!({"kind": "webcams"})).expect_err("kind").contains("unknown kind `webcams`: host"));
        // A missing permission is reported, with its fix, and listing still works (it never prompts).
        *f.host.permission.lock().expect("p") = Authorization::NotDetermined;
        let all = run(&mut s, "camera_sources", json!({})).expect("sources");
        assert_eq!(all["permission"]["status"], "not-determined");
        assert!(all["permission"]["fix"].as_str().expect("fix").contains("restart the simee server"));
    }

    #[test]
    fn a_shut_down_device_saves_the_link_for_its_boot_and_it_survives_a_restart() {
        let f = Fixture::new("saved");
        let mut s = f.session();
        let r = run(&mut s, "camera_link", json!({"device": "phone", "facing": "back", "source": "qr:hello"})).expect("link");
        assert_eq!(r["applied"], "at_boot");
        assert_eq!(
            r["links"],
            json!([{"device": "phone", "platform": "android", "facing": "back", "source": "qr:hello", "state": "saved"}])
        );
        // Part of a camera's name finds it; it is saved by its stable id.
        let r = run(&mut s, "camera_link", json!({"device": "phone", "facing": "front", "source": "obs"})).expect("obs");
        assert_eq!(r["name"], "OBS Virtual Camera");
        assert_eq!(f.saved(), "back=qr:hello\nfront=webcam:OBS-VCAM\n");
        drop(s);
        // A new server process reads the same links back from the device's state.
        let mut s = f.session();
        let rows = run(&mut s, "camera_links", json!({"device": "phone"})).expect("links");
        assert_eq!(rows[1]["source"], "webcam:OBS-VCAM");
        assert_eq!(rows[1]["name"], "OBS Virtual Camera");
        assert_eq!(rows[1]["state"], "saved");
        assert_eq!(run(&mut s, "camera_links", json!({})).expect("all"), rows, "every device's links");
        // Swap, then unlink one and both.
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "both", "source": "emulated"})).expect("swap");
        assert_eq!(f.saved(), "back=pattern:testcard\nfront=pattern:testcard\n");
        let r = run(&mut s, "camera_unlink", json!({"device": "phone", "facing": "front"})).expect("unlink front");
        assert_eq!(r["unlinked"], json!(["front"]));
        assert_eq!(f.saved(), "back=pattern:testcard\n");
        run(&mut s, "camera_unlink", json!({"device": "phone"})).expect("unlink both");
        assert_eq!(f.saved(), "", "no links: no file");
        assert_eq!(run(&mut s, "camera_links", json!({"device": "phone"})).expect("links"), json!([]));
    }

    #[test]
    fn a_running_device_relinks_live_through_the_console() {
        let f = Fixture::new("live");
        let emulator = f.running();
        let mut s = f.session();
        let r = run(&mut s, "camera_link", json!({"device": "phone", "facing": "back", "source": "qr:hello"})).expect("link");
        assert_eq!(r["applied"], "live");
        assert_eq!(r["links"][0]["state"], "linked");
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "both", "source": "camo"})).expect("swap");
        run(&mut s, "camera_unlink", json!({"device": "phone", "facing": "back"})).expect("unlink");
        let console: Vec<String> =
            emulator.console.lock().expect("console").iter().filter(|c| c.starts_with("camera link")).cloned().collect();
        // Unlinked, the back camera goes back to the AVD's own (a new AVD has none).
        assert_eq!(
            console,
            ["camera link back qr:hello", "camera link back webcam:CAMO-1", "camera link front webcam:CAMO-1", "camera link back none"]
        );
        assert_eq!(f.saved(), "front=webcam:CAMO-1\n");
    }

    fn set_avd_cameras(f: &Fixture, back: &str, front: &str) {
        let path = f.root.join("avd/phone.avd/config.ini");
        let mut text = std::fs::read_to_string(&path).expect("config.ini");
        text.push_str(&format!("\nhw.camera.back={back}\nhw.camera.front={front}\n"));
        std::fs::write(&path, text).expect("config.ini");
    }

    #[test]
    fn the_avds_own_default_cameras_are_not_listed_as_links() {
        let f = Fixture::new("defaults");
        set_avd_cameras(&f, "virtualscene", "emulated");
        let emulator = f.running();
        // emu maps both stock values to the test card at boot, and `camera list` says so.
        *emulator.cameras.lock().expect("cameras") =
            vec![("back".into(), "pattern:testcard".into()), ("front".into(), "pattern:testcard".into())];
        let mut s = f.session();
        assert_eq!(run(&mut s, "camera_links", json!({"device": "phone"})).expect("links"), json!([]), "nobody linked these");
        // A link made from the console only is still shown.
        emulator.cameras.lock().expect("cameras")[0].1 = "qr:console".into();
        let rows = run(&mut s, "camera_links", json!({"device": "phone"})).expect("links");
        assert_eq!((rows.as_array().map(Vec::len), rows[0]["state"].clone()), (Some(1), json!("unsaved")));
        // A saved link shows, and after the unlink the camera is on its default again: no row.
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "front", "source": "solid:red"})).expect("link");
        let r = run(&mut s, "camera_unlink", json!({"device": "phone", "facing": "front"})).expect("unlink");
        assert_eq!(r["now"], json!([{"facing": "front", "source": "emulated"}]));
        assert_eq!(r["links"].as_array().map(Vec::len), Some(1), "only the console-linked back camera: {}", r["links"]);
        assert_eq!(r["links"][0]["facing"], "back");
    }

    #[test]
    fn a_source_whose_whitespace_the_console_would_change_is_refused() {
        let f = Fixture::new("whitespace");
        let emulator = f.running();
        let mut s = f.session();
        for source in ["qr:a  b", "text:a\tb", "qr:a\u{a0}b", "text:a\u{7}b"] {
            let err = run(&mut s, "camera_link", json!({"device": "phone", "facing": "back", "source": source})).expect_err(source);
            assert!(err.contains("whitespace") || err.contains("control"), "{source:?}: {err}");
        }
        assert_eq!(f.saved(), "");
        assert!(emulator.console.lock().expect("console").iter().all(|c| !c.starts_with("camera link")), "nothing reached the console");
        // A single space is kept as it is, live and at boot.
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "back", "source": "qr:a b"})).expect("single space");
        assert_eq!(f.saved(), "back=qr:a b\n");
        assert_eq!(emulator.cameras.lock().expect("cameras")[0].1, "qr:a b");
    }

    #[test]
    fn both_rolls_the_back_camera_back_when_the_front_relink_fails() {
        let f = Fixture::new("both-fails");
        let emulator = f.running();
        *emulator.cameras.lock().expect("cameras") = vec![("back".into(), "qr:old".into()), ("front".into(), "solid:red".into())];
        *emulator.camera_link_fails.lock().expect("fails") = Some("front".into());
        let mut s = f.session();
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "back", "source": "qr:old"})).expect("saved back");
        assert_eq!(f.saved(), "back=qr:old\n");
        let err = run(&mut s, "camera_link", json!({"device": "phone", "facing": "both", "source": "qr:new"})).expect_err("front fails");
        assert!(err.contains("camera link front qr:new") && err.contains("back camera was put back on qr:old"), "{err}");
        assert_eq!(f.saved(), "back=qr:old\n", "nothing new was saved");
        let live = emulator.cameras.lock().expect("cameras").clone();
        assert!(live.contains(&("back".into(), "qr:old".into())), "the back camera is on its previous source: {live:?}");
        let console: Vec<String> =
            emulator.console.lock().expect("console").iter().filter(|c| c.starts_with("camera link")).cloned().collect();
        assert_eq!(
            console[1..],
            ["camera link back qr:new", "camera link front qr:new", "camera link back qr:old"],
            "back, front (fails), back restored"
        );
        // Both succeeding still links and saves both.
        *emulator.camera_link_fails.lock().expect("fails") = None;
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "both", "source": "qr:new"})).expect("both");
        assert_eq!(f.saved(), "back=qr:new\nfront=qr:new\n");
    }

    #[test]
    fn boot_links_its_camera_flags_first() {
        let f = Fixture::new("boot");
        let emulator = f.running();
        let mut s = f.session();
        // The links are applied and saved before the boot itself runs (stopped here by an iOS-only
        // parameter, so no boot wait asks a real adb about the fake).
        let p = json!({"device": "phone", "camera_back": "solid:orange", "camera_front": "qr:front", "settle": true});
        let err = run(&mut s, "boot", p).expect_err("settle is iOS's");
        assert!(err.contains("for iOS devices only"), "{err}");
        assert_eq!(f.saved(), "back=solid:orange\nfront=qr:front\n");
        let console: Vec<String> =
            emulator.console.lock().expect("console").iter().filter(|c| c.starts_with("camera link")).cloned().collect();
        assert_eq!(console, ["camera link back solid:orange", "camera link front qr:front"]);
        let err = run(&mut s, "boot", json!({"device": "phone", "camera_back": "video:/x.mp4"})).expect_err("unsupported");
        assert!(err.contains("aren't supported yet"), "{err}");
    }

    #[test]
    fn errors_name_the_fix() {
        let f = Fixture::new("errors");
        let mut s = f.session();
        let link = |s: &mut Session, source: &str| run(s, "camera_link", json!({"device": "phone", "facing": "back", "source": source}));
        let err = link(&mut s, "camera").expect_err("ambiguous");
        assert!(err.contains("matches more than one camera") && err.contains("OBS Virtual Camera (OBS-VCAM)"), "{err}");
        let err = link(&mut s, "brio").expect_err("absent");
        assert!(err.contains("no camera matches \"brio\"") && err.contains("pattern:testcard"), "{err}");
        let err = link(&mut s, "image:photo.jpg").expect_err("relative");
        assert!(err.contains("absolute path"), "{err}");
        let err = link(&mut s, "solid:nope").expect_err("colour");
        assert!(!err.is_empty());
        assert!(run(&mut s, "camera_link", json!({"device": "phone", "facing": "side", "source": "qr:x"})).is_err());
        *f.host.permission.lock().expect("p") = Authorization::Denied;
        let err = link(&mut s, "obs").expect_err("denied");
        assert!(err.contains("camera permission is denied") && err.contains("System Settings"), "{err}");
        link(&mut s, "qr:no permission needed").expect("synthetic sources need none");
        assert_eq!(f.saved(), "back=qr:no permission needed\n", "only the good link was saved");
    }

    #[test]
    fn an_unplugged_camera_is_source_missing() {
        let f = Fixture::new("missing");
        let mut s = f.session();
        run(&mut s, "camera_link", json!({"device": "phone", "facing": "back", "source": "obs"})).expect("link");
        f.host.cameras.lock().expect("cameras").retain(|c| c.id != "OBS-VCAM");
        let rows = run(&mut s, "camera_links", json!({"device": "phone"})).expect("links");
        assert_eq!(rows[0]["state"], "source missing");
        assert!(rows[0]["fix"].as_str().expect("fix").contains("test card"));
        f.host.cameras.lock().expect("cameras").push(camera("OBS-VCAM", "OBS Virtual Camera", true));
        assert_eq!(run(&mut s, "camera_links", json!({"device": "phone"})).expect("links")[0]["state"], "saved");
    }

    #[test]
    fn the_docs_and_schemas_agree_with_the_method_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(api) = std::fs::read_to_string(root.join("docs/api.md")) else {
            eprintln!("skipped: not in the simee workspace, so docs/api.md isn't here");
            return;
        };
        let cli = std::fs::read_to_string(root.join("docs/cli.md")).expect("docs/cli.md");
        let camera = std::fs::read_to_string(root.join("docs/camera.md")).expect("docs/camera.md");
        for (method, params, doc) in crate::METHODS.iter().filter(|(m, ..)| super::is_method(m) || *m == "boot") {
            let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
            let row = format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.contains(&row), "docs/api.md's row for `{method}` differs from METHODS; it should be\n{row}");
            let named = camera.contains(&format!("`{method}`")) || camera.contains(&format!("`{method} {{"));
            assert!(named, "docs/camera.md doesn't name `{method}`");
            assert!(crate::output_schema(method).is_some(), "{method} has no output schema");
            let schema = crate::input_schema(method).expect("input schema");
            for (name, prop) in schema["properties"].as_object().expect("properties") {
                assert!(prop.get("type").is_some(), "{method}'s `{name}` has no type in schema.rs");
            }
        }
        assert_eq!(crate::input_schema("camera_link").expect("schema")["required"], json!(["device", "facing", "source"]));
        assert_eq!(crate::input_schema("camera_unlink").expect("schema")["properties"]["facing"]["enum"], json!(["back", "front", "both"]));
        for command in
            ["simee camera sources", "simee camera link", "simee camera unlink", "simee camera links", "--camera-back", "--camera-front"]
        {
            assert!(cli.contains(command) && camera.contains(command), "docs/cli.md and docs/camera.md show `{command}`");
        }
    }

    #[test]
    fn camera_sources_needs_no_device() {
        use sim_core::platform::Platform;
        assert_eq!(crate::platforms("camera_sources"), Platform::ALL);
    }

    // ---- iOS: a fake simulator (its launchd environment) and fake feeds ----

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use super::{Env, Feeds, IosHost, IosTarget, LiveFeed};

    const UDID: &str = "0053A2F9-1FD0-4409-BCF1-6D32F934C172";
    const SHAPER: &str = "/opt/netshape/libsimee-netshape.dylib";

    /// One simulator: its state, and the environment its launchd holds; every launchctl call.
    #[derive(Clone)]
    struct FakeIos {
        target: Arc<Mutex<IosTarget>>,
        env: Arc<Mutex<HashMap<String, String>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl IosHost for FakeIos {
        fn target(&mut self, query: &str) -> Result<IosTarget, String> {
            let t = self.target.lock().expect("target").clone();
            if query == t.udid || query == t.name { Ok(t) } else { Err(format!("no device matches `{query}`")) }
        }
        fn targets(&mut self) -> Result<Vec<IosTarget>, String> {
            Ok(vec![self.target.lock().expect("target").clone()])
        }
        fn launchctl(&mut self, udid: &str, args: &[String]) -> Result<(String, String), String> {
            assert_eq!(udid, UDID);
            let mut env = self.env.lock().expect("env");
            let out = match args[0].as_str() {
                "getenv" => env.get(&args[1]).map(|v| format!("{v}\n")).unwrap_or_default(),
                "setenv" => {
                    for kv in args[1..].chunks(2) {
                        env.insert(kv[0].clone(), kv[1].clone());
                    }
                    String::new()
                }
                "unsetenv" => {
                    env.remove(&args[1]);
                    String::new()
                }
                other => panic!("launchctl {other}"),
            };
            if args[0] != "getenv" {
                self.calls.lock().expect("calls").push(args.join(" "));
            }
            Ok((out, String::new()))
        }
    }

    /// A feed that makes its socket file, and logs serve, relink and stop.
    struct FakeFeed {
        path: PathBuf,
        source: Mutex<String>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl LiveFeed for FakeFeed {
        fn source(&self) -> String {
            self.source.lock().expect("source").clone()
        }
        fn set_source(&self, source: sim_camera::SourceSpec) {
            self.log.lock().expect("log").push(format!("relink {source}"));
            *self.source.lock().expect("source") = source.to_string();
        }
        fn path(&self) -> PathBuf {
            self.path.clone()
        }
    }

    impl Drop for FakeFeed {
        fn drop(&mut self) {
            self.log.lock().expect("log").push("stop".into());
            std::fs::remove_file(&self.path).ok();
        }
    }

    type Forwarded = Arc<Mutex<Vec<(String, Value)>>>;

    struct Ios {
        f: Fixture,
        sim: FakeIos,
        feeds: Arc<Feeds>,
        log: Arc<Mutex<Vec<String>>>,
        shim: PathBuf,
        forwarded: Forwarded,
    }

    impl Ios {
        fn new(tag: &str, booted: bool) -> Ios {
            let f = Fixture::new(&format!("ios-{tag}"));
            let data_dir = f.root.join("ios/data");
            std::fs::create_dir_all(&data_dir).expect("data dir");
            let shim = f.root.join("ios-camera/libsimee-camera.dylib");
            let target = IosTarget { udid: UDID.into(), name: "iPhone 17".into(), data_dir, booted };
            let sim = FakeIos { target: Arc::new(Mutex::new(target)), env: Arc::default(), calls: Arc::default() };
            let log: Arc<Mutex<Vec<String>>> = Arc::default();
            let feed_log = log.clone();
            let feeds = Arc::new(Feeds::new(move |path: &Path, source: sim_camera::SourceSpec| {
                std::fs::write(path, b"").map_err(|e| e.to_string())?;
                feed_log.lock().expect("log").push(format!("serve {source} on {}", path.display()));
                Ok(Box::new(FakeFeed { path: path.to_owned(), source: Mutex::new(source.to_string()), log: feed_log.clone() })
                    as Box<dyn LiveFeed>)
            }));
            Ios { f, sim, feeds, log, shim, forwarded: Arc::default() }
        }

        fn socket(&self) -> String {
            self.sim.target.lock().expect("target").data_dir.join("cam.sock").display().to_string()
        }

        /// The environment: serving or not, a server that answers or none, the shim built or not.
        fn env(&self, serving: bool, server: bool, shim: bool) -> Env {
            let path = self.shim.clone();
            let forwarded = self.forwarded.clone();
            Env::fake(
                self.feeds.clone(),
                move || {
                    if shim { Ok(path.clone()) } else { Err("the iOS camera shim isn't built: run `shims/ios-camera/build.sh`".into()) }
                },
                serving,
                move |method, p| {
                    server.then(|| {
                        forwarded.lock().expect("forwarded").push((method.to_owned(), p.clone()));
                        Ok(json!({"forwarded": method}))
                    })
                },
            )
        }

        fn session(&self, env: Env) -> Session {
            let camera =
                super::Session { host: Some(Box::new(self.f.host.clone())), ios: Some(Box::new(self.sim.clone())), env: Some(env) };
            Session { camera, ..Session::with_android(self.f.android.clone()) }
        }

        fn saved(&self) -> String {
            let dir = self.sim.target.lock().expect("target").data_dir.clone();
            std::fs::read_to_string(dir.join("simee/cameras.ini")).unwrap_or_default()
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().expect("log"))
        }

        fn take_calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.sim.calls.lock().expect("calls"))
        }

        fn launchd(&self, key: &str) -> Option<String> {
            self.sim.env.lock().expect("env").get(key).cloned()
        }
    }

    #[test]
    fn an_ios_link_serves_a_feed_injects_the_shim_relinks_live_and_unlinks() {
        let ios = Ios::new("live", true);
        // The network shaper is injected already: the camera joins it.
        ios.sim.env.lock().expect("env").insert("DYLD_INSERT_LIBRARIES".into(), SHAPER.into());
        let mut s = ios.session(ios.env(true, false, true));
        let r = run(&mut s, "camera_link", json!({"device": UDID, "facing": "back", "source": "qr:hello"})).expect("link");
        assert_eq!(r["applied"], "live");
        let note = r["note"].as_str().expect("note");
        assert!(note.contains("both are linked") && note.contains("relaunched") && note.contains("simee privacy"), "{note}");
        assert_eq!(ios.take_log(), [format!("serve qr:hello on {}", ios.socket())]);
        let dyld = format!("{SHAPER}:{}", ios.shim.display());
        assert_eq!(ios.take_calls(), [format!("setenv DYLD_INSERT_LIBRARIES {dyld} SIMEE_CAMERA_SOCKET {}", ios.socket())]);
        assert_eq!(ios.saved(), "back=qr:hello\nfront=qr:hello\n", "both cameras: they read one feed");
        let rows = r["links"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            json!({"device": UDID, "platform": "ios", "facing": "back", "source": "qr:hello", "live": "qr:hello", "state": "linked"})
        );

        // A relink moves the running feed: no new feed, no launchctl, no relaunch note.
        let r = run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "emulated"})).expect("relink");
        assert_eq!(ios.take_log(), ["relink pattern:testcard"]);
        assert!(ios.take_calls().is_empty());
        assert!(r.get("note").is_none(), "{r}");
        assert_eq!(ios.saved(), "back=pattern:testcard\nfront=pattern:testcard\n");

        // The device was erased under the feed (its socket is gone): the next link starts one anew.
        std::fs::remove_file(ios.socket()).expect("socket");
        run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "solid:red"})).expect("after erase");
        assert_eq!(ios.take_log(), ["stop".to_owned(), format!("serve solid:red on {}", ios.socket())]);

        // Unlinking one camera unlinks both; the shaper stays inserted.
        let r = run(&mut s, "camera_unlink", json!({"device": UDID, "facing": "front"})).expect("unlink");
        assert_eq!(r["unlinked"], json!(["back", "front"]));
        assert_eq!(r["now"], json!([{"facing": "back", "source": "none"}, {"facing": "front", "source": "none"}]));
        assert_eq!(ios.take_log(), ["stop"]);
        assert_eq!(ios.take_calls(), [format!("setenv DYLD_INSERT_LIBRARIES {SHAPER}"), "unsetenv SIMEE_CAMERA_SOCKET".to_owned()]);
        assert_eq!((ios.launchd("DYLD_INSERT_LIBRARIES"), ios.launchd("SIMEE_CAMERA_SOCKET")), (Some(SHAPER.into()), None));
        assert_eq!(ios.saved(), "");
        assert_eq!(run(&mut s, "camera_links", json!({"device": UDID})).expect("links"), json!([]));

        // `none` turns the camera off: no feed, no injection, saved as off.
        let r = run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "none"})).expect("none");
        assert_eq!(r["links"][0]["state"], "off");
        assert!(ios.take_log().is_empty());
        assert_eq!(ios.launchd("DYLD_INSERT_LIBRARIES"), Some(SHAPER.into()));
    }

    #[test]
    fn a_saved_ios_link_is_reapplied_when_the_device_boots() {
        let ios = Ios::new("boot", false);
        let mut s = ios.session(ios.env(true, false, true));
        let r = run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "obs"})).expect("link");
        assert_eq!((r["applied"].clone(), r["name"].clone()), (json!("at_boot"), json!("OBS Virtual Camera")));
        assert!(ios.take_log().is_empty() && ios.take_calls().is_empty(), "nothing runs for a shut-down device");
        drop(s);
        // A server restart: the link is read back from the device's data directory.
        let mut s = ios.session(ios.env(true, false, true));
        let rows = run(&mut s, "camera_links", json!({})).expect("every device");
        let ios_rows: Vec<&Value> = rows.as_array().expect("rows").iter().filter(|r| r["platform"] == "ios").collect();
        assert_eq!(ios_rows.len(), 2);
        assert_eq!((ios_rows[0]["state"].clone(), ios_rows[0]["name"].clone()), (json!("saved"), json!("OBS Virtual Camera")));

        // The device boots: on a server, the feed starts and the shim is injected.
        ios.sim.target.lock().expect("target").booted = true;
        let env = ios.env(true, false, true);
        let mut sim = ios.sim.clone();
        let r = super::reapply(&mut sim, &env, UDID).expect("reapply").expect("a saved link");
        assert_eq!(r["injected"], true);
        assert_eq!(ios.take_log(), [format!("serve webcam:OBS-VCAM on {}", ios.socket())]);
        assert_eq!(ios.launchd("SIMEE_CAMERA_SOCKET"), Some(ios.socket()));
        assert_eq!(run(&mut s, "camera_links", json!({"device": UDID})).expect("links")[0]["state"], "linked");
        // Booted again (a `boot` of a booted device): the same feed, nothing to inject.
        let r = super::reapply(&mut sim, &env, UDID).expect("again").expect("link");
        assert_eq!(r["injected"], false);
        assert_eq!(ios.take_log(), ["relink webcam:OBS-VCAM"]);

        // A boot outside a server hands the link to a running one, or says it needs one.
        let r = super::reapply(&mut sim, &ios.env(false, true, true), UDID).expect("forwarded").expect("link");
        assert_eq!(r, json!({"forwarded": "camera_link"}));
        let forwarded = ios.forwarded.lock().expect("forwarded").clone();
        assert_eq!(forwarded, [("camera_link".to_owned(), json!({"device": UDID, "facing": "both", "source": "webcam:OBS-VCAM"}))]);
        let err = super::reapply(&mut sim, &ios.env(false, false, true), UDID).expect_err("no server");
        assert!(err.contains("simee serve"), "{err}");
        // A device with no saved link: nothing to do.
        run(&mut s, "camera_unlink", json!({"device": UDID})).expect("unlink");
        assert_eq!(super::reapply(&mut sim, &env, UDID), Ok(None));
    }

    #[test]
    fn a_booted_ios_link_outside_a_server_goes_to_the_server_or_needs_one() {
        let ios = Ios::new("client", true);
        // No server: refused, and nothing changes.
        let mut s = ios.session(ios.env(false, false, true));
        let err = run(&mut s, "camera_link", json!({"device": UDID, "facing": "back", "source": "qr:x"})).expect_err("no server");
        assert!(err.contains("start `simee serve`") && err.contains("nothing was linked or saved"), "{err}");
        assert!(ios.take_log().is_empty() && ios.take_calls().is_empty());
        assert_eq!(ios.saved(), "");
        // A running server of this build gets the call, with the device by its UDID.
        let mut s = ios.session(ios.env(false, true, true));
        let r = run(&mut s, "camera_link", json!({"device": UDID, "facing": "back", "source": "qr:x"})).expect("forwarded");
        assert_eq!(r, json!({"forwarded": "camera_link"}));
        let forwarded = ios.forwarded.lock().expect("forwarded").clone();
        assert_eq!(forwarded, [("camera_link".to_owned(), json!({"device": UDID, "facing": "back", "source": "qr:x"}))]);
        assert_eq!(ios.saved(), "", "the server saves it");
        // A shut-down device's link is only saved, so it needs no server; the note says the boot will.
        ios.sim.target.lock().expect("target").booted = false;
        let mut s = ios.session(ios.env(false, false, true));
        let r = run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "qr:x"})).expect("saved");
        assert!(r["note"].as_str().expect("note").contains("`simee serve` running when it boots"), "{r}");
        assert_eq!(ios.saved(), "back=qr:x\nfront=qr:x\n");
        // Unlinking a booted device with no server takes the shim out of its environment itself.
        ios.sim.target.lock().expect("target").booted = true;
        ios.sim.env.lock().expect("env").insert("SIMEE_CAMERA_SOCKET".into(), ios.socket());
        run(&mut s, "camera_unlink", json!({"device": UDID})).expect("unlink");
        assert_eq!(ios.take_calls(), ["unsetenv SIMEE_CAMERA_SOCKET"]);
        assert_eq!(ios.saved(), "");
    }

    #[test]
    fn a_missing_shim_names_the_build_command() {
        let ios = Ios::new("shim", true);
        let mut s = ios.session(ios.env(true, false, false));
        let err = run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "qr:x"})).expect_err("no shim");
        assert!(err.contains("shims/ios-camera/build.sh"), "{err}");
        assert!(ios.take_log().is_empty() && ios.take_calls().is_empty());
        assert_eq!(ios.saved(), "");
        // Turning the camera off needs no shim.
        run(&mut s, "camera_link", json!({"device": UDID, "facing": "both", "source": "none"})).expect("off");

        // Where the shim is looked for: beside the executable, one directory up (target/ios-camera
        // for target/release/simee), or the path SIMEE_CAMERA_SHIM gives; its hooks must be beside it.
        let root = ios.f.root.join("find");
        let exe = root.join("target/release/simee");
        let err = super::find_shim(None, Some(exe.clone())).expect_err("not built");
        assert!(err.contains("shims/ios-camera/build.sh") && err.contains("SIMEE_CAMERA_SHIM"), "{err}");
        assert!(err.contains(&root.join("target/ios-camera/libsimee-camera.dylib").display().to_string()), "{err}");
        let built = root.join("target/ios-camera");
        std::fs::create_dir_all(&built).expect("dir");
        std::fs::write(built.join("libsimee-camera.dylib"), b"").expect("shim");
        assert!(super::find_shim(None, Some(exe.clone())).is_err(), "the hooks must be beside it");
        std::fs::write(built.join("libsimee-camera-hooks.dylib"), b"").expect("hooks");
        let found = super::find_shim(None, Some(exe)).expect("found");
        assert!(found.is_absolute() && found.ends_with("target/ios-camera/libsimee-camera.dylib"), "{}", found.display());
        assert_eq!(super::find_shim(Some(built.join("libsimee-camera.dylib")), None).ok(), Some(found));
        assert!(super::find_shim(Some(root.join("nope.dylib")), None).is_err());
        assert!(super::find_shim(None, None).expect_err("no exe").contains("unknown"));
    }

    #[test]
    fn boot_flags_on_ios_give_one_source_and_need_a_server() {
        let ios = Ios::new("flags", false);
        let mut s = ios.session(ios.env(true, false, true));
        let p = json!({"device": UDID, "camera_back": "qr:a", "camera_front": "qr:b"});
        let err = super::before_boot(&mut s, &p).expect_err("two sources");
        assert!(err.contains("read one feed"), "{err}");
        super::before_boot(&mut s, &json!({"device": UDID, "camera_front": "qr:a"})).expect("one");
        assert_eq!(ios.saved(), "back=qr:a\nfront=qr:a\n");
        // A boot that runs outside a server needs one to hand the link to at boot.
        let mut s = ios.session(ios.env(false, false, true));
        let err = super::before_boot(&mut s, &json!({"device": UDID, "camera_back": "qr:c"})).expect_err("no server");
        assert!(err.contains("simee serve"), "{err}");
        assert_eq!(ios.saved(), "back=qr:a\nfront=qr:a\n", "unchanged");
        let mut s = ios.session(ios.env(false, true, true));
        super::before_boot(&mut s, &json!({"device": UDID, "camera_back": "qr:c"})).expect("a server answers");
        assert_eq!(ios.saved(), "back=qr:c\nfront=qr:c\n");
    }

    #[test]
    fn the_shim_joins_another_inserted_library_and_leaves_it_behind() {
        let shim = Path::new("/b/libsimee-camera.dylib");
        assert_eq!(super::with_shim("", Some(shim)), "/b/libsimee-camera.dylib");
        assert_eq!(super::with_shim(SHAPER, Some(shim)), format!("{SHAPER}:/b/libsimee-camera.dylib"));
        // An older build's path is replaced, not added to.
        assert_eq!(
            super::with_shim(&format!("/a/libsimee-camera.dylib:{SHAPER}"), Some(shim)),
            format!("{SHAPER}:/b/libsimee-camera.dylib")
        );
        assert_eq!(super::with_shim(&format!("{SHAPER}:/b/libsimee-camera.dylib"), None), SHAPER);
        assert_eq!(super::with_shim("/b/libsimee-camera.dylib", None), "");
    }
}
