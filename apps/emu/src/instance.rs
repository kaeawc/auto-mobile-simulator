//! Per-AVD runtime state: disk images under `<avd>/simee/`, the instance lock, boot config.
//!
//! The stock emulator's own files in the AVD directory (qcow2 overlays, snapshots) are never
//! touched, so the same AVD keeps working with both. What a boot involves is decided first, with
//! no side effects, as a [`BootPlan`] (`emu -avd NAME -dry-run` prints it on any host); the
//! [`Instance`] that creates the disks and runs the plan exists only on Apple silicon.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::fs::File;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use anyhow::{Context, Result, bail};
use emu_avd::{Avd, BootConfig};
use emu_vmm::layout::slots;
use emu_vmm::{DiskMode, DiskSpec};
use sim_core::platform::{AndroidOs, SubPlatform};

use crate::args::Opts;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const CACHE_BYTES: u64 = 66 << 20;

/// Kernel arguments the stock board passes for ranchu images.
const BASE_CMDLINE: &str = "8250.nr_uarts=1 no_timer_check console=ttyAMA0,38400 keep_bootcon earlyprintk=ttyAMA0 \
                            ndns=2 loop.max_part=7 printk.devkmsg=on bootconfig mac80211_hwsim.radios=0";

/// Without `-show-kernel`, only messages below KERN_ERR reach the console (emergencies, alerts,
/// criticals, so init's FATAL lines). The PL011 traps twice per byte, and booting printed 230 KB
/// on it, 88% of all vCPU exits (~0.6 vCPU-s per boot, ~2.5 µs per byte in the writer, often
/// init). Stock (`SetupParameters.cpp`) passes `console=0` then and prints nothing; with
/// `-show-kernel` it passes `console=ttyAMA0,38400`, as simee always does. This keeps critical
/// lines in `console.log`, panics included (`panic()` raises the console level). One sysrq-c test
/// found console.log empty: each reset used to recreate the file, wiping the panic; it's opened
/// once per process now. Use `-show-kernel` or `EMU_CONSOLE=full` to debug boots; dmesg still has
/// everything.
const QUIET_CONSOLE: &str = "loglevel=3";

/// Every message on the console: `-show-kernel`, or `EMU_CONSOLE=full` (console.log only).
fn full_console(opts: &Opts) -> bool {
    opts.show_kernel || std::env::var("EMU_CONSOLE").is_ok_and(|v| v == "full")
}

/// The guest's logcat filter for `goldfish-logcat` (`logcat -f /dev/hvc1 ${ro.boot.logcat}`):
/// `-logcat`'s, everything with `-logcat-output` alone, else silent (`*:S`): the stream is
/// discarded then (stock's null chardev), so formatting and writing it is wasted guest work.
fn logcat_filter(opts: &Opts) -> &str {
    match (&opts.logcat, &opts.logcat_output) {
        (Some(tags), _) => tags,
        (None, Some(_)) => "*:V",
        (None, None) => "*:S",
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub struct Instance {
    avd: Avd,
    /// Console port (the `emulator-<port>` serial); adb listens on port + 1.
    pub console_port: u16,
    /// Latest guest frame, when host rendering is on.
    pub display: Option<std::sync::Arc<emu_vmm::display::FrameStore>>,
    gpu_lib64: Option<PathBuf>,
    /// Input handles of the current run, for the console and desktop.
    pub inputs: std::sync::Arc<std::sync::Mutex<Option<emu_vmm::Inputs>>>,
    /// Sensor values, shared by the guest's sensors HAL and the console.
    pub sensors: std::sync::Arc<emu_vmm::pipe::sensors::Sensors>,
    /// Guest clipboard, shared with the console.
    pub clipboard: std::sync::Arc<emu_vmm::pipe::clipboard::Clipboard>,
    pub fingerprint: std::sync::Arc<emu_vmm::pipe::fingerprint::Fingerprint>,
    pub gps: std::sync::Arc<emu_vmm::pipe::gps::Gps>,
    pub battery: std::sync::Arc<emu_vmm::devices::goldfish_battery::Battery>,
    pub telephony: std::sync::Arc<emu_vmm::telephony::Telephony>,
    /// Android XR images: the guest's XR service link.
    pub xr: Option<std::sync::Arc<emu_vmm::pipe::xr::Xr>>,
    /// The vehicle HAL channel, for Automotive images only (as stock).
    pub car: Option<std::sync::Arc<emu_vmm::pipe::car::Car>>,
    /// The adb port, bound for the life of the process so a guest reset reboots on it.
    adb: std::sync::Arc<emu_vmm::pipe::adb::AdbListener>,
    _lock: Option<File>,
    /// This process's claim on its console port ([`claim_port`]).
    _port_claim: File,
    /// `<avd>/simee/console.log`, truncated when the process starts; every boot appends.
    console_log: File,
    /// Where logcat's stream goes (`-logcat`, `-logcat-output`), for the life of the process.
    pub logcat: Option<emu_vmm::virtio::console::Spool>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl Instance {
    pub fn prepare(avd: &Avd, opts: &Opts) -> Result<Instance> {
        let dir = emu_avd::state_dir(&avd.dir);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = if opts.read_only {
            None
        } else {
            let f = File::create(dir.join("instance.lock"))?;
            if f.try_lock().is_err() {
                bail!("AVD '{}' is already running (use -read-only to start another instance)", avd.name);
            }
            Some(f)
        };
        if opts.wipe_data {
            if opts.read_only {
                bail!("-wipe-data and -read-only can't be combined");
            }
            for f in ["userdata.img", "cache.img", "encryptionkey.img"] {
                let p = dir.join(f);
                if p.exists() {
                    std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                }
            }
        }
        // User data starts as an empty ext4 filesystem (like stock): the guest mounts it, then
        // vold sets up metadata encryption in place. The metadata partition starts blank and is
        // formatted by the guest (`formattable` in the fstab). The ext4 formatter is native
        // (`emu-ext4`) and writes only metadata, so the image stays sparse.
        let userdata = dir.join("userdata.img");
        if !userdata.exists()
            && let Err(e) = create_userdata(&userdata, avd.data_partition_bytes())
        {
            let _ = std::fs::remove_file(&userdata);
            return Err(e);
        }
        emu_vmm::disk::ensure_sparse(&dir.join("cache.img"), CACHE_BYTES)?;
        let key_src = avd.sysdir.join("encryptionkey.img");
        if key_src.is_file() {
            emu_vmm::disk::ensure_copy(&key_src, &dir.join("encryptionkey.img"))?;
        } else {
            emu_vmm::disk::ensure_sparse(&dir.join("encryptionkey.img"), 18 << 20)?;
        }
        // `<HOME>/Library/Caches/simee/ports/<port>.lock`, which launchers (sim-android's
        // allocator) also look at.
        let lock_dir = emu_avd::port_lock_dir().unwrap_or_else(|| dir.join("ports"));
        let (console_port, port_claim) = match opts.port {
            // An explicit port is claimed too: a second simee given the same one fails here,
            // clearly, instead of when it binds. Launchers match the message to try another pair.
            Some(p) => {
                let claim = claim_port(&lock_dir, p).with_context(|| format!("console port {p} {}", emu_avd::PORT_TAKEN))?;
                (p, claim)
            }
            None => pick_port(&lock_dir, PORTS, port_free).context("no free console/adb port pair in 5554..5682")?,
        };
        // Opened once per process, not per boot: a guest reset (a panic's reboot) keeps what
        // the boot before it wrote, panic text included.
        let console_log =
            File::create(dir.join("console.log")).with_context(|| format!("creating {}", dir.join("console.log").display()))?;
        let logcat =
            logcat_sink(opts).with_context(|| format!("opening -logcat-output {}", opts.logcat_output.as_deref().unwrap_or("")))?;
        let adb_port = console_port + 1;
        let adb = emu_vmm::pipe::adb::AdbListener::bind(adb_port).with_context(|| format!("binding adb port {adb_port}"))?;
        let gpu_lib64 = renderer(avd, opts);
        if gpu_lib64.is_none() && !gpu_off(opts) {
            eprintln!(
                "emu: no gfxstream renderer in the SDK (emulator/lib64 or simee/renderer); booting without graphics \
                 (install it with emu -provision-renderer)"
            );
        }
        if opts.verbose
            && let Some(lib64) = &gpu_lib64
        {
            eprintln!("emu: gfxstream renderer from {}", lib64.display());
        }
        if let Some(lib64) = &gpu_lib64
            && let Some(emulator_dir) = lib64.parent()
        {
            let icd = match opts.gpu.as_deref() {
                Some("swiftshader" | "swiftshader_indirect") => emu_gfxstream::Icd::SwiftShader,
                Some("lavapipe" | "swangle") => emu_gfxstream::Icd::Lavapipe,
                // MoltenVK (host GPU) unless it fails on this machine; then software.
                _ if crate::vkprobe::moltenvk_works(lib64) => emu_gfxstream::Icd::MoltenVk,
                _ => {
                    eprintln!("emu: MoltenVK unavailable here; using lavapipe (software Vulkan)");
                    emu_gfxstream::Icd::Lavapipe
                }
            };
            // The launcher directory is the parent of lib64: the SDK's `emulator/` or
            // `<sdk>/simee/renderer/<revision>/`; gfxstream finds `lib64/vulkan` and
            // `lib64/gles_*` under it.
            emu_gfxstream::link_beside_exe(lib64);
            emu_gfxstream::configure_environment(emulator_dir, icd);
        }
        let display = gpu_lib64.as_ref().map(|_| std::sync::Arc::new(emu_vmm::display::FrameStore::default()));
        Ok(Instance {
            avd: avd.clone(),
            console_port,
            display,
            gpu_lib64,
            inputs: Default::default(),
            sensors: std::sync::Arc::new(emu_vmm::pipe::sensors::Sensors::new(avd.sensor_mask())),
            clipboard: Default::default(),
            fingerprint: Default::default(),
            gps: Default::default(),
            battery: Default::default(),
            telephony: Default::default(),
            xr: is_xr(avd).then(Default::default),
            car: avd.is_car().then(Default::default),
            adb: std::sync::Arc::new(adb),
            _lock: lock,
            _port_claim: port_claim,
            console_log,
            logcat,
        })
    }

    pub fn plan(&self, opts: &Opts) -> BootPlan {
        BootPlan::new(&self.avd, opts, self.gpu_lib64.clone())
    }

    /// The GPU this boot gets, if any (`emu_vmm::start_renderer` takes it early, `run` again).
    pub fn gpu_spec(&self, opts: &Opts) -> Option<emu_vmm::GpuSpec> {
        self.gpu_lib64.as_ref().zip(self.display.as_ref()).map(|(lib64, display)| emu_vmm::GpuSpec {
            lib64: lib64.clone(),
            width: self.avd.lcd().0,
            height: self.avd.lcd().1,
            verbose: opts.verbose,
            display: display.clone(),
        })
    }

    /// `launched`: when the process started, for `-verbose` launch timings.
    pub fn machine_config(&self, opts: &Opts, launched: std::time::Instant) -> Result<emu_vmm::MachineConfig> {
        let plan = self.plan(opts);
        if let Some(missing) = plan.missing().first() {
            bail!("missing {}", missing.display());
        }
        for d in &plan.disks {
            if !d.path.is_file() {
                bail!("missing disk image {}", d.path.display());
            }
        }
        let mut initrd = std::fs::read(&plan.ramdisk).with_context(|| format!("reading {}", plan.ramdisk.display()))?;
        if !plan.bootconfig.entries().is_empty() {
            initrd.extend_from_slice(&plan.bootconfig.encode());
        }
        let log = self.console_log.try_clone().context("console.log")?;
        let console: Box<dyn Write + Send> = if opts.show_kernel { Box::new(Tee(std::io::stdout(), log)) } else { Box::new(log) };
        let logcat = self.logcat.clone();

        Ok(emu_vmm::MachineConfig {
            cpus: plan.cpus,
            ram_bytes: plan.ram_bytes,
            kernel: plan.kernel,
            // Inflated gzip kernels, beside vkprobe's verdicts.
            kernel_cache: std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/simee")),
            initrd: Some(initrd),
            cmdline: plan.cmdline,
            disks: plan.disks,
            disk_fsync: opts.disk_fsync,
            rng: true,
            adb: Some(self.adb.clone()),
            gpu: self.gpu_spec(opts),
            boot_props: plan.boot_props,
            input: Some((self.avd.lcd().0, self.avd.lcd().1)),
            touch_screen: plan.touch_screen,
            touchpad: plan.touchpad,
            dual_mode_mouse: plan.dual_mode_mouse,
            xr: self.xr.clone(),
            sensors: self.sensors.clone(),
            clipboard: self.clipboard.clone(),
            fingerprint: self.fingerprint.clone(),
            gps: self.gps.clone(),
            battery: self.battery.clone(),
            telephony: self.telephony.clone(),
            car: self.car.clone(),
            sound: plan.sound,
            on_guest_event: {
                let started = std::time::Instant::now();
                Some(std::sync::Arc::new(move |event: &str| {
                    if event == "bootcomplete" {
                        // On stdout, like the launch line: tools can watch for it.
                        println!("emu: boot completed in {:.1}s", started.elapsed().as_secs_f64());
                    }
                }))
            },
            on_input: {
                let slot = self.inputs.clone();
                Some(std::sync::Arc::new(move |i| *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(i)))
            },
            console,
            logcat,
            launched: opts.verbose.then_some(launched),
        })
    }
}

/// Console ports stock and simee pick from (adb is each one + 1); stock's ceiling.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub const PORTS: std::ops::RangeInclusive<u16> = 5554..=5682;

/// Claims console port `port` for this process: its lock file, held until the returned file is
/// dropped or the process exits. None when another simee instance holds it.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub fn claim_port(dir: &Path, port: u16) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let f = std::fs::File::create(dir.join(format!("{port}.lock"))).ok()?;
    f.try_lock().ok().map(|()| f)
}

/// The first even console port in `range` this process could claim whose console and adb
/// ports are free (`free`), with its claim. Instances launched together used to race between
/// probing a pair free and binding it (both picked the same pair, one then failed); the claim is
/// taken before the probe and kept, so two simee instances never pick one pair. A stock
/// emulator doesn't take claims, so the bind probe still keeps clear of the ones it holds.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub fn pick_port(dir: &Path, range: std::ops::RangeInclusive<u16>, free: impl Fn(u16) -> bool) -> Option<(u16, std::fs::File)> {
    range.step_by(2).find_map(|p| {
        let claim = claim_port(dir, p)?;
        (free(p) && free(p + 1)).then_some((p, claim))
    })
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn port_free(p: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, p)).is_ok()
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
/// Creates `image` as a sparse, empty ext4 filesystem of `size` bytes.
fn create_userdata(image: &std::path::Path, size: u64) -> Result<()> {
    let mut uuid = [0u8; 16];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut uuid)).context("reading /dev/urandom")?;
    uuid[6] = (uuid[6] & 0x0F) | 0x40; // RFC 4122 version 4 (random)
    uuid[8] = (uuid[8] & 0x3F) | 0x80;
    let file = File::options().read(true).write(true).create_new(true).open(image)?;
    emu_ext4::format(&file, size, "userdata", uuid).with_context(|| format!("formatting {} as ext4", image.display()))
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
struct Tee<A, B>(A, B);

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl<A: Write, B: Write> Write for Tee<A, B> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write_all(buf)?;
        self.1.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()?;
        self.1.flush()
    }
}

/// `-logcat-output FILE` rolls over to `FILE.1` past this size, so a long-lived device keeps at
/// most twice this on disk.
pub const LOGCAT_ROTATE_BYTES: u64 = 4 << 20;

/// Guest buffers logcat's spool holds while its writer is behind (each one hvc1 buffer, up to a
/// page); beyond that output is dropped and counted rather than stalling the console worker.
const LOGCAT_SPOOL_DEPTH: usize = 1024;

/// Where logcat's stream goes for this process: `-logcat-output`'s file (rotated), stdout with
/// `-logcat` alone, else nowhere. Behind a spool either way.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
pub fn logcat_sink(opts: &Opts) -> std::io::Result<Option<emu_vmm::virtio::console::Spool>> {
    let out: Box<dyn std::io::Write + Send> = match (&opts.logcat_output, &opts.logcat) {
        (Some(path), _) => Box::new(RotatingFile::create(Path::new(path), LOGCAT_ROTATE_BYTES)?),
        (None, Some(_)) => Box::new(std::io::stdout()),
        (None, None) => return Ok(None),
    };
    emu_vmm::virtio::console::Spool::new("logcat-out", out, LOGCAT_SPOOL_DEPTH).map(Some)
}

/// A log file that, before a write would take it past `cap` bytes, is renamed to `<path>.1`
/// (replacing the previous one) and started again.
pub struct RotatingFile {
    path: PathBuf,
    file: std::fs::File,
    len: u64,
    cap: u64,
}

impl RotatingFile {
    /// Creates (truncates) `path`.
    pub fn create(path: &Path, cap: u64) -> std::io::Result<RotatingFile> {
        let file = std::fs::File::create(path)?;
        Ok(RotatingFile { path: path.to_owned(), file, len: 0, cap })
    }

    fn rotated(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".1");
        PathBuf::from(p)
    }
}

impl std::io::Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.len > 0 && self.len + buf.len() as u64 > self.cap {
            std::io::Write::flush(&mut self.file)?;
            std::fs::rename(&self.path, self.rotated())?;
            self.file = std::fs::File::create(&self.path)?;
            self.len = 0;
        }
        std::io::Write::write_all(&mut self.file, buf)?;
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

fn gpu_off(opts: &Opts) -> bool {
    matches!(opts.gpu.as_deref(), Some("off" | "none"))
}

/// The gfxstream renderer's `lib64` the AVD's SDK offers, unless `-gpu off`.
pub fn renderer(avd: &Avd, opts: &Opts) -> Option<PathBuf> {
    if gpu_off(opts) { None } else { avd.sdk_root.as_deref().and_then(emu_sdk::renderer::find_lib64) }
}

fn is_xr(avd: &Avd) -> bool {
    avd.profile().kind.sub_platform == SubPlatform::Android(AndroidOs::Xr)
}

/// Everything a boot of one AVD involves, decided from its config and image with no side
/// effects: what the guest is told (kernel command line, bootconfig, boot properties) and what
/// the board gives it (disks, devices, sensors).
pub struct BootPlan {
    pub avd: String,
    /// `Android TV · TV`, ...
    pub kind: String,
    pub kernel: PathBuf,
    pub ramdisk: PathBuf,
    pub cmdline: String,
    pub bootconfig: BootConfig,
    pub boot_props: Vec<(String, String)>,
    pub disks: Vec<DiskSpec>,
    pub cpus: u32,
    pub ram_bytes: u64,
    /// Width, height, density.
    pub screen: (u32, u32, u32),
    /// The gfxstream renderer's `lib64`; None boots without graphics.
    pub renderer: Option<PathBuf>,
    /// A silent virtio-snd card.
    pub sound: bool,
    /// `qemud:xr_service`.
    pub xr: bool,
    /// The vehicle HAL channel, `qemud:car`.
    pub car: bool,
    /// Sensor handles the guest's sensors HAL is offered.
    pub sensor_mask: u32,
    /// A multi-touch screen (not on `hw.screen=no-touch`, TV's).
    pub touch_screen: bool,
    /// A touchpad's size in units (`hw.touchpad0`, the display glasses').
    pub touchpad: Option<(u32, u32)>,
    /// The dual-mode mouse: XR images that enable `VirtioDualModeMouse` (the headsets, not the
    /// display glasses), as stock adds it.
    pub dual_mode_mouse: bool,
}

impl BootPlan {
    pub fn new(avd: &Avd, opts: &Opts, renderer: Option<PathBuf>) -> BootPlan {
        let dir = emu_avd::state_dir(&avd.dir);
        let user_mode = if opts.read_only { DiskMode::Ephemeral } else { DiskMode::Persistent };
        let disk = |slot, path: PathBuf, mode, serial: &str| DiskSpec { slot, path, mode, serial: serial.to_string() };
        let mut cmdline = String::from(BASE_CMDLINE);
        if !full_console(opts) {
            cmdline.push(' ');
            cmdline.push_str(QUIET_CONSOLE);
        }
        let extra = avd.image_cmdline();
        if !extra.is_empty() {
            cmdline.push(' ');
            cmdline.push_str(&extra);
        }
        let (xr, car) = (is_xr(avd), avd.is_car());
        let mut plan = BootPlan {
            avd: avd.name.clone(),
            kind: avd.profile().kind.label(),
            kernel: avd.kernel(),
            ramdisk: avd.ramdisk(),
            cmdline,
            bootconfig: BootConfig::new(),
            boot_props: boot_props(avd),
            disks: vec![
                disk(slots::SYSTEM, avd.system_image(), DiskMode::Ephemeral, "system"),
                disk(slots::CACHE, dir.join("cache.img"), user_mode, "cache"),
                disk(slots::USERDATA, dir.join("userdata.img"), user_mode, "userdata"),
                disk(slots::METADATA, dir.join("encryptionkey.img"), user_mode, "metadata"),
                disk(slots::VENDOR, avd.vendor_image(), DiskMode::Ephemeral, "vendor"),
            ],
            cpus: opts.cores.unwrap_or_else(|| avd.cpus()),
            ram_bytes: opts.memory_mb.map_or_else(|| avd.ram_bytes(), |mb| mb << 20),
            screen: avd.lcd(),
            renderer,
            sound: sound(xr, car),
            xr,
            car,
            sensor_mask: avd.sensor_mask(),
            touch_screen: avd.touch_screen(),
            touchpad: avd.touchpad(),
            dual_mode_mouse: xr && avd.image_feature("VirtioDualModeMouse").unwrap_or(true),
        };
        let bc = bootconfig(avd, opts, plan.renderer.is_some());
        if takes_bootconfig(avd) {
            plan.bootconfig = bc;
        } else {
            // API 30 and older: kernels without bootconfig (4.14, android11-5.4) take a bootconfig
            // block after the ramdisk as junk, fall back to an initrd and panic ("Unable to mount
            // root fs on unknown-block(0,0)"). Stock passes these images everything on the
            // command line instead, under the older names.
            plan.cmdline = plan.cmdline.replace(" bootconfig", "");
            plan.cmdline.push(' ');
            plan.cmdline.push_str(&legacy_cmdline(&bc, plan.renderer.is_some()));
        }
        plan
    }

    /// Image files the boot reads that aren't there. The AVD's own disks under `simee/` are
    /// created at boot, so only the image's are checked.
    pub fn missing(&self) -> Vec<&Path> {
        let image = self.disks.iter().filter(|d| d.mode == DiskMode::Ephemeral && !d.path.starts_with(self.state_dir()));
        [self.kernel.as_path(), self.ramdisk.as_path()]
            .into_iter()
            .chain(image.map(|d| d.path.as_path()))
            .filter(|p| !p.is_file())
            .collect()
    }

    fn state_dir(&self) -> PathBuf {
        self.disks.iter().find(|d| d.serial == "userdata").and_then(|d| d.path.parent()).map(Path::to_path_buf).unwrap_or_default()
    }

    /// The virtio devices and host services the board gives the guest.
    pub fn devices(&self) -> Vec<&'static str> {
        let mut out = vec![
            "virtio-blk x5",
            "virtio-console (logcat, modem)",
            "virtio-net (link only)",
            "virtio-rng",
            "virtio-vsock (adb, qemud pipes)",
        ];
        out.push(if self.touch_screen {
            "virtio-input: multi-touch, keyboard (qwerty2), rotary"
        } else {
            "virtio-input: keyboard (qwerty2), rotary"
        });
        if self.dual_mode_mouse {
            out.push("virtio-input: dual-mode mouse");
        }
        if self.touchpad.is_some() {
            out.push("virtio-input: touchpad");
        }
        if self.renderer.is_some() {
            out.push("virtio-gpu (gfxstream), goldfish address space");
        }
        if self.sound {
            out.push("virtio-snd (silent)");
        }
        out.push("goldfish battery, PL011, PL031");
        out.extend(["qemud:sensors", "qemud:gps", "qemud:fingerprintlisten", "qemud:camera", "qemud:boot-properties", "clipboard"]);
        if self.xr {
            out.push("qemud:xr_service");
        }
        if self.car {
            out.push("qemud:car (vehicle HAL)");
        }
        out
    }
}

impl std::fmt::Display for BootPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h, dpi) = self.screen;
        writeln!(f, "avd       {} ({})", self.avd, self.kind)?;
        writeln!(f, "machine   {} vCPUs, {} MB RAM, {w}x{h} @ {dpi} dpi", self.cpus, self.ram_bytes >> 20)?;
        writeln!(f, "kernel    {}", self.kernel.display())?;
        writeln!(f, "ramdisk   {} + bootconfig ({} entries)", self.ramdisk.display(), self.bootconfig.entries().len())?;
        writeln!(f, "cmdline   {}", self.cmdline)?;
        match &self.renderer {
            Some(lib64) => writeln!(f, "graphics  gfxstream from {}", lib64.display())?,
            None => writeln!(f, "graphics  none: no renderer (-gpu off, or none installed: emu -provision-renderer)")?,
        }
        writeln!(f, "devices   {}", self.devices().join(", "))?;
        let sensors: Vec<&str> =
            emu_vmm::pipe::sensors::NAMES.iter().enumerate().filter(|(h, _)| self.sensor_mask & (1 << h) != 0).map(|(_, n)| *n).collect();
        writeln!(f, "sensors   {}", sensors.join(", "))?;
        writeln!(f, "disks")?;
        for d in &self.disks {
            let mode = match d.mode {
                DiskMode::Persistent => "",
                DiskMode::Ephemeral => " (writes discarded)",
            };
            writeln!(f, "  {:<9} slot {:<2} {}{mode}", d.serial, d.slot, d.path.display())?;
        }
        writeln!(f, "bootconfig")?;
        for (k, v) in self.bootconfig.entries() {
            writeln!(f, "  {k}={v}")?;
        }
        writeln!(f, "boot props")?;
        for (k, v) in &self.boot_props {
            writeln!(f, "  {k}={v}")?;
        }
        Ok(())
    }
}

/// A silent virtio-snd card for images whose audio HAL needs ALSA card 0: Automotive
/// (`audio.primary.caremu` crashes without one) and Android XR (its images enable `VirtioSndCard`).
/// `EMU_VIRTIO_SND=1` or `0` forces it on or off for any image. `-no-audio` leaves it alone:
/// the card never plays on the host anyway.
fn sound(xr: bool, car: bool) -> bool {
    match std::env::var("EMU_VIRTIO_SND").as_deref() {
        Ok("1" | "on" | "yes") => true,
        Ok("0" | "off" | "no") => false,
        _ => car || xr,
    }
}

fn bootconfig(avd: &Avd, opts: &Opts, graphics: bool) -> BootConfig {
    let (w, h, _density) = avd.lcd();
    let mut bc = BootConfig::new();
    bc.set("androidboot.hardware", "ranchu")
        .set("androidboot.boot_devices", "a003600.virtio_mmio")
        .set("androidboot.qemu", "1")
        .set("androidboot.qemu.avd_name", avd.name.as_str())
        .set("androidboot.serialno", "EMULATOR0EMURS0")
        .set("androidboot.veritymode", "enforcing")
        .set("androidboot.dalvik.vm.heapsize", "576m")
        .set("androidboot.logcat", logcat_filter(opts))
        .set("androidboot.qemu.skin", format!("{w}x{h}"))
        .set("androidboot.qemu.vsync", "60")
        .set("androidboot.qemu.settings.system.screen_off_timeout", "2147483647")
        .set("androidboot.opengles.version", "196609");
    if graphics {
        // gfxstream over virtio-gpu: GLES through the pipe stream, Vulkan and ANGLE through
        // address-space graphics, both on the host renderer.
        bc.set("androidboot.hardware.gltransport", "virtio-gpu-asg").set("androidboot.hardware.vulkan", "ranchu");
        // Guest GLES through ANGLE on the guest's Vulkan, so every context uses
        // address-space graphics: with `emulation` GLES rides the virtio-gpu pipe stream,
        // whose blocking reads serialize all processes on one queue (boots took 14–28 s with
        // ANRs, against 5–8 s on ANGLE). EMU_GUEST_EGL=emulation restores it.
        let egl = std::env::var("EMU_GUEST_EGL").unwrap_or_else(|_| "angle".into());
        bc.set("androidboot.hardwareegl", egl);
    }
    if opts.no_boot_anim {
        bc.set("androidboot.debug.sf.nobootanimation", "1");
    }
    // A round screen (boot.prop's `ro.emulator.circular`), as stock tells the guest.
    if avd.circular() {
        bc.set("androidboot.emulator.circular", "1");
    }
    for (k, v) in avd.vbmeta_params() {
        bc.set(k, v);
    }
    // Pre-authorize the host's adb key, like the stock emulator.
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(key) = std::fs::read_to_string(PathBuf::from(home).join(".android/adbkey.pub"))
    {
        bc.set("androidboot.qemu.adb.pubkey", key.trim());
    }
    bc
}

/// Whether the image's kernel reads bootconfig: images say so with `AndroidbootProps2` (every
/// image from API 31 on), and stock passes bootconfig only then. An image that says nothing and
/// isn't known to be older (no `advancedFeatures.ini`, or no API level) is taken as new.
fn takes_bootconfig(avd: &Avd) -> bool {
    avd.image_feature("AndroidbootProps2") == Some(true) || avd.api_level().is_none_or(|api| api >= 31)
}

/// Why emu can't run this AVD's image yet, if it can't: system images below API 34 boot their
/// kernel and init but never get graphics, so a boot would only loop. emu refuses them up front
/// with the reason; `EMU_ALLOW_OLD_IMAGES=1` boots them anyway, for work on the issues named.
pub fn unsupported(avd: &Avd) -> Option<String> {
    let api = avd.api_level()?;
    let why = match api {
        ..=30 => {
            "its graphics, modem and logcat use goldfish pipes emu doesn't serve yet (`opengles`, `qemud:gsm`; https://github.com/kaeawc/simee/issues/38)"
        }
        31..=33 => {
            "its address-space graphics ring is a guest-backed 3D resource the renderer can't map yet (https://github.com/kaeawc/simee/issues/39)"
        }
        _ => return None,
    };
    Some(format!(
        "{}: API {api} images don't run on emu yet: {why}. emu runs API 34 and newer; create the AVD from a newer image (`emu -provision 36`), or boot this one with the SDK's emulator",
        avd.name
    ))
}

/// Whether `EMU_ALLOW_OLD_IMAGES` lets an unsupported image boot anyway.
pub fn allow_old_images() -> bool {
    std::env::var("EMU_ALLOW_OLD_IMAGES").is_ok_and(|v| v == "1")
}

/// The kernel command line stock gives images without `AndroidbootProps2` (API 30 and older,
/// whose kernels lack bootconfig), from the same entries: `androidboot.qemu.*` as `qemu.*`, the
/// few properties bootconfig renamed back to their `qemu.*` names, the rest as they are. Init
/// splits the command line on spaces without quoting, so a value with a space (the adb key) is
/// left out; those images' adbd doesn't read it. ANGLE isn't on these images: GLES is the
/// image's own `emulation` driver (`qemu.gles=1`).
fn legacy_cmdline(bc: &BootConfig, graphics: bool) -> String {
    let mut out = vec!["android.qemud=1".to_string(), "qemu.encrypt=1".to_string()];
    if graphics {
        out.push("qemu.gles=1".to_string());
    }
    for (k, v) in bc.entries() {
        if v.contains(char::is_whitespace) || k == "androidboot.hardwareegl" {
            continue;
        }
        let key = match k.as_str() {
            "androidboot.qemu" => "qemu".to_string(),
            "androidboot.dalvik.vm.heapsize" => "qemu.dalvik.vm.heapsize".to_string(),
            "androidboot.opengles.version" => "qemu.opengles.version".to_string(),
            "androidboot.hardware.gltransport" => "qemu.gltransport".to_string(),
            _ => match k.strip_prefix("androidboot.qemu.") {
                Some(rest) => format!("qemu.{rest}"),
                None => k.clone(),
            },
        };
        out.push(format!("{key}={v}"));
    }
    out.join(" ")
}

/// What the stock host hands `qemu-props` at boot.
fn boot_props(avd: &Avd) -> Vec<(String, String)> {
    let mut props = avd.qemu_props();
    if let Ok(tz) = std::fs::read_link("/etc/localtime")
        && let Some(name) = tz.to_str().and_then(|s| s.split("zoneinfo/").nth(1))
    {
        props.push(("qemu.timezone".to_string(), name.to_string()));
    }
    props
}

#[cfg(test)]
mod tests {
    use emu_avd::{Env, NewAvd};

    use super::*;

    /// An SDK with one image per form factor (its tag, `build.prop` and `advancedFeatures.ini`
    /// as the real images have them) and an AVD made from each the way `emu -create-avd` does.
    struct Sdk {
        root: PathBuf,
        env: Env,
    }

    impl Drop for Sdk {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn sdk(tag: &str) -> Sdk {
        let root = std::env::temp_dir().join(format!("emu-plan-{tag}-{}", std::process::id()));
        let sdk = root.join("sdk");
        let images: [(&str, &str, &str); 6] = [
            ("android-36/google_apis/arm64-v8a", "ro.product.name=sdk_gphone64_arm64\n", "VirtioSndCard = on\n"),
            ("android-36/android-wear-signed/arm64-v8a", "", "VirtioSndCard = on\n"),
            ("android-34/android-tv/arm64-v8a", "", "VirtioSndCard = on\n"),
            ("android-34-ext9/android-automotive/arm64-v8a", "ro.product.name=sdk_car_arm64\n", "VirtioSndCard = on\n"),
            ("android-34/google-xr/arm64-v8a", "", "GuestAngle = on\nVirtioDualModeMouse = on\n"),
            ("android-36/ai-glasses/arm64-v8a", "", "VirtioSndCard = on\nXrStatusLed = on\n"),
        ];
        for (dir, build_prop, features) in images {
            let d = sdk.join("system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            for f in ["kernel-ranchu", "ramdisk.img", "system.img", "vendor.img"] {
                std::fs::write(d.join(f), b"x").expect("image file");
            }
            if !build_prop.is_empty() {
                std::fs::write(d.join("build.prop"), build_prop).expect("build.prop");
            }
            std::fs::write(d.join("advancedFeatures.ini"), features).expect("features");
            std::fs::write(d.join("VerifiedBootParams.textproto"), "param: \"androidboot.vbmeta.size=6592\"\n").expect("vbmeta");
        }
        std::fs::write(sdk.join("system-images/android-36/ai-glasses/arm64-v8a/kernel_cmdline.txt"), "8250.nr_uarts=1\n").expect("cmdline");
        let env = Env { sdk_root: Some(sdk.clone()), avd_home: root.join("avd") };
        for (name, dir) in [
            ("phone", "android-36/google_apis/arm64-v8a"),
            ("watch", "android-36/android-wear-signed/arm64-v8a"),
            ("tv", "android-34/android-tv/arm64-v8a"),
            ("car", "android-34-ext9/android-automotive/arm64-v8a"),
            ("headset", "android-34/google-xr/arm64-v8a"),
            ("glasses", "android-36/ai-glasses/arm64-v8a"),
        ] {
            env.create_avd(&NewAvd::for_image(name, &format!("system-images/{dir}"))).expect("create");
        }
        Sdk { root, env }
    }

    /// Adds an image of `api` (with `features` as its `advancedFeatures.ini`) and an AVD on it.
    fn add_image(s: &Sdk, name: &str, api: u32, features: &str) {
        let rel = format!("system-images/android-{api}/default/arm64-v8a");
        let d = s.root.join("sdk").join(&rel);
        std::fs::create_dir_all(&d).expect("image dir");
        for f in ["kernel-ranchu", "ramdisk.img", "system.img", "vendor.img"] {
            std::fs::write(d.join(f), b"x").expect("image file");
        }
        std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel={api}\n")).expect("source.properties");
        std::fs::write(d.join("advancedFeatures.ini"), features).expect("features");
        s.env.create_avd(&NewAvd::for_image(name, &rel)).expect("create");
    }

    #[test]
    fn images_without_bootconfig_get_stocks_legacy_command_line() {
        let s = sdk("legacy");
        add_image(&s, "q", 29, "GLAsyncSwap = on\n");
        add_image(&s, "s", 33, "AndroidbootProps2 = on\n");

        // The 4.14 kernel would take a bootconfig block after the ramdisk as junk and panic.
        let q = plan(&s, "q");
        assert!(q.bootconfig.entries().is_empty(), "{:?}", q.bootconfig.entries());
        let words: Vec<&str> = q.cmdline.split(' ').collect();
        assert!(!words.contains(&"bootconfig"), "{}", q.cmdline);
        // As the stock emulator passes them to the same API 29 image.
        for w in [
            "android.qemud=1",
            "qemu.encrypt=1",
            "androidboot.hardware=ranchu",
            "androidboot.boot_devices=a003600.virtio_mmio",
            "qemu=1",
            "qemu.avd_name=q",
            "qemu.dalvik.vm.heapsize=576m",
            "qemu.opengles.version=196609",
            "qemu.vsync=60",
            "qemu.settings.system.screen_off_timeout=2147483647",
        ] {
            assert!(words.contains(&w), "{w} missing from {}", q.cmdline);
        }
        assert!(!q.cmdline.contains("androidboot.qemu"), "{}", q.cmdline);

        let new = plan(&s, "s");
        assert_eq!(new.bootconfig.get("androidboot.qemu.avd_name"), Some("s"));
        assert!(new.cmdline.split(' ').any(|w| w == "bootconfig") && !new.cmdline.contains("qemu.avd_name"), "{}", new.cmdline);
        let phone = plan(&s, "phone");
        assert_eq!(phone.bootconfig.get("androidboot.hardware"), Some("ranchu"), "no API level or flag: taken as new");
    }

    #[test]
    fn legacy_command_line_renames_graphics_and_drops_what_init_cannot_split() {
        let mut bc = BootConfig::new();
        bc.set("androidboot.hardware.gltransport", "virtio-gpu-asg")
            .set("androidboot.hardwareegl", "angle")
            .set("androidboot.qemu.adb.pubkey", "QAAAAB== user@host")
            .set("androidboot.vbmeta.size", "4352");
        let line = legacy_cmdline(&bc, true);
        assert_eq!(line, "android.qemud=1 qemu.encrypt=1 qemu.gles=1 qemu.gltransport=virtio-gpu-asg androidboot.vbmeta.size=4352");
        assert!(!legacy_cmdline(&bc, false).contains("qemu.gles"));
    }

    #[test]
    fn images_below_api_34_are_refused_with_the_reason() {
        let s = sdk("old");
        add_image(&s, "q", 29, "");
        add_image(&s, "r", 30, "");
        add_image(&s, "s", 31, "AndroidbootProps2 = on\n");
        add_image(&s, "t", 33, "AndroidbootProps2 = on\n");
        add_image(&s, "u", 34, "AndroidbootProps2 = on\n");
        let why = |n: &str| unsupported(&s.env.load(n).expect("load"));
        for (n, api, issue) in [("q", 29, "issues/38"), ("r", 30, "issues/38"), ("s", 31, "issues/39"), ("t", 33, "issues/39")] {
            let w = why(n).unwrap_or_default();
            assert!(w.starts_with(&format!("{n}: API {api} images")) && w.contains(issue) && w.contains("API 34 and newer"), "{w}");
        }
        assert_eq!(why("u"), None);
        assert_eq!(why("phone"), None, "no API level: not refused");
    }

    fn plan(s: &Sdk, name: &str) -> BootPlan {
        BootPlan::new(&s.env.load(name).expect("load"), &Opts::default(), None)
    }

    #[test]
    fn logcat_files_rotate_at_their_cap() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("emu-logcat-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("logcat.txt");
        std::fs::write(&path, "old run\n").expect("old");
        let mut f = RotatingFile::create(&path, 10).expect("create");
        assert_eq!(std::fs::read(&path).expect("read"), b"", "a new process starts the file again");
        f.write_all(b"aaaa\n").and_then(|()| f.write_all(b"bbbb\n")).expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"aaaa\nbbbb\n");
        f.write_all(b"cccc\n").expect("write");
        assert_eq!(std::fs::read(dir.join("logcat.txt.1")).expect("rotated"), b"aaaa\nbbbb\n");
        assert_eq!(std::fs::read(&path).expect("read"), b"cccc\n");
        // A chunk bigger than the cap still goes in whole, into a fresh file.
        f.write_all(&[b'x'; 25]).expect("write");
        assert_eq!(std::fs::read(dir.join("logcat.txt.1")).expect("rotated"), b"cccc\n");
        assert_eq!(std::fs::read(&path).expect("read").len(), 25);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A guest reset builds a new machine config; what the boot before it logged (a panic's
    /// text) stays in console.log and the logcat file.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn a_reset_keeps_the_logs_of_the_boot_before() {
        let s = sdk("reset-logs");
        let avd = s.env.load("phone").expect("load");
        let logcat = s.root.join("logcat.txt");
        // A port outside 5554..5682, so no real emulator or farm is affected by its claim.
        let opts = Opts { port: Some(6990), gpu: Some("off".into()), logcat_output: Some(logcat.display().to_string()), ..Opts::default() };
        let inst = Instance::prepare(&avd, &opts).expect("prepare");
        let launched = std::time::Instant::now();
        let mut first = inst.machine_config(&opts, launched).expect("config");
        first.console.write_all(b"Kernel panic - not syncing: sysrq triggered crash\n").expect("console");
        first.logcat.as_ref().expect("logcat").send(b"F/libc: Fatal signal 6\n");
        drop(first);
        let mut second = inst.machine_config(&opts, launched).expect("config after reset");
        second.console.write_all(b"Booting Linux\n").expect("console");
        second.logcat.as_ref().expect("logcat").send(b"I/init: second boot\n");
        let console = std::fs::read_to_string(emu_avd::state_dir(&avd.dir).join("console.log")).expect("console.log");
        assert_eq!(console, "Kernel panic - not syncing: sysrq triggered crash\nBooting Linux\n");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::fs::read_to_string(&logcat).unwrap_or_default() != "F/libc: Fatal signal 6\nI/init: second boot\n" {
            assert!(std::time::Instant::now() < deadline, "{:?}", std::fs::read_to_string(&logcat));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop((second, inst));
        if let Some(locks) = emu_avd::port_lock_dir() {
            let _ = std::fs::remove_file(locks.join("6990.lock"));
        }
    }

    #[test]
    fn parallel_launches_claim_distinct_ports() {
        let dir = std::env::temp_dir().join(format!("emu-ports-{}", std::process::id()));
        // Sixteen launches at once, every pair probing free: each gets its own pair.
        let picked: Vec<(u16, std::fs::File)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16).map(|_| s.spawn(|| pick_port(&dir, 5600..=5682, |_| true).expect("a port"))).collect();
            handles.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        let mut ports: Vec<u16> = picked.iter().map(|(p, _)| *p).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 16, "{ports:?}");
        assert!(ports.iter().all(|p| p % 2 == 0 && (5600..=5630).contains(p)), "{ports:?}");
        // A held claim is skipped and an explicit claim of it refused; a pair in use (a stock
        // emulator's) is skipped too. Dropping a claim frees its port.
        assert!(claim_port(&dir, 5600).is_none());
        assert_eq!(pick_port(&dir, 5600..=5682, |p| p != 5633).map(|(p, _)| p), Some(5634));
        drop(picked);
        assert_eq!(pick_port(&dir, 5600..=5682, |_| true).map(|(p, _)| p), Some(5600));
        assert!(pick_port(&dir, 5600..=5602, |_| false).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_form_factor_gets_its_devices() {
        let s = sdk("devices");
        let shape = |p: &BootPlan| (p.touch_screen, p.touchpad, p.dual_mode_mouse, p.xr, p.car, p.sound);
        assert_eq!(shape(&plan(&s, "phone")), (true, None, false, false, false, false));
        assert_eq!(shape(&plan(&s, "watch")), (true, None, false, false, false, false));
        assert_eq!(shape(&plan(&s, "tv")), (false, None, false, false, false, false), "no touch screen");
        assert_eq!(shape(&plan(&s, "car")), (true, None, false, false, true, true), "the VHAL and a sound card");
        assert_eq!(shape(&plan(&s, "headset")), (true, None, true, true, false, true));
        assert_eq!(shape(&plan(&s, "glasses")), (true, Some((1542, 297)), false, true, false, true), "a touchpad, no mouse");

        let tv = plan(&s, "tv").devices().join(", ");
        assert!(tv.contains("virtio-input: keyboard (qwerty2), rotary") && !tv.contains("multi-touch"), "{tv}");
        let glasses = plan(&s, "glasses").devices().join(", ");
        assert!(glasses.contains("virtio-input: touchpad") && !glasses.contains("dual-mode mouse"), "{glasses}");
        assert!(plan(&s, "car").devices().contains(&"qemud:car (vehicle HAL)"));
        assert!(plan(&s, "headset").devices().contains(&"qemud:xr_service"));
    }

    #[test]
    fn plans_tell_the_guest_its_screen_and_shape() {
        let s = sdk("boot");
        let watch = plan(&s, "watch");
        assert_eq!((watch.screen, watch.ram_bytes), ((454, 454, 320), 1 << 30), "512 MB, booted with stock's padding");
        let bc = |p: &BootPlan, k: &str| p.bootconfig.get(k).map(str::to_owned);
        assert_eq!(bc(&watch, "androidboot.emulator.circular").as_deref(), Some("1"));
        assert_eq!(bc(&watch, "androidboot.qemu.skin").as_deref(), Some("454x454"));
        assert_eq!(bc(&watch, "androidboot.qemu.avd_name").as_deref(), Some("watch"));
        assert_eq!(bc(&watch, "androidboot.vbmeta.size").as_deref(), Some("6592"));
        assert_eq!(bc(&watch, "androidboot.hardware.gltransport"), None, "no renderer, no graphics transport");
        assert!(watch.boot_props.contains(&("qemu.hw.mainkeys".into(), "1".into())));
        assert_eq!(bc(&plan(&s, "phone"), "androidboot.emulator.circular"), None);

        let with_gpu = BootPlan::new(&s.env.load("phone").expect("load"), &Opts::default(), Some(PathBuf::from("/r/lib64")));
        assert_eq!(bc(&with_gpu, "androidboot.hardware.gltransport").as_deref(), Some("virtio-gpu-asg"));
        assert!(with_gpu.devices().iter().any(|d| d.starts_with("virtio-gpu")));

        let glasses = plan(&s, "glasses");
        assert!(
            glasses.cmdline.starts_with(BASE_CMDLINE) && glasses.cmdline.ends_with(" 8250.nr_uarts=1"),
            "the image's cmdline is appended"
        );
        assert_eq!((glasses.screen, glasses.ram_bytes), ((450, 450, 160), 2 << 30));
        assert_eq!(plan(&s, "car").ram_bytes, 3686 << 20);
        assert_eq!(plan(&s, "car").kind, "Android Automotive · Auto");
    }

    #[test]
    fn plans_follow_the_command_line_and_find_missing_files() {
        let s = sdk("opts");
        let avd = s.env.load("phone").expect("load");
        let opts = Opts { cores: Some(2), memory_mb: Some(1536), read_only: true, no_boot_anim: true, ..Opts::default() };
        let p = BootPlan::new(&avd, &opts, None);
        assert_eq!((p.cpus, p.ram_bytes), (2, 1536 << 20));
        assert!(p.disks.iter().all(|d| d.mode == DiskMode::Ephemeral), "-read-only keeps every write in memory");
        assert_eq!(p.bootconfig.get("androidboot.debug.sf.nobootanimation"), Some("1"));
        assert!(p.missing().is_empty(), "the AVD's own disks are made at boot");

        // The console is quiet unless -show-kernel; logcat's stream is silent unless asked for.
        let p = BootPlan::new(&avd, &Opts::default(), None);
        assert!(p.cmdline.contains(" loglevel=3") && p.cmdline.contains("console=ttyAMA0"), "{}", p.cmdline);
        assert_eq!(p.bootconfig.get("androidboot.logcat"), Some("*:S"));
        let shown = BootPlan::new(&avd, &Opts { show_kernel: true, logcat: Some("*:I".into()), ..Opts::default() }, None);
        assert!(!shown.cmdline.contains("loglevel="), "{}", shown.cmdline);
        assert_eq!(shown.bootconfig.get("androidboot.logcat"), Some("*:I"));
        let to_file = BootPlan::new(&avd, &Opts { logcat_output: Some("/tmp/l".into()), ..Opts::default() }, None);
        assert_eq!(to_file.bootconfig.get("androidboot.logcat"), Some("*:V"));

        let userdata = p.disks.iter().find(|d| d.serial == "userdata").expect("userdata");
        assert_eq!((userdata.slot, userdata.mode), (slots::USERDATA, DiskMode::Persistent));
        std::fs::remove_file(avd.vendor_image()).expect("rm");
        assert_eq!(p.missing(), [avd.vendor_image().as_path()]);

        let text = p.to_string();
        for line in [
            "avd       phone (Android · Phone)",
            "machine   4 vCPUs, 2048 MB RAM, 720x1280 @ 320 dpi",
            "  userdata  slot 29 ",
            "bootconfig\n  androidboot.hardware=ranchu\n",
        ] {
            assert!(text.contains(line), "{line:?} missing from\n{text}");
        }
    }
}
