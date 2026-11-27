//! The Android backend, client side. Android devices are AVDs (the SDK's own format, read by
//! `emu-avd`) run by the `emu` emulator binary, one process per device: Hypervisor.framework
//! allows one VM per process, and a crashed guest can't take the window or the API server down.
//!
//! This crate starts and stops `emu`, and drives a running instance the way any tool can: over
//! its console port (the stock text protocol `adb emu` speaks) and its display socket
//! (`sim_core::stream`). It has no FFI, so it builds and is tested on every host.
//!
//! An AVD's name is its identifier (the `udid` of its [`DeviceInfo`]): AVD names are unique within
//! an AVD home, and Android has no other stable id for a device that isn't running.

pub mod apps;
pub mod console;
pub mod display;
pub mod listing;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The SDK and AVD home an [`Android`] works over (`Android::env`).
pub use emu_avd::Env;
/// AVD export and import results ([`Android::export_avd`], [`Android::import_avd`]).
pub use emu_avd::bundle;
use emu_avd::{Avd, NewAvd, Running};
use sim_core::device::{DeviceInfo, DeviceState, RuntimeInfo, Selection, select};
use sim_core::form_factor::{FormFactor, InputMethod, Kind, Profile, android_profile};
use sim_core::frame::Frame;
use sim_core::gesture::{Phase, Sample, pace, pace_samples};
use sim_core::hid::{Button, Direction, KEY_LEFT_SHIFT, KeyStroke, keystroke, linux_keycode};
use sim_core::platform::{AndroidOs, Platform, SubPlatform};
use sim_core::rotation::Rotation;
use sim_core::stream::Input;
use sim_core::vhal;
use sim_core::xr::XrCommand;

/// The Android backend over one SDK and AVD home.
#[derive(Clone, Debug)]
pub struct Android {
    pub env: Env,
    /// Where running emulators advertise themselves (default: [`emu_avd::running_dir`]).
    pub running_dir: Option<PathBuf>,
    /// Stock emulators found by earlier lookups, so a listing asks `ps` only about new ones.
    stock: std::sync::Arc<listing::StockCache>,
}

impl Default for Android {
    fn default() -> Self {
        Self::from_env()
    }
}

/// How long a launched emulator that didn't boot gets to quit after SIGINT before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Stops a child this crate started: SIGINT (emu shuts down in order, like `kill -INT <pid>`),
/// up to `grace` for it to exit, then SIGKILL; reaped either way.
fn stop_child(child: &mut std::process::Child, grace: Duration) -> Result<(), String> {
    if child.try_wait().map_err(|e| e.to_string())?.is_some() {
        return Ok(());
    }
    let _ = Command::new("kill").args(["-INT", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Ok(());
        }
        sleep(Duration::from_millis(50));
    }
    child.kill().map_err(|e| e.to_string())?;
    child.wait().map(|_| ()).map_err(|e| e.to_string())
}

/// A running emulator answers its console within this long of advertising itself.
const CONSOLE_GRACE: Duration = Duration::from_secs(5);

/// How often a boot wait looks at the child and the log.
const BOOT_POLL: Duration = Duration::from_millis(50);
/// How often adb is asked while the log hasn't said the guest booted (emulators that never say).
const ADB_FALLBACK: Duration = Duration::from_secs(1);
/// How often adb is asked when it's the only signal, or after the log said boot completed but
/// adb didn't confirm yet.
const ADB_RETRY: Duration = Duration::from_millis(250);

/// Follows a log file from a start offset for emu's `emu: boot completed` line, reading only
/// what was appended since the last look.
struct LogTail {
    path: PathBuf,
    offset: u64,
    /// The unfinished last line of what has been read.
    partial: Vec<u8>,
}

impl LogTail {
    fn new(path: &Path) -> Self {
        LogTail { path: path.to_owned(), offset: 0, partial: Vec::new() }
    }

    /// Reads what's new; whether a complete line announced boot completion.
    fn saw_boot(&mut self) -> bool {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(&self.path) else { return false };
        let mut new = Vec::new();
        if f.seek(SeekFrom::Start(self.offset)).and_then(|_| f.read_to_end(&mut new)).is_err() || new.is_empty() {
            return false;
        }
        self.offset += new.len() as u64;
        self.partial.extend_from_slice(&new);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else { return false };
        let done: Vec<u8> = self.partial.drain(..=end).collect();
        String::from_utf8_lossy(&done).lines().any(|l| l.starts_with("emu: boot completed"))
    }
}

impl Android {
    /// The SDK and AVD home from the environment, resolved like the SDK tools do.
    pub fn from_env() -> Self {
        Android::new(Env::from_env())
    }

    pub fn new(env: Env) -> Self {
        Android::with_host(env, std::sync::Arc::new(listing::Ps))
    }

    /// [`Android::new`] asking `host` about processes instead of `ps` and `lsof`.
    pub fn with_host(env: Env, host: std::sync::Arc<dyn listing::Host>) -> Self {
        Android { env, running_dir: emu_avd::running_dir(), stock: std::sync::Arc::new(listing::StockCache::new(host)) }
    }

    /// Every running emulator: simee's (their advertisements), then the stock emulator's
    /// ([`Env::stock_instances`]) for AVDs simee doesn't run.
    fn running(&self) -> Vec<Running> {
        let mut all = self.simee_running();
        let stock = self.stock.instances(&self.env, &all);
        all.extend(stock);
        all
    }

    fn simee_running(&self) -> Vec<Running> {
        self.running_dir.as_deref().map(emu_avd::running_instances_in).unwrap_or_default()
    }

    /// The emulator running AVD `name` alone (simee's, else the stock one's): reads that AVD's
    /// lock only, not every AVD's, which is what a call on one device needs.
    fn running_of(&self, name: &str) -> Option<Running> {
        let simee = self.simee_running();
        match simee.iter().find(|r| r.avd_name == name) {
            Some(r) => Some(r.clone()),
            None => self.stock.instance(&self.env, name, &simee),
        }
    }

    /// Every AVD, labelled with its form factor; running ones (simee or stock) are `Booted`.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let running = self.running();
        self.env.list_avds().into_iter().map(|name| self.describe(&name, &running).info).collect()
    }

    /// AVD `name` as a booted device, from its config alone (no emulator lookup: the caller saw
    /// its process); `None` when no AVD has that name.
    pub fn running_info(&self, name: &str) -> Option<DeviceInfo> {
        if !self.env.has_avd(name) {
            return None;
        }
        let mut info = self.describe(name, &[]).info;
        info.state = DeviceState::Booted;
        Some(info)
    }

    /// Whether an AVD is named exactly `name` (its UDID), without loading or listing any: one
    /// stat of its pointer, in exact case ([`emu_avd::Env::has_avd`]).
    pub fn has_avd(&self, name: &str) -> bool {
        self.env.has_avd(name)
    }

    /// Resolves a UDID-or-name query (`booted` is the only running device) to a device. An exact
    /// AVD name loads just that AVD.
    pub fn device(&self, query: &str) -> Result<Device, String> {
        // One directory scan, not one for the exact-name check and another for the list.
        let names = self.env.list_avds();
        // An exact AVD name (what every API call passes) probes that AVD alone.
        if !query.is_empty() && names.iter().any(|n| n == query) {
            return Ok(self.describe(query, &self.running_of(query).into_iter().collect::<Vec<_>>()));
        }
        let running = self.running();
        // `booted` is decided by what is running alone: load only those AVDs, not every config.
        let only_running = query.eq_ignore_ascii_case("booted");
        let devices: Vec<Device> = names
            .into_iter()
            .filter(|name| !only_running || name.eq_ignore_ascii_case(query) || running.iter().any(|r| r.avd_name == *name))
            .map(|name| self.describe(&name, &running))
            .collect();
        let infos: Vec<DeviceInfo> = devices.iter().map(|d| d.info.clone()).collect();
        match select(&infos, query) {
            Selection::One(i) => devices.into_iter().nth(i).ok_or_else(|| format!("no device matches `{query}`")),
            Selection::None => Err(format!("no device matches `{query}`")),
            Selection::Ambiguous => Err(format!("`{query}` matches several devices; use a UDID")),
        }
    }

    fn describe(&self, name: &str, running: &[Running]) -> Device {
        let running = running.iter().find(|r| r.avd_name == name).cloned();
        let state = if running.is_some() { DeviceState::Booted } else { DeviceState::Shutdown };
        let mut info = DeviceInfo {
            udid: name.to_owned(),
            name: name.to_owned(),
            state,
            runtime_id: String::new(),
            device_type_id: "custom".into(),
            available: false,
            platform: Platform::Android,
            form_factor: None,
            sub_platform: None,
        };
        let avd = self.env.load(name).ok();
        if let Some(avd) = &avd {
            let profile = avd.profile();
            info = info.with_kind(Some(profile.kind));
            info.name = avd.get("avd.ini.displayname").unwrap_or(name).to_owned();
            info.runtime_id = runtime_id(&avd.sysdir);
            info.device_type_id = avd.get("hw.device.name").unwrap_or("custom").to_owned();
            info.available = true;
        }
        Device { info, avd, running }
    }

    /// Installed system images, as runtimes (`system-images;android-36;google_apis;arm64-v8a`).
    pub fn runtimes(&self) -> Vec<RuntimeInfo> {
        let Some(root) = &self.env.sdk_root else { return Vec::new() };
        let mut out = Vec::new();
        let dirs =
            |p: &Path| std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>();
        for platform in dirs(&root.join("system-images")) {
            for tag in dirs(&platform) {
                for abi in dirs(&tag) {
                    if !abi.join("system.img").is_file() && !abi.join("kernel-ranchu").is_file() {
                        continue;
                    }
                    let props = std::fs::read_to_string(abi.join("source.properties")).map(|t| emu_avd::parse_ini(&t)).unwrap_or_default();
                    let api = props.get("AndroidVersion.ApiLevel").cloned().unwrap_or_default();
                    let tag_name = tag.file_name().map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
                    out.push(RuntimeInfo {
                        identifier: runtime_id(&abi),
                        name: format!("Android {api} ({tag_name})"),
                        version: api,
                        build: props.get("Pkg.Revision").cloned().unwrap_or_default(),
                        available: true,
                        platform: Platform::Android,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.identifier.cmp(&b.identifier));
        out
    }

    /// Creates an AVD from an installed system image, shaped like `form_factor` (else like the
    /// image's own kind: a phone for mobile images, a watch for Wear OS, ...). Rejects a form
    /// factor the image can't boot, such as a tablet from a Wear OS image.
    pub fn create(&self, name: &str, system_image: &str, form_factor: Option<FormFactor>) -> Result<DeviceInfo, String> {
        self.create_device(name, system_image, form_factor, None)
    }

    /// [`Android::create`] following a hardware definition, as avdmanager's `-d` does: a
    /// foldable's (`pixel_fold`, `surface_duo`; [`sim_core::fold::devices`]) screen and hinge.
    pub fn create_device(
        &self,
        name: &str,
        system_image: &str,
        form_factor: Option<FormFactor>,
        device: Option<&str>,
    ) -> Result<DeviceInfo, String> {
        let hardware = match device {
            None => None,
            Some(d) => Some(sim_core::fold::device(d).ok_or_else(|| {
                let known: Vec<&str> = sim_core::fold::devices().iter().map(|d| d.id).collect();
                format!("unknown device `{d}` (foldables: {})", known.join(", "))
            })?),
        };
        let form_factor = match (form_factor, &hardware) {
            (Some(f), Some(h)) if f != FormFactor::Foldable => return Err(format!("{} is a foldable, not {}", h.name, f.as_str())),
            (None, Some(_)) => Some(FormFactor::Foldable),
            (f, _) => f,
        };
        let sysdir = sysdir_path(system_image)?;
        let tag = sysdir.split('/').nth(2).unwrap_or_default().to_owned();
        let image = android_profile(|k| (k == "tag.id").then(|| tag.clone())).kind.form_factor;
        let wanted = form_factor.unwrap_or(image);
        let kind = Kind::new(Platform::Android, wanted).map_err(|e| e.to_string())?;
        let mobile = |f: FormFactor| matches!(f, FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable);
        if wanted != image && !(mobile(wanted) && mobile(image)) {
            let label = Kind::new(Platform::Android, image).map(|k| k.label()).unwrap_or_default();
            return Err(format!("system image `{system_image}` (tag `{tag}`) boots {label} devices, not {}", wanted.as_str()));
        }
        let mut profile = kind.image_profile(&tag);
        if let Some(h) = &hardware {
            profile = profile.with_screen(h.lcd.0, h.lcd.1, sim_core::form_factor::Density::Dpi(h.lcd.2));
            if let Some(fold) = &mut profile.fold {
                fold.angle = h.hinge.default;
                if let Some(r) = h.hinge.folded {
                    (fold.folded_width, fold.folded_height) = (r.width, r.height);
                }
            }
        }
        self.env.create_avd(&NewAvd::for_profile(name, &sysdir, &profile)).map_err(|e| e.to_string())?;
        Ok(self.describe(name, &[]).info)
    }

    /// Creates AVD `name` from `golden`, a stopped AVD that has booted under simee, with
    /// copy-on-write clones of its settled disks (`emu_avd::Env::clone_avd`): it boots warm,
    /// without first boot, and takes no disk space until it writes.
    pub fn clone_avd(&self, golden: &str, name: &str) -> Result<DeviceInfo, String> {
        self.env.clone_avd(golden, name).map_err(|e| e.to_string())?;
        Ok(self.describe(name, &[]).info)
    }

    /// Exports `device`, which must be shut down, to a bundle at `to` (`emu_avd::Env::export_avd`):
    /// a `.tar` file, else a new directory.
    pub fn export_avd(&self, device: &Device, to: &Path) -> Result<emu_avd::bundle::Exported, String> {
        if device.running.is_some() {
            return Err(format!("{} must be shut down to export it", device.info.name));
        }
        self.env.export_avd(&device.info.udid, to, &emu_avd::bundle::Options::default()).map_err(|e| e.to_string())
    }

    /// Imports the bundle at `from` as a new AVD `name` (`emu_avd::Env::import_avd`; with
    /// `allow_mismatch`, onto another revision of the same system image).
    pub fn import_avd(&self, from: &Path, name: &str, allow_mismatch: bool) -> Result<(DeviceInfo, emu_avd::bundle::Imported), String> {
        let imported = self.env.import_avd(from, name, allow_mismatch).map_err(|e| e.to_string())?;
        Ok((self.describe(name, &[]).info, imported))
    }

    /// Boots every AVD in `avds` (those not running yet), at most `concurrency` at a time, each
    /// on its own console port handed out here from `first_port` up ([`allocate_ports`]), and
    /// waits for each like [`Device::boot`]. One result per AVD, in order: how long it took from
    /// launch to booted, or why it didn't. A device that fails doesn't stop the others; the next
    /// one starts as soon as any finishes.
    pub fn boot_many(&self, avds: &[String], concurrency: usize, timeout: Duration, first_port: u16) -> Result<Vec<BootOutcome>, String> {
        let locks = emu_avd::port_lock_dir();
        let free = |p: u16| port_usable(locks.as_deref(), p, port_free);
        // A farm stays silent: N devices would open N host audio streams.
        self.boot_many_with(avds, concurrency, timeout, first_port, free, |d, port| d.launch_command(Some(port), false))
    }

    fn boot_many_with(
        &self,
        avds: &[String],
        concurrency: usize,
        timeout: Duration,
        first_port: u16,
        free: impl Fn(u16) -> bool + Sync,
        launch: impl Fn(&Device, u16) -> Command + Sync,
    ) -> Result<Vec<BootOutcome>, String> {
        let running = self.running();
        let devices: Vec<Device> = avds.iter().map(|a| self.device(a)).collect::<Result<_, _>>()?;
        let taken: Vec<u16> = running.iter().flat_map(|r| [r.console_port, r.adb_port]).collect();
        let to_start = devices.iter().filter(|d| d.running.is_none()).count();
        let last = *emu_ports().end();
        let handed = allocate_ports(to_start, first_port, last, &taken, &free)?;
        // Where a device whose pair was taken after all (another farm, or a bare emu started by
        // another tool, claimed it between our probe and emu's claim) looks for its next one.
        let spare = std::sync::Mutex::new(handed.iter().max().map_or(first_port, |p| p + 2));
        let retry_port = |taken_now: &[u16]| -> Option<u16> {
            let mut from = spare.lock().unwrap_or_else(|e| e.into_inner());
            let p = allocate_ports(1, *from, last, taken_now, &free).ok()?.first().copied()?;
            *from = p + 2;
            Some(p)
        };
        let mut ports = handed.into_iter();
        let jobs: Vec<(Device, u16)> = devices
            .into_iter()
            .map(|d| {
                let port = match &d.running {
                    Some(r) => r.console_port,
                    None => ports.next().unwrap_or_default(),
                };
                (d, port)
            })
            .collect();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: std::sync::Mutex<Vec<Option<BootOutcome>>> = std::sync::Mutex::new(vec![None; jobs.len()]);
        std::thread::scope(|s| {
            for _ in 0..concurrency.clamp(1, jobs.len().max(1)) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((d, port)) = jobs.get(i) else { break };
                        let (mut port, mut tries) = (*port, 0);
                        let started = Instant::now();
                        let result = loop {
                            let r = d.boot_with(self, timeout, launch(d, port)).map(|()| started.elapsed());
                            match r {
                                Err(e) if e.contains(emu_avd::PORT_TAKEN) && tries < PORT_RETRIES => {
                                    tries += 1;
                                    let taken_now: Vec<u16> = self.running().iter().flat_map(|r| [r.console_port, r.adb_port]).collect();
                                    match retry_port(&taken_now) {
                                        Some(p) => port = p,
                                        None => break Err(e),
                                    }
                                }
                                r => break r,
                            }
                        };
                        let outcome = BootOutcome { avd: d.info.udid.clone(), console_port: port, result };
                        results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(outcome);
                    }
                });
            }
        });
        Ok(results.into_inner().unwrap_or_else(|e| e.into_inner()).into_iter().flatten().collect())
    }

    /// Deletes a stopped AVD (its directory and pointer file).
    pub fn delete(&self, device: &Device) -> Result<(), String> {
        if device.running.is_some() {
            return Err(format!("{} is running; shut it down first", device.info.name));
        }
        // Also refuses one a stock emulator holds that hasn't advertised itself (its lock files).
        self.env.delete_avd_in(&device.info.udid, self.running_dir.as_deref()).map_err(|e| e.to_string())
    }

    /// `adb`, from the SDK's platform-tools or the PATH.
    pub fn adb(&self) -> Option<PathBuf> {
        let sdk = self.env.sdk_root.as_ref().map(|r| r.join("platform-tools/adb")).filter(|p| p.is_file());
        sdk.or_else(|| Command::new("adb").arg("version").output().ok().filter(|o| o.status.success()).map(|_| PathBuf::from("adb")))
    }
}

/// One device's result in [`Android::boot_many`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootOutcome {
    pub avd: String,
    /// Its console port (`emulator-<port>`); adb is the next one.
    pub console_port: u16,
    /// Launch to booted, or why it didn't boot.
    pub result: Result<Duration, String>,
}

/// The console ports emulators use (stock's range: 5554 to 5682, adb on each + 1).
pub fn emu_ports() -> std::ops::RangeInclusive<u16> {
    5554..=5682
}

/// How many other pairs [`Android::boot_many`] tries for a device whose emu reports its pair
/// taken ([`emu_avd::PORT_TAKEN`]).
const PORT_RETRIES: usize = 3;

/// Whether console or adb port `p` is usable: no simee instance holds its claim in `locks`
/// (`<locks>/<p>.lock`, taken by emu before it probes or binds, so one that's starting counts)
/// and `bind` says it's free now.
pub fn port_usable(locks: Option<&Path>, p: u16, bind: impl Fn(u16) -> bool) -> bool {
    !locks.is_some_and(|d| emu_avd::port_claimed(d, p)) && bind(p)
}

/// Console ports for `n` devices launched together: even ports from `first` to `last` whose
/// console and adb ports aren't `taken` (advertised by a running emulator, stock or simee) and
/// are `free` (in [`Android::boot_many`]: not claimed by a starting simee, [`port_usable`],
/// and free to bind now). One allocator hands out every pair, so devices launched at once never
/// probe their way to the same one. Another allocator (a second farm, a bare `emu`) can still
/// claim a pair between this probe and emu's own claim; emu then exits with
/// [`emu_avd::PORT_TAKEN`] and `boot_many` moves that device to another pair.
pub fn allocate_ports(n: usize, first: u16, last: u16, taken: &[u16], free: &dyn Fn(u16) -> bool) -> Result<Vec<u16>, String> {
    let start = first + first % 2;
    let ports: Vec<u16> =
        (start..=last).step_by(2).filter(|&p| !taken.contains(&p) && !taken.contains(&(p + 1)) && free(p) && free(p + 1)).take(n).collect();
    if ports.len() < n {
        return Err(format!("only {} free console/adb port pairs in {first}..{last} for {n} devices", ports.len()));
    }
    Ok(ports)
}

fn port_free(p: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, p)).is_ok()
}

/// `system-images;android-36;google_apis;arm64-v8a` for an image directory.
fn runtime_id(sysdir: &Path) -> String {
    let parts: Vec<String> = sysdir.components().rev().take(4).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    match parts.as_slice() {
        [abi, tag, platform, root] if root == "system-images" => format!("system-images;{platform};{tag};{abi}"),
        _ => sysdir.display().to_string(),
    }
}

/// `system-images/android-36/google_apis/arm64-v8a` from either the SDK package form
/// (`system-images;android-36;google_apis;arm64-v8a`) or the path form.
fn sysdir_path(image: &str) -> Result<String, String> {
    let path = image.replace(';', "/");
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["system-images", platform, tag, abi] => Ok(format!("system-images/{platform}/{tag}/{abi}")),
        [platform, tag, abi] => Ok(format!("system-images/{platform}/{tag}/{abi}")),
        _ => Err(format!("system image `{image}` should look like system-images;android-36;google_apis;arm64-v8a")),
    }
}

/// The `emu` binary: `$SIMEE_EMU` (or `$EMU_BIN`) if set, else the release build: an `emu` beside
/// this executable unless that is a debug build's, else this checkout's `target/release/emu`;
/// a debug `emu` only when there is no release one (a debug guest boots several times slower, so
/// a debug simee or desktop must not pick it up just because it sits beside them), else the PATH.
/// A release `emu` in this checkout older than the sources is used, with a note on stderr that
/// it is stale: simee never builds it behind your back (`cargo build --release -p simee-emu`).
pub fn emu_binary() -> PathBuf {
    let var = |n: &str| std::env::var_os(n).map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let chosen = choose_emu(var("SIMEE_EMU").or_else(|| var("EMU_BIN")), exe.as_deref(), &target, Path::is_file);
    if is_checkout_release(&chosen, &target) {
        static NOTED: std::sync::Once = std::sync::Once::new();
        NOTED.call_once(|| {
            if let Some(note) = stale_note(&chosen, &Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")) {
                eprintln!("{note}");
            }
        });
    }
    chosen
}

/// Whether `emu` is a release binary under `target`, compared canonically: the running
/// executable's path is resolved, `target` (built from `CARGO_MANIFEST_DIR/../../target`) isn't.
fn is_checkout_release(emu: &Path, target: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (emu, target) = (canon(emu), canon(target));
    emu.starts_with(&target) && emu.parent().is_some_and(|d| d.ends_with("release"))
}

/// Which `emu` to start (see [`emu_binary`]): `explicit` wins as given; else, in order, an `emu`
/// beside `exe` that is not a debug build's, `target/release/emu`, an `emu` beside `exe`, then
/// `target/debug/emu`, then `emu` on the PATH. `is_file` says which of those exist.
pub fn choose_emu(explicit: Option<PathBuf>, exe: Option<&Path>, target: &Path, is_file: impl Fn(&Path) -> bool) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    let beside = exe.map(|e| e.with_file_name("emu")).filter(|p| is_file(p));
    let beside_is_debug = beside.as_ref().and_then(|p| p.parent()).is_some_and(|d| d.ends_with("debug") || d.ends_with("deps"));
    let release = target.join("release").join("emu");
    let debug = target.join("debug").join("emu");
    if let Some(b) = beside.clone().filter(|_| !beside_is_debug) {
        return b;
    }
    if is_file(&release) {
        return release;
    }
    beside.or_else(|| Some(debug).filter(|d| is_file(d))).unwrap_or_else(|| PathBuf::from("emu"))
}

/// The note to print when `emu` is older than the newest source in `checkout` that builds it
/// (`apps/emu`, `crates/emu-*`, `crates/sim-core`), `None` when it is current or can't be told.
fn stale_note(emu: &Path, checkout: &Path) -> Option<String> {
    let built = std::fs::metadata(emu).and_then(|m| m.modified()).ok()?;
    let mut newest = None;
    let mut roots = vec![checkout.join("apps/emu/src"), checkout.join("crates/sim-core/src")];
    roots.extend(
        std::fs::read_dir(checkout.join("crates"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("emu-")).then(|| e.path().join("src"))),
    );
    while let Some(dir) = roots.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                roots.push(e.path());
            } else if let Ok(t) = meta.modified() {
                newest = newest.max(Some(t));
            }
        }
    }
    (newest? > built).then(|| {
        format!(
            "simee: {} is older than its sources; rebuild it with `cargo build --release -p simee-emu` (SIMEE_EMU picks another binary)",
            emu.display()
        )
    })
}

/// One AVD, running or not.
#[derive(Clone, Debug)]
pub struct Device {
    pub info: DeviceInfo,
    /// `None` when its config or system image can't be read.
    pub avd: Option<Avd>,
    pub running: Option<Running>,
}

impl Device {
    fn running(&self) -> Result<&Running, String> {
        self.running.as_ref().ok_or_else(|| format!("{} isn't running; boot it first", self.info.name))
    }

    /// The running instance, for what only simee's `emu` offers (`what`: the display socket,
    /// simee's console commands). A device the stock emulator runs gets a clear refusal.
    fn running_simee(&self, what: &str) -> Result<&Running, String> {
        let r = self.running()?;
        if r.simee {
            Ok(r)
        } else {
            Err(format!(
                "{} is running under the stock emulator ({}, PID {}); {what} needs simee's emu. Shut it down and boot it with `simee boot {}`",
                self.info.name,
                r.serial(),
                r.pid,
                self.info.udid
            ))
        }
    }

    fn avd(&self) -> Result<&Avd, String> {
        self.avd.as_ref().ok_or_else(|| format!("{}: its config or system image can't be read", self.info.name))
    }

    /// The form factor profile (screen, shape, fold, inputs).
    pub fn profile(&self) -> Result<Profile, String> {
        Ok(self.avd()?.profile())
    }

    /// Sends console commands in one session; returns the reply lines.
    pub fn console(&self, commands: &[String]) -> Result<Vec<String>, String> {
        console::send(self.running()?.console_port, commands)
    }

    /// Screen size in pixels (the AVD's LCD).
    pub fn screen_size(&self) -> Result<(u32, u32), String> {
        let (w, h, _) = self.avd()?.lcd();
        Ok((w, h))
    }

    /// The size the guest lays out for, which [`play`](Self::play) and [`touch`](Self::touch)
    /// points are in, turned as the guest rotated its display (a phone in landscape: 2400x1080,
    /// as screenshots, `adb screencap` and uiautomator's bounds are then): the LCD, except on a
    /// folded foldable under simee, whose guest lays out for its outer screen (a Pixel Fold's
    /// 1080x2092, as uiautomator's bounds are).
    pub fn layout_size(&self) -> Result<(u32, u32), String> {
        match self.folded_view()? {
            Some((s, turn)) => Ok(turn.turned((s.width, s.height))),
            None => Ok(self.rotation()?.turned(self.screen_size()?)),
        }
    }

    /// How far the running device's guest rotated its display (emu's `rotation`, from what the
    /// guest logs as it rotates, not the accelerometer it may not follow): simee's screenshots,
    /// frames and touches are turned by this much, as `adb screencap` and uiautomator's bounds
    /// are. Upright unless it runs under simee (or under an emu that predates it).
    pub fn rotation(&self) -> Result<Rotation, String> {
        if !self.running.as_ref().is_some_and(|r| r.simee) {
            return Ok(Rotation::Deg0);
        }
        Ok(display_rotation(&self.console(&["rotation".into()])?))
    }

    /// A folded foldable's hinge under simee, whose frames (the display socket, screenshots) show
    /// only the outer screen's region of the scanout (949x1840 of a Pixel Fold's 2208x1840) while
    /// the guest lays out for the outer screen's own size, and how far its display is turned (in
    /// the same console session): None for every other device, and without asking the emulator
    /// unless the device is a foldable.
    fn folded_view(&self) -> Result<Option<(sim_core::fold::FoldState, Rotation)>, String> {
        let foldable = self.info.kind().is_some_and(|k| k.accepts(InputMethod::Hinge));
        if !foldable || !self.running.as_ref().is_some_and(|r| r.simee) {
            return Ok(None);
        }
        let lines = self.console(&["fold status".into(), "rotation".into()])?;
        Ok(sim_core::fold::FoldState::parse(&lines).filter(|s| s.folded).map(|s| (s, display_rotation(&lines))))
    }

    /// `point` in the guest's layout on the display socket's frames (see [`Device::layout_size`]),
    /// which emu turns onto the panel itself.
    fn to_view(view: Option<&(sim_core::fold::FoldState, Rotation)>, x: f64, y: f64) -> (f32, f32) {
        let (x, y) = view.map_or((x, y), |(s, turn)| s.guest_to_turned_view((x, y), *turn));
        (x as f32, y as f32)
    }

    /// Starts `emu` for this AVD (unless it's running) and waits until it has booted: until
    /// `sys.boot_completed` is 1 when adb is available, else until the display has a frame.
    /// The emulator's own output goes to `<avd>/simee/emu.log`. With `audio` the device plays
    /// on the host's default output; without it (what tools and farms want) it is silent
    /// (`-no-audio`) and opens no host audio device.
    pub fn boot(&self, android: &Android, timeout: Duration, audio: bool) -> Result<(), String> {
        self.boot_with(android, timeout, self.launch_command(None, audio))
    }

    /// The command that starts `emu` for this AVD, on console port `port` if given (else emu
    /// picks the first free pair), silent unless `audio`.
    fn launch_command(&self, port: Option<u16>, audio: bool) -> Command {
        let bin = emu_binary();
        let signer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sign-run.sh");
        let mut cmd = if cfg!(target_os = "macos") && signer.is_file() {
            let mut c = Command::new(signer);
            c.arg(&bin);
            c
        } else {
            Command::new(&bin)
        };
        cmd.args(["-avd", &self.info.udid, "-no-boot-anim"]);
        if !audio {
            cmd.arg("-no-audio");
        }
        // The guest's logcat, for the desktop's panel and anyone debugging a boot: the console
        // is quiet now, and logcat over hvc1 is the cheap stream.
        if let Some(avd) = &self.avd {
            cmd.args(["-logcat", LOGCAT_FILTER, "-logcat-output"]).arg(emu_avd::state_dir(&avd.dir).join(LOGCAT_FILE));
        }
        if let Some(port) = port {
            cmd.args(["-port", &port.to_string()]);
        }
        cmd
    }

    /// [`boot`](Self::boot) with the launch command given (its stdout and stderr are replaced by
    /// the log). Boot is detected from emu's own `emu: boot completed in X s` line, tailed in the
    /// log from where this launch began: no process per poll, and the lag is one poll (50 ms).
    /// adb is the fallback for an emulator that never prints it (the stock one, an older emu),
    /// consulted every [`ADB_FALLBACK`] and once more to confirm after the line appears. A child
    /// that exits fails the wait at once instead of at the timeout.
    fn boot_with(&self, android: &Android, timeout: Duration, mut cmd: Command) -> Result<(), String> {
        let avd = self.avd()?;
        let deadline = Instant::now() + timeout;
        let log_dir = emu_avd::state_dir(&avd.dir);
        let log_path = log_dir.join("emu.log");
        let mut child = None;
        let mut tail = None;
        if self.running.is_none() {
            std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
            let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
            // Truncated just now: this launch's output starts at offset 0.
            tail = Some(LogTail::new(&log_path));
            let err = log.try_clone().map_err(|e| e.to_string())?;
            let spawned = cmd.stdin(Stdio::null()).stdout(log).stderr(err).spawn();
            child = Some(spawned.map_err(|e| format!("starting {}: {e}", cmd.get_program().to_string_lossy()))?);
        }
        let adb = android.adb();
        let find = || android.running().into_iter().find(|r| r.avd_name == self.info.udid);
        let mut printed = false;
        // Our own launch is watched through its log: adb is only the slow fallback. An emulator
        // already running has no log to read from its start, so adb answers at once.
        let slow = tail.is_some();
        let mut next_check = if slow { Instant::now() + ADB_FALLBACK } else { Instant::now() };
        loop {
            if let Some(c) = child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                let text = std::fs::read_to_string(&log_path).unwrap_or_default();
                let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").to_owned();
                return Err(format!("emu exited ({status}) before booting: {last} (log: {})", log_path.display()));
            }
            if !printed && tail.as_mut().is_some_and(LogTail::saw_boot) {
                printed = true;
                next_check = Instant::now(); // confirm now
            }
            if Instant::now() >= next_check {
                next_check = Instant::now() + if printed || !slow { ADB_RETRY } else { ADB_FALLBACK };
                if let Some(r) = find()
                    && booted(&r, adb.as_deref())
                {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                // A device that failed leaves nothing behind: the emulator this call started is
                // stopped and reaped (no zombie in a long-lived caller), its ports released.
                let stopped = child.as_mut().map(|c| stop_child(c, STOP_GRACE));
                let note = match stopped {
                    Some(Ok(())) => "; stopped it",
                    Some(Err(_)) => "; it could not be stopped",
                    None => "",
                };
                return Err(format!("{} did not finish booting within {}s{note}", self.info.name, timeout.as_secs()));
            }
            sleep(BOOT_POLL);
        }
    }

    /// Asks the emulator to quit (console `kill`) and waits for it to go.
    pub fn shutdown(&self) -> Result<(), String> {
        let r = self.running()?;
        if r.simee {
            // `kill` closes the session after its reply, so the reply may be cut short.
            let _ = console::send(r.console_port, &["kill".into()]);
        } else {
            // `kill -INT` makes the stock emulator exit cleanly without a console session
            // (which would need its auth token first). The PID came from a lock file, so first
            // confirm it is still a qemu running this AVD: never signal a reused PID.
            if !emu_avd::process_runs_avd(r.pid, &r.avd_name) {
                return Err(format!(
                    "{}: PID {} no longer looks like the stock emulator running it (its command line changed); not signalling it",
                    self.info.name, r.pid
                ));
            }
            Command::new("kill").args(["-INT", &r.pid.to_string()]).status().map_err(|e| format!("kill: {e}"))?;
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while r.alive() && Instant::now() < deadline {
            sleep(Duration::from_millis(200));
        }
        if r.alive() { Err(format!("{} is still running after `kill`", self.info.name)) } else { Ok(()) }
    }

    /// The current screen at full resolution, from the emulator's renderer, decoded.
    pub fn screenshot(&self) -> Result<Frame, String> {
        let (_, _, png) = self.screenshot_png()?;
        decode_png(&png)
    }

    /// The current screen as the emulator encodes it: width, height and PNG bytes, with no
    /// decode and re-encode on the way.
    pub fn screenshot_png(&self) -> Result<(u32, u32, Vec<u8>), String> {
        // Removes the file on every path out, a panic included.
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        // Unique per call: calls can run at once on one device.
        static SHOTS: AtomicU64 = AtomicU64::new(0);
        let n = SHOTS.fetch_add(1, Ordering::Relaxed);
        let scratch = Scratch(std::env::temp_dir().join(format!("sim-android-{}-{}-{n}.png", std::process::id(), self.info.udid)));
        let size = self.screenshot_png_to(&scratch.0);
        let png = std::fs::read(&scratch.0).map_err(|e| format!("{}: {e}", scratch.0.display()));
        let (w, h) = size?;
        Ok((w, h, png?))
    }

    /// Has the emulator write the current screen to `path` as PNG (it runs on this host);
    /// returns the width and height.
    pub fn screenshot_png_to(&self, path: &Path) -> Result<(u32, u32), String> {
        let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.console(&[format!("screenshot {}", path.display())])?;
        let mut header = [0u8; 24];
        std::fs::File::open(&path)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut header))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        sim_core::frame::png_size(&header).ok_or_else(|| format!("{} isn't a PNG", path.display()))
    }

    /// Plays touch samples (pixels of the guest's layout, [`Device::layout_size`]) over one
    /// display connection.
    pub fn play(&self, samples: &[Sample]) -> Result<(), String> {
        let avd = self.touch_screen()?;
        let view = self.folded_view()?;
        self.with_display(avd, |display| {
            // Against deadlines from the first sample, so send costs and oversleeps don't stretch it.
            let start = Instant::now();
            pace_samples(
                samples,
                || start.elapsed(),
                sleep,
                |s| {
                    let (x, y) = Self::to_view(view.as_ref(), s.point.x, s.point.y);
                    display.send(&Input::Touch { x, y, phase: s.phase })
                },
            )
        })
    }

    /// One touch sample at pixels of the guest's layout ([`Device::layout_size`]).
    pub fn touch(&self, x: f64, y: f64, phase: Phase) -> Result<(), String> {
        let avd = self.touch_screen()?;
        let (x, y) = Self::to_view(self.folded_view()?.as_ref(), x, y);
        self.with_display(avd, |d| d.send(&Input::Touch { x, y, phase }))
    }

    /// A touch on the touchpad beside the screen (`hw.touchpad0`: Android XR display glasses), in
    /// touchpad units: `phase` is down, move, up or tap. Through simee's console `touchpad`.
    pub fn touchpad(&self, x: u32, y: u32, phase: &str) -> Result<(), String> {
        self.touchpad_size()?;
        self.running_simee("the touchpad console command")?;
        self.console(&[format!("touchpad {x} {y} {phase}")]).map(drop)
    }

    /// A swipe across the touchpad from `from` to `to` (touchpad units) over about `duration`.
    pub fn touchpad_swipe(&self, from: (u32, u32), to: (u32, u32), duration: Duration) -> Result<(), String> {
        let (w, h) = self.touchpad_size()?;
        if from.0.max(to.0) >= w || from.1.max(to.1) >= h {
            return Err(format!("the touchpad is {w}x{h}"));
        }
        const STEPS: u32 = 10;
        let lerp = |a: u32, b: u32, i: u32| (f64::from(a) + (f64::from(b) - f64::from(a)) * f64::from(i) / f64::from(STEPS)).round() as u32;
        let mut lines = vec![format!("touchpad {} {} down", from.0, from.1)];
        lines.extend((1..=STEPS).map(|i| format!("touchpad {} {} move", lerp(from.0, to.0, i), lerp(from.1, to.1, i))));
        lines.push(format!("touchpad {} {} up", to.0, to.1));
        // One console session for the whole swipe, its steps on deadlines.
        let mut session = console::Session::open(self.running_simee("the touchpad console command")?.console_port)?;
        let mut delays = vec![duration / STEPS; lines.len()];
        delays[0] = Duration::ZERO;
        let start = Instant::now();
        pace(&delays, || start.elapsed(), sleep, |i| session.command(&lines[i]).map(drop))
    }

    /// Stylus samples on the touch screen at pixels of the screen as screenshots show it (turned
    /// with the device), with the codes stock's gRPC `PenEvent` sends, spread over `duration` in
    /// one console session (simee's `pen` command).
    pub fn stylus(&self, samples: &[sim_core::stylus::PenSample], duration: Duration) -> Result<(), String> {
        let (w, h) = self.rotation()?.turned(self.screen_size()?);
        if let Some(s) = samples.iter().find(|s| s.x as u32 >= w || s.y as u32 >= h) {
            return Err(format!("({}, {}) is off the {w}x{h} screen", s.x, s.y));
        }
        if samples.is_empty() {
            return Ok(());
        }
        let lines: Vec<String> = samples.iter().map(sim_core::stylus::PenSample::console).collect();
        let mut session = self.pen_session()?;
        let steps = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        let mut delays = vec![duration / steps; lines.len()];
        delays[0] = Duration::ZERO;
        let start = Instant::now();
        pace(&delays, || start.elapsed(), sleep, |i| session.command(&lines[i]).map(drop))
    }

    /// A console session for `pen` lines (`PenSample::console`), for a caller that sends samples
    /// as they come (the desktop's stylus) and keeps the session open across them.
    pub fn pen_session(&self) -> Result<console::Session, String> {
        console::Session::open(self.running_simee("the pen console command")?.console_port)
    }

    /// The touchpad's size, if the AVD has one.
    fn touchpad_size(&self) -> Result<(u32, u32), String> {
        self.avd()?
            .touchpad()
            .ok_or_else(|| format!("{} has no touchpad (hw.touchpad0; Android XR display glasses have one)", self.info.name))
    }

    /// Runs `f` on this process's connection to the running emulator's display socket, made on
    /// first use and kept: the server reads each connection on its own thread, so input sent
    /// over separate connections (a tap, then the next touch) can reach the guest out of order.
    /// One connection per device keeps it in order, and a touch doesn't pay a connect. A
    /// connection that fails (the emulator restarted) is replaced once.
    fn with_display<T>(&self, avd: &Avd, mut f: impl FnMut(&mut display::Display) -> Result<T, String>) -> Result<T, String> {
        type Slot = std::sync::Arc<std::sync::Mutex<Option<display::Display>>>;
        static DISPLAYS: std::sync::Mutex<Vec<(PathBuf, Slot)>> = std::sync::Mutex::new(Vec::new());
        self.running_simee("input and frames over the display socket")?;
        let path = display::socket(&avd.dir);
        let slot = {
            let mut all = DISPLAYS.lock().unwrap_or_else(|e| e.into_inner());
            match all.iter().find(|(p, _)| *p == path) {
                Some((_, slot)) => slot.clone(),
                None => {
                    let slot = Slot::default();
                    all.push((path, slot.clone()));
                    slot
                }
            }
        };
        let mut conn = slot.lock().unwrap_or_else(|e| e.into_inner());
        let mut retried = false;
        loop {
            let d = match conn.as_mut() {
                Some(d) => d,
                None => conn.insert(display::Display::connect(&avd.dir)?),
            };
            match f(d) {
                Err(_) if !retried => {
                    *conn = None;
                    retried = true;
                }
                result => return result,
            }
        }
    }

    /// The AVD, if its screen takes touch (an SDK TV's says `hw.screen=no-touch`).
    fn touch_screen(&self) -> Result<&Avd, String> {
        let avd = self.avd()?;
        if avd.touch_screen() {
            Ok(avd)
        } else {
            Err(format!("{} has no touch screen (hw.screen=no-touch); use button up|down|left|right|select", self.info.name))
        }
    }

    /// Presses or releases a hardware button.
    pub fn button(&self, button: Button, direction: Direction) -> Result<(), String> {
        let key = button.android_key().ok_or_else(|| format!("the {} button doesn't exist on Android", button.as_str()))?;
        self.console(&[format!("event send EV_KEY:{key}:{}", u8::from(direction == Direction::Down))]).map(drop)
    }

    /// Presses a hardware button, holds it for `hold`, and releases it, in one console session
    /// (down and up used to be two sessions).
    pub fn press_button(&self, button: Button, hold: Duration) -> Result<(), String> {
        let key = button.android_key().ok_or_else(|| format!("the {} button doesn't exist on Android", button.as_str()))?;
        self.press_key_event(key, hold)
    }

    /// Presses a key by HID usage, holds it for `hold`, and releases it, in one console session.
    pub fn press_key(&self, usage: u32, hold: Duration) -> Result<(), String> {
        let code = linux_keycode(usage).ok_or_else(|| format!("HID usage {usage:#x} has no Android key"))?;
        self.press_key_event(&code.to_string(), hold)
    }

    fn press_key_event(&self, key: &str, hold: Duration) -> Result<(), String> {
        let (down, up) = (format!("event send EV_KEY:{key}:1"), format!("event send EV_KEY:{key}:0"));
        if hold.is_zero() {
            return self.console(&[down, up]).map(drop);
        }
        let mut session = console::Session::open(self.running()?.console_port)?;
        session.command(&down)?;
        sleep(hold);
        session.command(&up).map(drop)
    }

    /// Presses or releases a key by HID usage.
    pub fn key(&self, usage: u32, direction: Direction) -> Result<(), String> {
        let code = linux_keycode(usage).ok_or_else(|| format!("HID usage {usage:#x} has no Android key"))?;
        self.console(&[format!("event send EV_KEY:{code}:{}", u8::from(direction == Direction::Down))]).map(drop)
    }

    /// Types text on the hardware keyboard (US layout), in one console session.
    pub fn type_text(&self, text: &str) -> Result<(), String> {
        let strokes = text
            .chars()
            .map(|c| keystroke(c).ok_or_else(|| format!("no key for {c:?} on the US layout")))
            .collect::<Result<Vec<_>, _>>()?;
        self.strokes(&strokes)
    }

    /// Presses each key stroke (HID usage, with shift if set), in one console session.
    pub fn strokes(&self, strokes: &[KeyStroke]) -> Result<(), String> {
        let shift = linux_keycode(KEY_LEFT_SHIFT).unwrap_or(42);
        let mut commands = Vec::with_capacity(strokes.len());
        for stroke in strokes {
            let code = linux_keycode(stroke.usage).ok_or_else(|| format!("HID usage {:#x} has no Android key", stroke.usage))?;
            commands.push(if stroke.shift {
                format!("event send EV_KEY:{shift}:1 EV_KEY:{code}:1 EV_KEY:{code}:0 EV_KEY:{shift}:0")
            } else {
                format!("event send EV_KEY:{code}:1 EV_KEY:{code}:0")
            });
        }
        self.console(&commands).map(drop)
    }

    /// Simulated location.
    pub fn set_location(&self, latitude: f64, longitude: f64) -> Result<(), String> {
        // The console takes longitude first.
        self.console(&[format!("geo fix {longitude} {latitude}")]).map(drop)
    }

    /// Rotates the device 90° clockwise.
    pub fn rotate(&self) -> Result<(), String> {
        self.console(&["rotate".into()]).map(drop)
    }

    fn require(&self, input: InputMethod, what: &str) -> Result<(), String> {
        match self.info.kind() {
            Some(k) if k.accepts(input) => Ok(()),
            Some(k) => Err(format!("{what} needs a device with {}; {} is {}", input.label(Platform::Android), self.info.name, k.label())),
            None => Err(format!("{what}: {}'s form factor is unknown", self.info.name)),
        }
    }

    /// Sets a foldable's hinge angle (0 closed, 180 flat; a dual-screen turns on to 360, folded
    /// back). The emulator checks it against the AVD's hinge range. Foldables only.
    pub fn fold(&self, angle: f32) -> Result<(), String> {
        self.require(InputMethod::Hinge, "folding")?;
        if !(0.0..=360.0).contains(&angle) {
            return Err(format!("hinge angle {angle} is outside 0-360"));
        }
        self.console(&[format!("sensor set hinge-angle0 {angle}")]).map(drop)
    }

    /// Moves a foldable's hinge to `posture` (the console's `posture <id>`, as stock's). Foldables
    /// only; the emulator refuses a posture the AVD's hinge doesn't have.
    pub fn posture(&self, posture: sim_core::fold::StockPosture) -> Result<(), String> {
        self.require(InputMethod::Hinge, "folding")?;
        self.console(&[format!("posture {}", posture.id())]).map(drop)
    }

    /// Folds a foldable to its folded screen (the console's `fold`: a fold-in shut, a dual-screen
    /// folded back), or unfolds it flat (`unfold`). Foldables only.
    pub fn fold_display(&self, folded: bool) -> Result<(), String> {
        self.require(InputMethod::Hinge, "folding")?;
        self.console(&[if folded { "fold" } else { "unfold" }.to_owned()]).map(drop)
    }

    /// A foldable's hinge as the emulator has it (simee's `fold status`); None from an emulator
    /// that doesn't say, the stock one among them (it isn't asked: it refuses the command).
    pub fn fold_state(&self) -> Result<Option<sim_core::fold::FoldState>, String> {
        self.require(InputMethod::Hinge, "folding")?;
        if !self.running()?.simee {
            return Ok(None);
        }
        Ok(sim_core::fold::FoldState::parse(&self.console(&["fold status".into()])?))
    }

    /// Turns the rotary input (Wear OS crown or bezel) by `ticks` clicks, which the emulator
    /// scales to stock's 15 units per click (`sim_core::stream::wheel_units`). Watches only.
    pub fn rotary(&self, ticks: i32) -> Result<(), String> {
        self.require(InputMethod::Rotary, "rotary input")?;
        self.with_display(self.avd()?, |d| d.send(&Input::Wheel(ticks)))
    }

    /// An XR control (head pose, environment, input mode, pointer) through simee's `xr` console
    /// command; returns its reply lines. Android XR devices only.
    pub fn xr(&self, command: &XrCommand) -> Result<Vec<String>, String> {
        match self.info.kind() {
            Some(k) if k.sub_platform == SubPlatform::Android(AndroidOs::Xr) => self.console(&[command.console()]),
            Some(k) => Err(format!("XR controls need an Android XR device; {} is {}", self.info.name, k.label())),
            None => Err(format!("XR controls: {}'s form factor is unknown", self.info.name)),
        }
    }

    /// Sets a vehicle HAL property through the emulator's `car` console command (an simee
    /// extension). `property` is a VHAL name (`GEAR_SELECTION`, `gear`) or id; `value` is typed
    /// by the property (`drive`, `12.5`, `true`). Automotive devices only.
    pub fn vehicle_set(&self, property: &str, area_id: i32, value: &str) -> Result<(), String> {
        self.require(InputMethod::HeadUnit, "the vehicle HAL")?;
        // Checked here so a bad name or value fails before any emulator is involved.
        let prop = vhal::property(property)?;
        vhal::parse_value(prop, area_id, value)?;
        self.console(&[format!("car set {prop:#x} {area_id} {value}")]).map(drop)
    }

    /// A vehicle HAL property's current value, read from the guest. Automotive devices only.
    pub fn vehicle_get(&self, property: &str, area_id: i32) -> Result<vhal::Reading, String> {
        self.require(InputMethod::HeadUnit, "the vehicle HAL")?;
        let prop = vhal::property(property)?;
        let lines = self.console(&[format!("car get {prop:#x} {area_id}")])?;
        lines.iter().find_map(|l| vhal::Reading::parse(l)).ok_or_else(|| format!("unexpected `car get` reply: {lines:?}"))
    }

    /// Every vehicle HAL property's current value. Automotive devices only.
    pub fn vehicle_list(&self) -> Result<Vec<vhal::Reading>, String> {
        self.require(InputMethod::HeadUnit, "the vehicle HAL")?;
        Ok(self.console(&["car list".into()])?.iter().filter_map(|l| vhal::Reading::parse(l)).collect())
    }
}

/// Whether a running instance has finished booting.
fn booted(r: &Running, adb: Option<&Path>) -> bool {
    let started = Instant::now();
    // Only simee's console is pinged here; for the stock emulator (whose console also wants its
    // auth token first) adb alone says whether it has booted.
    let console_up = !r.simee
        || loop {
            if console::send(r.console_port, &["ping".into()]).is_ok() {
                break true;
            }
            if started.elapsed() > CONSOLE_GRACE {
                break false;
            }
            sleep(Duration::from_millis(200));
        };
    if !console_up {
        return false;
    }
    match adb {
        Some(adb) => Command::new(adb)
            .args(["-s", &r.serial(), "shell", "getprop", "sys.boot_completed"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "1"),
        None => console::send(r.console_port, &["frames".into()])
            .ok()
            .and_then(|l| l.first().and_then(|n| n.trim().parse::<u64>().ok()))
            .is_some_and(|n| n > 0),
    }
}

/// The last `n` lines of a device's guest console (`<avd>/simee/console.log`). Without
/// `-show-kernel` (or `EMU_CONSOLE=full`) emu keeps the console quiet: only critical kernel
/// messages and panics, so for a normal boot it's empty.
pub fn console_log_tail(avd_dir: &Path, n: usize) -> Vec<String> {
    file_tail(&emu_avd::state_dir(avd_dir).join("console.log"), n)
}

/// Where [`Device::boot`] has emu write the guest's logcat, in the AVD's state directory
/// ([`emu_avd::state_dir`]; `-logcat-output`; it rolls over to `logcat.txt.1` at 4 MiB). Filter
/// [`LOGCAT_FILTER`].
pub const LOGCAT_FILE: &str = "logcat.txt";

/// The logcat filter devices booted here stream to [`LOGCAT_FILE`]: info and up. hvc1 takes
/// whole buffers per kick, so it costs the guest logcat's formatting, not vCPU exits per byte.
pub const LOGCAT_FILTER: &str = "*:I";

/// The last `n` lines of a device's logcat stream ([`LOGCAT_FILE`], the rolled-over file
/// before it when the current one is short).
pub fn logcat_tail(avd_dir: &Path, n: usize) -> Vec<String> {
    let dir = emu_avd::state_dir(avd_dir);
    let current = file_tail(&dir.join(LOGCAT_FILE), n);
    if current.len() >= n {
        return current;
    }
    let mut older = file_tail(&dir.join(format!("{LOGCAT_FILE}.1")), n - current.len());
    older.extend(current);
    older
}

/// What a device panel shows, newest last, at most `n` lines: the console's critical lines (a
/// panic, init's FATAL; at most a quarter of `n`) then logcat's.
pub fn device_log_tail(avd_dir: &Path, n: usize) -> Vec<String> {
    let mut lines = console_log_tail(avd_dir, n / 4);
    let rest = n - lines.len();
    lines.extend(logcat_tail(avd_dir, rest));
    lines
}

/// The last `n` lines of `path`. Reads from the end, 64 KiB at a time, until it has them: logs
/// grow to megabytes, and the desktop asks every two seconds. Bytes that aren't UTF-8 show as
/// U+FFFD.
fn file_tail(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const STEP: u64 = 64 * 1024;
    if n == 0 {
        return Vec::new();
    }
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return Vec::new() };
    let mut window = STEP.min(len);
    let tail = loop {
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(len - window)).and_then(|_| f.by_ref().take(window).read_to_end(&mut buf)).is_err() {
            return Vec::new();
        }
        // More newlines than lines wanted: even after dropping the partial first line, enough.
        if window == len || buf.iter().filter(|&&b| b == b'\n').count() > n {
            if window < len {
                let first = buf.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
                buf.drain(..first);
            }
            break buf;
        }
        window = (window * 2).min(len);
    };
    let text = String::from_utf8_lossy(&tail);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| (*l).to_owned()).collect()
}

/// How far a display is turned whose emulator's console answered `rotation` with `lines`
/// (`rotation: 90`); upright when the reply has no such line (an emu that predates it) or says
/// `rotation: unknown` (an image whose rotation emu doesn't read, API 29 and 30: its frames and
/// touches stay in panel space, as they are upright).
fn display_rotation(lines: &[String]) -> Rotation {
    lines.iter().find_map(|l| Rotation::from_degrees(l.strip_prefix("rotation: ")?.trim().parse().ok()?)).unwrap_or_default()
}

/// Decodes an 8-bit RGBA or RGB PNG (the emulator's `screenshot` writes RGBA; `encode_png` writes
/// RGB for an opaque frame).
fn decode_png(bytes: &[u8]) -> Result<Frame, String> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buf,
        (png::ColorType::Rgb, png::BitDepth::Eight) => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 0xff]).collect(),
        (color, depth) => return Err(format!("unexpected screenshot format {color:?} {depth:?}")),
    };
    Ok(Frame { width: info.width, height: info.height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fake_emulator, fake_stock_emulator, wait_for};

    struct Fixture {
        root: PathBuf,
        android: Android,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// An SDK with a mobile and a Wear OS image, and an empty AVD home.
    fn fixture(tag: &str) -> Fixture {
        // Under /tmp, not $TMPDIR: the fake display socket's path must fit a unix socket address
        // (104 bytes on macOS), and macOS's $TMPDIR alone takes about 50.
        let root = PathBuf::from("/tmp").join(format!("sa-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        for (dir, api, image_tag) in
            [("android-36/google_apis/arm64-v8a", 36, "google_apis"), ("android-34/android-wear/arm64-v8a", 34, "android-wear")]
        {
            let d = sdk.join("system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(
                d.join("source.properties"),
                format!("AndroidVersion.ApiLevel={api}\nPkg.Revision=7\nSystemImage.TagId={image_tag}\n"),
            )
            .expect("props");
        }
        let mut android = Android::new(Env { sdk_root: Some(sdk), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        Fixture { root, android }
    }

    #[test]
    fn emu_choice_prefers_release_and_an_explicit_override() {
        let target = Path::new("/repo/target");
        let exists = |files: &'static [&'static str]| move |p: &Path| files.iter().any(|f| Path::new(f) == p);
        let choose = |explicit: Option<&str>, exe: &str, files: &'static [&'static str]| {
            choose_emu(explicit.map(PathBuf::from), Some(Path::new(exe)), target, exists(files))
        };
        // An explicit path wins, even when it doesn't exist.
        assert_eq!(choose(Some("/x/emu"), "/repo/target/debug/simee", &["/repo/target/release/emu"]), PathBuf::from("/x/emu"));
        // A debug simee beside a debug emu starts the release one.
        let both = &["/repo/target/debug/emu", "/repo/target/release/emu"];
        assert_eq!(choose(None, "/repo/target/debug/simee", both), PathBuf::from("/repo/target/release/emu"));
        assert_eq!(choose(None, "/repo/target/debug/deps/e2e-1a2b", both), PathBuf::from("/repo/target/release/emu"));
        // Only a debug one exists: it's used rather than nothing.
        assert_eq!(choose(None, "/repo/target/debug/simee", &["/repo/target/debug/emu"]), PathBuf::from("/repo/target/debug/emu"));
        // A release simee, or an installed one, uses the emu beside it.
        assert_eq!(choose(None, "/repo/target/release/simee", both), PathBuf::from("/repo/target/release/emu"));
        assert_eq!(
            choose(None, "/opt/simee/bin/simee", &["/opt/simee/bin/emu", "/repo/target/release/emu"]),
            PathBuf::from("/opt/simee/bin/emu")
        );
        // Nothing found: the PATH.
        assert_eq!(choose(None, "/opt/simee/bin/simee", &[]), PathBuf::from("emu"));
    }

    #[test]
    fn a_release_emu_older_than_its_sources_is_called_out() {
        let root = PathBuf::from("/tmp").join(format!("sa-stale-{}", std::process::id()));
        let src = root.join("apps/emu/src");
        std::fs::create_dir_all(&src).expect("src");
        let emu = root.join("emu");
        std::fs::write(&emu, b"bin").expect("emu");
        std::fs::File::options()
            .write(true)
            .open(&emu)
            .and_then(|f| f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)))
            .expect("age it");
        assert_eq!(stale_note(&emu, &root), None, "no sources newer");
        std::fs::write(src.join("main.rs"), b"fn main() {}").expect("source");
        let note = stale_note(&emu, &root).expect("stale");
        assert!(note.contains("cargo build --release -p simee-emu") && note.contains("SIMEE_EMU"), "{note}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stock_emulator_devices_refuse_display_only_operations_clearly() {
        let f = fixture("stockrun");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let mut d = f.android.device("phone").expect("phone");
        d.running = Some(Running { pid: 4242, avd_name: "phone".into(), console_port: 5750, adb_port: 5751, avd_dir: None, simee: false });
        let err = d.touch(1.0, 1.0, Phase::Down).expect_err("stock has no display socket");
        assert!(err.contains("stock emulator") && err.contains("emulator-5750") && err.contains("needs simee's emu"), "{err}");
        assert!(err.contains("simee boot phone"), "{err}");
        assert_eq!(d.running.as_ref().map(Running::emulator), Some("stock"));
    }

    #[test]
    fn stock_shutdown_refuses_a_pid_that_is_no_longer_its_qemu() {
        let f = fixture("stockkill");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let mut d = f.android.device("phone").expect("phone");
        // A PID that is some other program now (as after a reuse): a `sleep` this test started.
        let mut other = Command::new("sleep").arg("30").spawn().expect("sleep");
        d.running = Some(Running {
            pid: other.id() as i32,
            avd_name: "phone".into(),
            console_port: 5750,
            adb_port: 5751,
            avd_dir: None,
            simee: false,
        });
        let err = d.shutdown().expect_err("not its qemu");
        let alive = other.try_wait().expect("wait").is_none();
        let _ = other.kill();
        let _ = other.wait();
        assert!(alive, "the unrelated process was not signalled");
        assert!(err.contains("no longer looks like the stock emulator") && err.contains("not signalling"), "{err}");
    }

    #[test]
    fn the_stale_check_sees_a_release_emu_through_a_dotdot_target() {
        let root = PathBuf::from("/tmp").join(format!("sa-canon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("target/release")).expect("dirs");
        std::fs::create_dir_all(root.join("crates/x")).expect("dirs");
        std::fs::write(root.join("target/release/emu"), b"").expect("emu");
        let canonical = std::fs::canonicalize(root.join("target/release/emu")).expect("canonical");
        // `CARGO_MANIFEST_DIR/../../target` is not normalized; the executable's path is.
        let target = root.join("crates/x/../../target");
        assert!(is_checkout_release(&canonical, &target));
        assert!(!is_checkout_release(&canonical.with_file_name("elsewhere").join("emu"), &target));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runtimes_are_installed_system_images() {
        let f = fixture("runtimes");
        let rts = f.android.runtimes();
        let ids: Vec<&str> = rts.iter().map(|r| r.identifier.as_str()).collect();
        assert_eq!(ids, ["system-images;android-34;android-wear;arm64-v8a", "system-images;android-36;google_apis;arm64-v8a"]);
        assert_eq!((rts[1].name.as_str(), rts[1].version.as_str(), rts[1].platform), ("Android 36 (google_apis)", "36", Platform::Android));
    }

    #[test]
    fn foldables_follow_their_hardware_definition() {
        let f = fixture("foldables");
        let image = "system-images;android-36;google_apis;arm64-v8a";
        let duo = f.android.create_device("duo", image, None, Some("duo")).expect("duo");
        assert_eq!(duo.form_factor, Some(FormFactor::Foldable), "a foldable without being told");
        let avd = f.android.env.load("duo").expect("load");
        assert_eq!((avd.lcd(), avd.get("hw.device.name")), ((2784, 1800, 401), Some("surface_duo")));
        assert_eq!(avd.profile().fold.map(|f| (f.folded_width, f.folded_height)), Some((1350, 1800)));
        f.android.create_device("pf", image, Some(FormFactor::Foldable), Some("pixel_fold")).expect("pixel fold");
        assert_eq!(f.android.env.load("pf").expect("load").get("hw.displayRegion.0.1.height"), Some("2092"));
        let err = f.android.create_device("x", image, Some(FormFactor::Tablet), Some("duo")).expect_err("not a tablet");
        assert_eq!(err, "Dual-screen (Surface Duo-style) is a foldable, not tablet");
        let err = f.android.create_device("x", image, None, Some("pixel_9")).expect_err("unknown");
        assert_eq!(err, "unknown device `pixel_9` (foldables: pixel_fold, surface_duo)");
    }

    #[test]
    fn created_avds_list_as_labelled_android_devices() {
        let f = fixture("create");
        let phone = f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        assert_eq!(
            (phone.platform, phone.form_factor, phone.label()),
            (Platform::Android, Some(FormFactor::Phone), "Android · Phone".to_owned())
        );
        assert_eq!(phone.runtime_id, "system-images;android-36;google_apis;arm64-v8a");
        let fold = f.android.create("fold", "system-images/android-36/google_apis/arm64-v8a", Some(FormFactor::Foldable)).expect("fold");
        assert_eq!(fold.form_factor, Some(FormFactor::Foldable));
        let watch = f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        assert_eq!(watch.label(), "Wear OS · Watch");
        let devices = f.android.devices();
        assert_eq!(devices.iter().map(|d| d.udid.as_str()).collect::<Vec<_>>(), ["fold", "phone", "watch"]);
        assert!(devices.iter().all(|d| d.state == DeviceState::Shutdown && d.available));
        let d = f.android.device("watch").expect("watch");
        assert_eq!(d.profile().expect("profile").shape, sim_core::form_factor::Shape::Round);
        assert_eq!(d.screen_size(), Ok((454, 454)));
        assert!(f.android.device("nope").expect_err("missing").contains("no device matches `nope`"));
        assert!(f.android.device("booted").is_err(), "nothing is running");
    }

    #[test]
    fn create_rejects_form_factors_the_image_cant_boot() {
        let f = fixture("reject");
        let wear = "system-images;android-34;android-wear;arm64-v8a";
        let err = f.android.create("t", wear, Some(FormFactor::Tablet)).expect_err("tablet from a Wear image");
        assert_eq!(err, format!("system image `{wear}` (tag `android-wear`) boots Wear OS · Watch devices, not tablet"));
        let err = f.android.create("v", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Vision)).expect_err("vision");
        assert!(err.ends_with("(tag `google_apis`) boots Android · Phone devices, not vision"), "{err}");
        assert!(f.android.create("x", "android-36", None).expect_err("bad image").contains("should look like"));
        assert!(f.android.devices().is_empty(), "nothing was written");
    }

    #[test]
    fn stopped_devices_refuse_running_only_operations() {
        let f = fixture("stopped");
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let d = f.android.device("watch").expect("watch");
        assert_eq!(d.rotate(), Err("watch isn't running; boot it first".into()));
        assert_eq!(d.touch(1.0, 1.0, Phase::Down), Err("watch isn't running; boot it first".into()), "not a socket error");
        assert_eq!(d.rotary(1), Err("watch isn't running; boot it first".into()));
        assert!(d.fold(90.0).expect_err("not a foldable").starts_with("folding needs a device with Hinge; watch is Wear OS · Watch"));
        assert!(d.button(Button::Siri, Direction::Down).expect_err("no Siri").contains("doesn't exist on Android"));
        f.android.delete(&d).expect("delete");
        assert!(f.android.devices().is_empty());
    }

    #[test]
    fn running_devices_are_driven_over_console_and_display() {
        let f = fixture("running");
        f.android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("fold");
        let fake = fake_emulator(&f.android, "fold");
        let d = f.android.device("booted").expect("the running device is `booted`");
        assert_eq!((d.info.udid.as_str(), d.info.state), ("fold", DeviceState::Booted));
        let r = d.running.clone().expect("running");
        assert!(booted(&r, None), "console answers and the display has frames");

        d.type_text("Hi").expect("type");
        d.button(Button::Home, Direction::Down).expect("home down");
        d.button(Button::Back, Direction::Up).expect("back up");
        d.key(0x28, Direction::Down).expect("return");
        d.set_location(37.5, -122.25).expect("geo");
        d.rotate().expect("rotate");
        d.fold(90.0).expect("fold");
        assert_eq!(
            wait_for(&fake.console, 10)[2..],
            [
                "event send EV_KEY:42:1 EV_KEY:35:1 EV_KEY:35:0 EV_KEY:42:0",
                "event send EV_KEY:23:1 EV_KEY:23:0",
                "event send EV_KEY:KEY_HOME:1",
                "event send EV_KEY:KEY_BACK:0",
                "event send EV_KEY:28:1",
                "geo fix -122.25 37.5",
                "rotate",
                "sensor set hinge-angle0 90",
            ]
        );
        let shot = d.screenshot().expect("screenshot");
        assert_eq!((shot.width, shot.height, shot.rgba[0]), (2, 1, 9));
        let (w, h, png) = d.screenshot_png().expect("png");
        assert_eq!((w, h, sim_core::frame::png_size(&png)), (2, 1, Some((2, 1))));
        let to = f.root.join("shot.png");
        assert_eq!(d.screenshot_png_to(&to), Ok((2, 1)));
        assert!(std::fs::read(&to).expect("written").starts_with(b"\x89PNG"), "the emulator wrote it there");

        d.play(&sim_core::gesture::tap(sim_core::geometry::Point::new(100.0, 200.0), Duration::ZERO)).expect("tap");
        assert_eq!(wait_for(&fake.display, 2), ["touch 100 200 down", "touch 100 200 up"]);
        assert!(d.rotary(1).expect_err("not a watch").starts_with("rotary input needs a device with Rotary input"));
        assert!(f.android.delete(&d).expect_err("running").contains("is running"));
    }

    #[test]
    fn concurrent_screenshots_of_one_device_each_get_their_own_file() {
        let f = fixture("shots");
        f.android.create("shots", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let _fake = fake_emulator(&f.android, "shots");
        let d = f.android.device("booted").expect("running");
        let results: Vec<_> = std::thread::scope(|s| {
            let jobs: Vec<_> = (0..2).map(|_| s.spawn(|| d.screenshot_png())).collect();
            jobs.into_iter().map(|j| j.join().expect("thread")).collect()
        });
        for r in results {
            let (w, h, png) = r.expect("each screenshot succeeds");
            assert_eq!((w, h, sim_core::frame::png_size(&png)), (2, 1, Some((2, 1))));
        }
    }

    /// Adds an installed system image under the fixture's SDK.
    fn add_image(f: &Fixture, dir: &str, image_tag: &str) {
        let d = f.root.join("sdk/system-images").join(dir);
        std::fs::create_dir_all(&d).expect("image dir");
        std::fs::write(d.join("system.img"), b"s").expect("system.img");
        std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=34\nSystemImage.TagId={image_tag}\n")).expect("props");
    }

    #[test]
    fn only_devices_with_a_touchpad_take_touchpad_input() {
        let f = fixture("pad");
        add_image(&f, "android-36/ai-glasses/arm64-v8a", "ai-glasses");
        f.android.create("glasses", "system-images;android-36;ai-glasses;arm64-v8a", None).expect("glasses");
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let watch = f.android.device("watch").expect("watch");
        assert!(watch.touchpad(1, 1, "tap").expect_err("no touchpad").starts_with("watch has no touchpad"));
        let glasses = f.android.device("glasses").expect("glasses");
        assert_eq!(glasses.touchpad_swipe((0, 0), (1542, 10), Duration::ZERO), Err("the touchpad is 1542x297".into()));
        assert_eq!(glasses.touchpad(1, 1, "tap"), Err("glasses isn't running; boot it first".into()));
    }

    /// A fake `adb` that counts its runs in `<root>/adb-runs` and answers `1`.
    fn fake_adb(f: &Fixture) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = f.root.join("sdk/platform-tools");
        std::fs::create_dir_all(&dir).expect("platform-tools");
        let adb = dir.join("adb");
        std::fs::write(&adb, format!("#!/bin/sh\necho run >> {}/adb-runs\necho 1\n", f.root.display())).expect("adb");
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        f.root.join("adb-runs")
    }

    fn adb_runs(counter: &Path) -> usize {
        std::fs::read_to_string(counter).map(|t| t.lines().count()).unwrap_or(0)
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn boot_is_seen_in_the_log_with_one_adb_confirmation() {
        let f = fixture("bootlog");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let runs = fake_adb(&f);
        let d = f.android.device("phone").expect("stopped");
        let _fake = fake_emulator(&f.android, "phone"); // the advertisement and console of the "launched" emu
        let t = Instant::now();
        d.boot_with(&f.android, Duration::from_secs(30), sh("sleep 0.5; echo 'emu: boot completed in 0.5s'; sleep 10")).expect("boots");
        let took = t.elapsed();
        println!("detected {:?} after the line", took.saturating_sub(Duration::from_millis(500)));
        assert!(took >= Duration::from_millis(500), "not before the line: {took:?}");
        // Seen from the log, well before the "emulator" exits 10 s after the line; the bound leaves
        // room for a loaded host, where the fake adb's spawn alone can take a second.
        assert!(took < Duration::from_secs(5), "from the log, not the process exit: {took:?}");
        assert_eq!(adb_runs(&runs), 1, "no adb until the line, then one confirmation");
    }

    #[test]
    fn ports_are_handed_out_once_around_running_emulators() {
        assert_eq!(allocate_ports(3, 5600, 5682, &[5602, 5603], &|_| true), Ok(vec![5600, 5604, 5606]));
        assert_eq!(allocate_ports(2, 5601, 5682, &[5605], &|p| p != 5603), Ok(vec![5606, 5608]), "odd start rounds up; adb ports count");
        assert_eq!(allocate_ports(2, 5678, 5682, &[], &|_| true), Ok(vec![5678, 5680]));
        assert!(allocate_ports(4, 5678, 5682, &[], &|_| true).expect_err("too few").starts_with("only 3 free"), "5682 is the last");
        assert_eq!(allocate_ports(0, 5600, 5682, &[], &|_| false), Ok(vec![]));
    }

    #[test]
    fn ports_claimed_by_a_starting_emu_are_skipped() {
        // Another simee (a second farm, a bare `emu` from another tool) holds 5600's claim but
        // hasn't bound or advertised yet: only the lock file shows it.
        let locks = f_dir("port-locks");
        std::fs::create_dir_all(&locks).expect("mkdir");
        let held = std::fs::File::create(locks.join("5600.lock")).expect("lock file");
        held.lock().expect("lock");
        std::fs::File::create(locks.join("5602.lock")).expect("an unheld leftover");
        let free = |p: u16| port_usable(Some(&locks), p, |_| true);
        assert!(!free(5600) && free(5602) && free(5601));
        assert_eq!(allocate_ports(2, 5600, 5682, &[], &free), Ok(vec![5602, 5604]));
        drop(held);
        // Released (a child another test forks at this moment holds a copy of the descriptor
        // until it execs, so give it a moment).
        let deadline = Instant::now() + Duration::from_secs(2);
        while emu_avd::port_claimed(&locks, 5600) && Instant::now() < deadline {
            sleep(Duration::from_millis(10));
        }
        assert_eq!(allocate_ports(2, 5600, 5682, &[], &free), Ok(vec![5600, 5602]), "released");
        assert!(!port_usable(None, 5600, |p| p != 5600), "the bind probe still counts");
        let _ = std::fs::remove_dir_all(locks);
    }

    #[test]
    fn a_device_whose_pair_was_taken_moves_to_the_next() {
        let f = fixture("taken");
        let names: Vec<String> = ["t1", "t2"].iter().map(|s| s.to_string()).collect();
        for n in &names {
            f.android.create(n, "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        }
        fake_adb(&f);
        let _console = fake_emulator(&f.android, "console-only");
        let me = f.android.running_dir.clone().expect("running").join(format!("pid_{}.ini", std::process::id()));
        let console = Running::parse(&format!("pid_{}.ini", std::process::id()), &std::fs::read_to_string(&me).expect("ad"))
            .expect("parse")
            .console_port;
        let run = f.android.running_dir.clone().expect("running");
        let launches = std::sync::Mutex::new(Vec::new());
        let out = f
            .android
            .boot_many_with(
                &names,
                2,
                Duration::from_secs(20),
                5600,
                |_| true,
                |d, port| {
                    launches.lock().expect("launches").push((d.info.udid.clone(), port));
                    // t1's first pair was claimed by someone else between allocation and launch.
                    if port == 5600 {
                        return sh(&format!("echo 'emu: error: console port {port} {}' >&2; exit 1", emu_avd::PORT_TAKEN));
                    }
                    sh(&format!(
                        "printf 'port.serial={console}\\navd.name={}\\nemulator=simee\\n' > {}/pid_$$.ini; \
                     echo 'emu: boot completed in 0.1s'; sleep 2",
                        d.info.udid,
                        run.display()
                    ))
                },
            )
            .expect("boot_many");
        assert_eq!(out.iter().map(|o| (o.avd.as_str(), o.console_port)).collect::<Vec<_>>(), [("t1", 5604), ("t2", 5602)]);
        assert!(out.iter().all(|o| o.result.is_ok()), "{out:?}");
        let tries: Vec<u16> = launches.into_inner().expect("launches").into_iter().filter(|(a, _)| a == "t1").map(|(_, p)| p).collect();
        assert_eq!(tries, [5600, 5604]);
    }

    #[test]
    fn a_boot_that_times_out_stops_and_reaps_its_emulator() {
        let f = fixture("timeout");
        f.android.create("slow", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let d = f.android.device("slow").expect("stopped");
        let pid_file = f.root.join("slow.pid");
        // An emu that never boots; it quits on SIGINT as emu does.
        let t = Instant::now();
        let err = d
            .boot_with(&f.android, Duration::from_millis(500), sh(&format!("echo $$ > {}; exec sleep 60", pid_file.display())))
            .expect_err("never boots");
        assert!(err.contains("did not finish booting") && err.contains("stopped it"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let pid: i32 = std::fs::read_to_string(&pid_file).expect("pid").trim().parse().expect("pid");
        // Gone and reaped: a zombie would still answer signal 0.
        let alive = Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().expect("kill -0");
        assert!(!alive.success(), "pid {pid} is still there");
    }

    #[test]
    fn an_emulator_that_ignores_sigint_is_killed() {
        let mut child = sh("trap '' INT; while :; do sleep 0.05; done").spawn().expect("spawn");
        sleep(Duration::from_millis(200)); // the trap is set
        let t = Instant::now();
        stop_child(&mut child, Duration::from_millis(300)).expect("stopped");
        assert!(t.elapsed() >= Duration::from_millis(300), "waited the grace first");
        assert!(child.try_wait().expect("wait").is_some(), "reaped");
    }

    #[test]
    fn launches_stream_logcat_to_the_avd() {
        let f = fixture("logcat-arg");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let d = f.android.device("phone").expect("device");
        let args: Vec<String> = d.launch_command(Some(5600), false).get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let at = args.iter().position(|a| a == "-logcat-output").expect("-logcat-output");
        assert_eq!(Path::new(&args[at + 1]), emu_avd::state_dir(&d.avd.as_ref().expect("avd").dir).join(LOGCAT_FILE));
        let at = args.iter().position(|a| a == "-logcat").expect("-logcat");
        assert_eq!(args[at + 1], LOGCAT_FILTER);
    }

    #[test]
    fn launches_are_silent_unless_audio_is_asked_for() {
        let f = fixture("audio-arg");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let d = f.android.device("phone").expect("device");
        let silent = |audio| d.launch_command(None, audio).get_args().any(|a| a == "-no-audio");
        assert!(silent(false), "tools and farms: no host audio stream per device");
        assert!(!silent(true), "a person watching hears it");
    }

    #[test]
    fn the_device_log_is_the_consoles_critical_lines_then_logcat() {
        let dir = f_dir("devlog");
        std::fs::create_dir_all(emu_avd::state_dir(&dir)).expect("mkdir");
        assert!(device_log_tail(&dir, 200).is_empty());
        std::fs::write(emu_avd::state_dir(&dir).join("console.log"), "").expect("quiet console");
        std::fs::write(emu_avd::state_dir(&dir).join("logcat.txt.1"), "o1\no2\no3\n").expect("rolled over");
        std::fs::write(emu_avd::state_dir(&dir).join("logcat.txt"), "l1\nl2\n").expect("logcat");
        assert_eq!(device_log_tail(&dir, 4), ["o2", "o3", "l1", "l2"], "the rolled-over file fills a short current one");
        assert_eq!(logcat_tail(&dir, 2), ["l1", "l2"]);
        std::fs::write(emu_avd::state_dir(&dir).join("console.log"), "Kernel panic - not syncing\n").expect("panic");
        assert_eq!(device_log_tail(&dir, 4), ["Kernel panic - not syncing", "o3", "l1", "l2"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn boot_many_runs_k_at_a_time_on_their_own_ports() {
        let f = fixture("many");
        let names: Vec<String> = ["d1", "d2", "d3", "bad"].iter().map(|s| s.to_string()).collect();
        for n in &names {
            f.android.create(n, "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        }
        fake_adb(&f);
        // One fake console answers for every device; each launched "emu" advertises itself
        // (its shell's PID) with it, then prints the boot line.
        let _console = fake_emulator(&f.android, "console-only");
        let me = f.android.running_dir.clone().expect("running").join(format!("pid_{}.ini", std::process::id()));
        let ad = std::fs::read_to_string(&me).expect("fake ad");
        let console = Running::parse(&format!("pid_{}.ini", std::process::id()), &ad).expect("parse").console_port;
        let run = f.android.running_dir.clone().expect("running");
        let launches = std::sync::Mutex::new(Vec::new());
        let t = Instant::now();
        let out = f
            .android
            .boot_many_with(
                &names,
                2,
                Duration::from_secs(20),
                5600,
                |_| true,
                |d, port| {
                    launches.lock().expect("launches").push((d.info.udid.clone(), port));
                    if d.info.udid == "bad" {
                        return sh("echo 'emu: error: no hypervisor' >&2; exit 1");
                    }
                    sh(&format!(
                        "printf 'port.serial={console}\\navd.name={}\\nemulator=simee\\n' > {}/pid_$$.ini; sleep 0.4; \
                     echo 'emu: boot completed in 0.4s'; sleep 3",
                        d.info.udid,
                        run.display()
                    ))
                },
            )
            .expect("boot_many");
        let took = t.elapsed();
        assert_eq!(
            out.iter().map(|o| (o.avd.as_str(), o.console_port)).collect::<Vec<_>>(),
            [("d1", 5600), ("d2", 5602), ("d3", 5604), ("bad", 5606)]
        );
        assert!(out[..3].iter().all(|o| o.result.as_ref().is_ok_and(|d| *d >= Duration::from_millis(400))), "{out:?}");
        assert!(out[3].result.as_ref().expect_err("bad fails").contains("no hypervisor"), "{out:?}");
        assert!(took >= Duration::from_millis(800), "three 0.4 s boots two at a time take two rounds: {took:?}");
        let mut ports: Vec<u16> = launches.into_inner().expect("launches").into_iter().map(|(_, p)| p).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 4, "one port each");
        assert!(f.android.boot_many(&["nope".to_string()], 1, Duration::from_secs(1), 5600).is_err(), "unknown AVDs fail up front");
    }

    #[test]
    fn an_emu_that_dies_fails_the_boot_at_once() {
        let f = fixture("bootdies");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let runs = fake_adb(&f);
        let d = f.android.device("phone").expect("stopped");
        let t = Instant::now();
        let err = d.boot_with(&f.android, Duration::from_secs(60), sh("echo 'no hypervisor' >&2; exit 3")).expect_err("dies");
        assert!(err.starts_with("emu exited") && err.contains("no hypervisor"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(5), "not the 60 s timeout");
        assert_eq!(adb_runs(&runs), 0);
    }

    #[test]
    fn an_emu_that_never_prints_boot_is_found_by_adb() {
        let f = fixture("bootadb");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let runs = fake_adb(&f);
        let d = f.android.device("phone").expect("stopped");
        let _fake = fake_emulator(&f.android, "phone");
        d.boot_with(&f.android, Duration::from_secs(30), sh("echo started; sleep 3")).expect("boots by adb");
        assert!(adb_runs(&runs) >= 1);
    }

    #[test]
    fn log_tail_reads_appended_lines_once_complete() {
        use std::io::Write;
        let dir = f_dir("tail-line");
        let path = dir.join("emu.log");
        std::fs::write(&path, "starting\n").expect("write");
        let mut tail = LogTail::new(&path);
        assert!(!tail.saw_boot());
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open");
        file.write_all(b"emu: boot comp").expect("write");
        assert!(!tail.saw_boot(), "a partial line isn't the line");
        file.write_all(b"leted in 9.1s\nmore\n").expect("write");
        assert!(tail.saw_boot());
        assert!(!tail.saw_boot(), "seen lines aren't read again");
        std::fs::remove_dir_all(dir).ok();
    }

    fn f_dir(tag: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp").join(format!("sa-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn booted_resolves_by_what_is_running() {
        let f = fixture("booted");
        for n in ["a", "b", "c"] {
            f.android.create(n, "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        }
        assert!(f.android.device("booted").is_err(), "nothing is running");
        let _fake = fake_emulator(&f.android, "b");
        let d = f.android.device("booted").expect("b is the booted one");
        assert_eq!((d.info.udid.as_str(), d.info.state, d.avd.is_some()), ("b", DeviceState::Booted, true));
        assert_eq!(f.android.device("c").map(|d| d.info.state), Ok(DeviceState::Shutdown));
        assert!(f.android.device("nope").is_err());
    }

    #[test]
    fn only_foldables_ask_for_their_hinge_before_a_touch() {
        let f = fixture("phonepts");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let fake = fake_emulator(&f.android, "phone");
        // Even an emulator that would say "folded" isn't asked about a phone.
        *fake.fold_status.lock().expect("status") =
            Some("angle: 0\r\nposture: closed (1)\r\nfolded: true\r\nvisible: 949x1840+629+0\r\nsize: 1080x2092\r\npostures: 1,2,3".into());
        let d = f.android.device("phone").expect("running");
        assert_eq!(d.layout_size(), d.screen_size());
        assert_eq!(std::mem::take(&mut *fake.console.lock().expect("console")), ["rotation"], "how it's turned");
        d.touch(1000.0, 2000.0, Phase::Down).expect("touch");
        d.play(&sim_core::gesture::tap(sim_core::geometry::Point::new(7.0, 9.0), Duration::ZERO)).expect("tap");
        assert_eq!(wait_for(&fake.display, 3), ["touch 1000 2000 down", "touch 7 9 down", "touch 7 9 up"]);
        assert!(fake.console.lock().expect("console").is_empty(), "no console round trip for a phone's touch");
    }

    /// The layout and touches follow the guest's display rotation as emu reports it (`rotation`),
    /// not the accelerometer: a turned phone whose guest stayed upright (auto-rotate off, a
    /// portrait-locked app) keeps its portrait layout.
    #[test]
    fn a_turned_devices_layout_and_folded_touches_are_turned() {
        let f = fixture("turned");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let fake = fake_emulator(&f.android, "phone");
        let d = f.android.device("phone").expect("running");
        let panel = d.screen_size().expect("lcd");
        assert_eq!(d.layout_size(), Ok(panel), "the guest hasn't rotated");
        for (degrees, turn) in [(90, Rotation::Deg90), (180, Rotation::Deg180), (270, Rotation::Deg270), (0, Rotation::Deg0)] {
            *fake.rotation.lock().expect("rotation") = degrees;
            assert_eq!(d.rotation(), Ok(turn));
            assert_eq!(d.layout_size(), Ok(turn.turned(panel)), "{turn:?}");
        }
        // A phone's touches go as they are: emu turns them onto the panel.
        *fake.rotation.lock().expect("rotation") = 90;
        d.touch(2000.0, 100.0, Phase::Down).expect("touch");
        assert_eq!(wait_for(&fake.display, 1), ["touch 2000 100 down"]);
        // A folded Pixel Fold whose guest turned a quarter: its layout is 2092x1080, its frames
        // 1840x949.
        let f = fixture("turnedfold");
        f.android.create("fold", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Foldable)).expect("fold");
        let fake = fake_emulator(&f.android, "fold");
        *fake.fold_status.lock().expect("status") =
            Some("angle: 0\r\nposture: closed (1)\r\nfolded: true\r\nvisible: 949x1840+629+0\r\nsize: 1080x2092\r\npostures: 1,2,3".into());
        *fake.rotation.lock().expect("rotation") = 90;
        let d = f.android.device("fold").expect("running");
        assert_eq!(d.layout_size(), Ok((2092, 1080)));
        d.touch(2092.0, 1080.0, Phase::Down).expect("touch");
        assert_eq!(wait_for(&fake.display, 1), ["touch 1840 949 down"], "the layout's corner on the frames' corner");
        // An emu that predates `rotation`, or a reply without it: upright.
        assert_eq!(display_rotation(&["KO: unknown command".into()]), Rotation::Deg0);
        assert_eq!(display_rotation(&["rotation: 45".into()]), Rotation::Deg0);
        assert_eq!(display_rotation(&["rotation: unknown".into(), "OK".into()]), Rotation::Deg0, "panel space");
        assert_eq!(display_rotation(&["rotation: 270".into()]), Rotation::Deg270);
        // The stock emulator: never asked.
        let f = fixture("turnedstock");
        f.android.create("stock", "system-images;android-36;google_apis;arm64-v8a", None).expect("stock");
        let fake = fake_stock_emulator(&f.android, "stock");
        let d = f.android.device("stock").expect("running");
        assert_eq!(d.rotation(), Ok(Rotation::Deg0));
        assert!(fake.console.lock().expect("console").is_empty());
    }

    #[test]
    fn held_buttons_and_keys_go_down_then_up() {
        let f = fixture("press");
        f.android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("phone");
        let fake = fake_emulator(&f.android, "phone");
        let d = f.android.device("phone").expect("running");
        d.press_button(Button::Home, Duration::from_millis(40)).expect("home");
        d.press_key(0x28, Duration::ZERO).expect("return");
        d.press_key(0x28, Duration::from_millis(15)).expect("return held");
        assert_eq!(
            wait_for(&fake.console, 6),
            [
                "event send EV_KEY:KEY_HOME:1",
                "event send EV_KEY:KEY_HOME:0",
                "event send EV_KEY:28:1",
                "event send EV_KEY:28:0",
                "event send EV_KEY:28:1",
                "event send EV_KEY:28:0",
            ]
        );
        assert!(d.press_button(Button::Siri, Duration::ZERO).expect_err("no Siri").contains("doesn't exist on Android"));
    }

    #[test]
    fn touchpad_swipes_run_in_order_over_one_session() {
        let f = fixture("swipe");
        add_image(&f, "android-36/ai-glasses/arm64-v8a", "ai-glasses");
        f.android.create("glasses", "system-images;android-36;ai-glasses;arm64-v8a", None).expect("glasses");
        let fake = fake_emulator(&f.android, "glasses");
        let d = f.android.device("glasses").expect("running");
        let t = Instant::now();
        d.touchpad_swipe((0, 0), (100, 10), Duration::from_millis(100)).expect("swipe");
        assert!(t.elapsed() >= Duration::from_millis(100), "the swipe takes its duration");
        let got = wait_for(&fake.console, 12);
        assert_eq!(got.len(), 12);
        assert_eq!((got[0].as_str(), got[1].as_str(), got[11].as_str()), ("touchpad 0 0 down", "touchpad 10 1 move", "touchpad 100 10 up"));
    }

    #[test]
    fn console_log_tail_reads_only_the_end() {
        let dir = std::env::temp_dir().join(format!("sa-tail-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let log = dir.join("simee/console.log");
        let naive = |n: usize| -> Vec<String> {
            let text = String::from_utf8_lossy(&std::fs::read(&log).expect("read")).into_owned();
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(n)..].iter().map(|l| (*l).to_owned()).collect()
        };
        assert!(console_log_tail(&dir, 5).is_empty(), "no log yet");
        // Long, with lines of every length, CRLFs and a non-UTF-8 byte.
        let mut text = Vec::new();
        for i in 0..200_000u32 {
            text.extend(format!("[{i:>8}] {}{}", "x".repeat((i % 97) as usize), if i % 7 == 0 { "\r\n" } else { "\n" }).bytes());
        }
        text.extend(b"bad \xff byte\nno newline at the end");
        std::fs::write(&log, &text).expect("write");
        for n in [0, 1, 2, 200, 5000] {
            assert_eq!(console_log_tail(&dir, n), naive(n), "last {n}");
        }
        assert_eq!(console_log_tail(&dir, 1), ["no newline at the end"]);
        assert_eq!(console_log_tail(&dir, 2)[0], "bad \u{fffd} byte", "kept, not an empty panel");
        std::fs::write(&log, "one\ntwo\n").expect("short");
        assert_eq!(console_log_tail(&dir, 200), ["one", "two"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exact_avd_names_resolve_without_a_scan() {
        let f = fixture("exact");
        f.android.create("pixel", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        assert!(f.android.has_avd("pixel"));
        for not in ["", "pix", "../avd/pixel", "pixel/"] {
            assert!(!f.android.has_avd(not), "{not:?}");
        }
        assert_eq!(f.android.device("pixel").expect("exact").info.udid, "pixel");
        assert!(!f.android.has_avd("Pixel"), "exact case, whatever the filesystem");
        assert_eq!(f.android.device("Pixel").map(|d| d.info.udid), Ok("pixel".into()), "the scan still matches names loosely");
    }

    #[test]
    fn a_running_avds_info_comes_from_its_config_alone() {
        let f = fixture("runinfo");
        f.android.create("pixel", "system-images;android-36;google_apis;arm64-v8a", Some(FormFactor::Phone)).expect("create");
        let info = f.android.running_info("pixel").expect("info");
        assert_eq!((info.state, info.platform, info.form_factor), (DeviceState::Booted, Platform::Android, Some(FormFactor::Phone)));
        assert_eq!(f.android.running_info("no-such-avd"), None);
    }

    #[test]
    fn tvs_have_no_touch_screen() {
        let f = fixture("tv");
        add_image(&f, "android-34/android-tv/arm64-v8a", "android-tv");
        f.android.create("tv", "system-images;android-34;android-tv;arm64-v8a", None).expect("tv");
        let tv = f.android.device("tv").expect("tv");
        assert_eq!(tv.avd.as_ref().and_then(|a| a.get("hw.screen")), Some("no-touch"));
        let err = tv.touch(10.0, 10.0, Phase::Down).expect_err("no touch screen");
        assert!(err.starts_with("tv has no touch screen (hw.screen=no-touch)"), "{err}");
        assert!(tv.play(&[]).expect_err("no swipes either").contains("no touch screen"));
    }

    #[test]
    fn android_xr_images_make_headsets_and_glasses() {
        let f = fixture("xr");
        add_image(&f, "android-34/google-xr/arm64-v8a", "android-xr");
        add_image(&f, "android-36/ai-glasses/arm64-v8a", "ai-glasses");
        let headset = f.android.create("headset", "system-images;android-34;google-xr;arm64-v8a", None).expect("headset");
        assert_eq!((headset.form_factor, headset.label()), (Some(FormFactor::Vision), "Android XR · Vision".to_owned()));
        let d = f.android.device("headset").expect("headset");
        assert_eq!(d.screen_size(), Ok((2560, 2558)));
        assert_eq!(d.avd.as_ref().and_then(|a| a.get("hw.initialOrientation")), Some("landscape"));
        let glasses =
            f.android.create("glasses", "system-images;android-36;ai-glasses;arm64-v8a", Some(FormFactor::Vision)).expect("glasses");
        assert_eq!(glasses.sub_platform.map(|s| s.as_str()), Some("androidxr"));
        assert_eq!(f.android.device("glasses").expect("glasses").screen_size(), Ok((450, 450)));
        let err = f.android.create("p", "system-images;android-34;google-xr;arm64-v8a", Some(FormFactor::Phone)).expect_err("phone");
        assert!(err.ends_with("boots Android XR · Vision devices, not phone"), "{err}");

        // XR controls are for Android XR devices, and go to a running one's console.
        let rotate = XrCommand::Rotate { x: 0.0, y: 0.5, z: 0.0 };
        assert_eq!(d.xr(&rotate), Err("headset isn't running; boot it first".into()));
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let watch = f.android.device("watch").expect("watch");
        assert_eq!(watch.xr(&rotate), Err("XR controls need an Android XR device; watch is Wear OS · Watch".into()));
        let fake = fake_emulator(&f.android, "headset");
        let d = f.android.device("headset").expect("running headset");
        d.xr(&rotate).expect("rotate");
        d.xr(&XrCommand::Recenter).expect("recenter");
        assert_eq!(wait_for(&fake.console, 2), ["xr rotate 0 0.5 0", "xr recenter"]);
    }

    #[test]
    fn automotive_devices_drive_the_vehicle_hal() {
        let f = fixture("car");
        let image = f.android.env.sdk_root.clone().expect("sdk").join("system-images/android-34/android-automotive/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=34\nSystemImage.TagId=android-automotive\n")
            .expect("props");
        let car = f.android.create("car", "system-images;android-34;android-automotive;arm64-v8a", None).expect("car");
        assert_eq!(car.form_factor, Some(FormFactor::Auto));
        f.android.create("watch", "system-images;android-34;android-wear;arm64-v8a", None).expect("watch");
        let watch = f.android.device("watch").expect("watch");
        assert!(
            watch
                .vehicle_set("gear", 0, "drive")
                .expect_err("not a car")
                .starts_with("the vehicle HAL needs a device with Automotive head unit; watch is Wear OS")
        );

        let d = f.android.device("car").expect("car");
        assert!(d.vehicle_set("warp_drive", 0, "1").expect_err("unknown").contains("unknown vehicle property"));
        assert!(d.vehicle_set("gear", 0, "fast").expect_err("bad value").contains("isn't a Int32 value"));
        assert_eq!(d.vehicle_set("gear", 0, "drive"), Err("car isn't running; boot it first".into()));

        let fake = fake_emulator(&f.android, "car");
        let d = f.android.device("car").expect("car");
        d.vehicle_set("gear", 0, "drive").expect("set");
        d.vehicle_set("HVAC_TEMPERATURE_SET", 49, "21.5").expect("set seat");
        let gear = d.vehicle_get("GEAR_SELECTION", 0).expect("get");
        assert_eq!((gear.property.as_str(), gear.id, gear.value.as_str()), ("GEAR_SELECTION", 0x1140_0400, "8"));
        let all = d.vehicle_list().expect("list");
        assert_eq!(all.iter().map(|r| r.value.as_str()).collect::<Vec<_>>(), ["8", "Toy Car"]);
        assert_eq!(
            wait_for(&fake.console, 4),
            ["car set 0x11400400 0 drive", "car set 0x15600503 49 21.5", "car get 0x11400400 0", "car list"]
        );
    }

    #[test]
    fn decodes_screenshots() {
        let frame = Frame { width: 1, height: 2, rgba: vec![1, 2, 3, 255, 4, 5, 6, 255] };
        assert_eq!(decode_png(&sim_core::frame::encode_png(&frame).expect("png")), Ok(frame));
        assert!(decode_png(b"nope").is_err());
    }
}
