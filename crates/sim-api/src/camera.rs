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

use serde_json::{Value, json};
use sim_android::{CAMERA_FACINGS, Device, save_cameras, saved_cameras};
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

// TODO(#322): link iOS device cameras once the iOS camera feed lands.
const IOS_UNSUPPORTED: &str = "camera links on iOS devices come with the iOS camera feed (#322), not yet; \
     Android devices link through emu's camera HAL";

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

/// The camera host a connection reads, made on first use.
#[derive(Default)]
pub struct Session {
    host: Option<Box<dyn Host>>,
}

impl Session {
    #[cfg(test)]
    pub fn with(host: impl Host + 'static) -> Session {
        Session { host: Some(Box::new(host)) }
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
        Session { host: Some(Box::new(Fixed { cameras, permission })) }
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
                Ok(Value::Array(rows))
            }
            Some(_) => {
                let d = android_device(s, p)?;
                Ok(Value::Array(link_rows(s.camera.host(), &d)))
            }
        },
        "camera_link" => {
            let facings = facings(Some(need_str(p, "facing")?))?;
            let source = need_str(p, "source")?;
            let d = android_device(s, p)?;
            link(s.camera.host(), &d, &facings, source)
        }
        "camera_unlink" => {
            let facings = facings(str_param(p, "facing"))?;
            let d = android_device(s, p)?;
            unlink(s.camera.host(), &d, &facings)
        }
        other => Err(format!("unknown camera method `{other}`")),
    }
}

/// `boot`'s `camera_back` / `camera_front`: linked (and saved) before the boot, so the boot
/// passes them to emu. Nothing when neither is given.
pub fn before_boot(s: &mut Calls, p: &Value) -> Result<(), String> {
    let asked: Vec<(&str, &str)> =
        [("back", "camera_back"), ("front", "camera_front")].into_iter().filter_map(|(f, k)| Some((f, str_param(p, k)?))).collect();
    if asked.is_empty() {
        return Ok(());
    }
    let d = android_device(s, p)?;
    for (facing, source) in asked {
        link(s.camera.host(), &d, &[facing], source)?;
    }
    Ok(())
}

/// The Android device a call names; an iOS one is refused (#322).
fn android_device(s: &mut Calls, p: &Value) -> Result<Device, String> {
    let (platform, udid) = crate::methods::resolve(s, p)?;
    if platform == Platform::Ios {
        crate::methods::ios_device_exists(s, platform, str_param(p, "device").unwrap_or("booted"))?;
        return Err(IOS_UNSUPPORTED.into());
    }
    s.android.backend().device(&udid)
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
    let (dir, mut links) = saved(d)?;
    let running = simee_or_off(d)?;
    let mut note = None;
    if running {
        let before = live(d);
        for facing in facings {
            relink(d, facing, &resolved.id)?;
            let was_off = before.as_ref().is_some_and(|b| b.iter().any(|(f, s)| f == facing && s == "none"));
            if was_off && resolved.id != "none" {
                note = Some(format!(
                    "the guest lists its cameras once, at boot: reboot {} for its new {facing} camera to appear (the link is saved for that boot)",
                    d.info.name
                ));
            }
        }
    }
    for facing in facings {
        links.retain(|(f, _)| f != facing);
        links.push(((*facing).to_owned(), resolved.id.clone()));
    }
    links.sort_by_key(|(f, _)| CAMERA_FACINGS.iter().position(|c| c == f));
    save_cameras(&dir, &links)?;
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

/// `camera_unlink`: removes the links; a running device goes back to the AVD's own cameras live.
fn unlink(host: &dyn Host, d: &Device, facings: &[&str]) -> Result<Value, String> {
    let (dir, mut links) = saved(d)?;
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
    links.retain(|(f, _)| !facings.contains(&f.as_str()));
    save_cameras(&dir, &links)?;
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
        } else if let Some(query) = effective.strip_prefix("webcam:") {
            let cams = cameras.get_or_insert_with(|| host.cameras().ok());
            match cams.as_deref().map(|c| sim_camera::resolve_webcam(query, c)) {
                Some(Ok(c)) => {
                    row["name"] = json!(c.name);
                    if let Ok(status) = host.permission()
                        && let Some(fix) = status.fix()
                    {
                        state = "permission missing";
                        row["fix"] = json!(fix);
                    }
                }
                Some(Err(sim_camera::Error::NotFound { .. })) => {
                    state = "source missing";
                    row["fix"] = json!(
                        "the host doesn't list this camera now (unplugged, or its app quit): the device shows the test card until it is back, or link another source"
                    );
                }
                _ => {}
            }
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
    fn camera_links_are_android_methods_until_the_ios_feed() {
        // An iOS device is refused with IOS_UNSUPPORTED (reaching one needs the simulator service,
        // which unit tests don't start).
        assert!(super::IOS_UNSUPPORTED.contains("#322"));
        use sim_core::platform::Platform;
        assert_eq!(crate::platforms("camera_sources"), Platform::ALL);
        for m in ["camera_link", "camera_unlink", "camera_links"] {
            assert_eq!(crate::platforms(m), [Platform::Android], "{m}");
        }
    }
}
