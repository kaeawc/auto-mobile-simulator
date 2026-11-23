//! Per-AVD runtime state: disk images under `<avd>/emu-rs/`, the instance lock, boot config.
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub struct Instance {
    avd: Avd,
    dir: PathBuf,
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
    /// Android XR images: the guest's XR service link (and, with it, the dual-mode mouse).
    pub xr: Option<std::sync::Arc<emu_vmm::pipe::xr::Xr>>,
    /// The vehicle HAL channel, for Automotive images only (as stock).
    pub car: Option<std::sync::Arc<emu_vmm::pipe::car::Car>>,
    _lock: Option<File>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl Instance {
    pub fn prepare(avd: &Avd, opts: &Opts) -> Result<Instance> {
        let dir = avd.dir.join("emu-rs");
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
        let console_port = match opts.port {
            Some(p) => p,
            None => free_port_pair().context("no free console/adb port pair in 5554..5682")?,
        };
        let gpu_lib64 = renderer(avd, opts);
        if gpu_lib64.is_none() && !gpu_off(opts) {
            eprintln!(
                "emu: no gfxstream renderer in the SDK (emulator/lib64 or emu-rs/renderer); booting without graphics \
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
            // `<sdk>/emu-rs/renderer/<revision>/`; gfxstream finds `lib64/vulkan` and
            // `lib64/gles_*` under it.
            emu_gfxstream::link_beside_exe(lib64);
            emu_gfxstream::configure_environment(emulator_dir, icd);
        }
        let display = gpu_lib64.as_ref().map(|_| std::sync::Arc::new(emu_vmm::display::FrameStore::default()));
        Ok(Instance {
            avd: avd.clone(),
            dir,
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
            _lock: lock,
        })
    }

    pub fn plan(&self, opts: &Opts) -> BootPlan {
        BootPlan::new(&self.avd, opts, self.gpu_lib64.clone())
    }

    pub fn machine_config(&self, opts: &Opts) -> Result<emu_vmm::MachineConfig> {
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
        initrd.extend_from_slice(&plan.bootconfig.encode());
        let log = File::create(self.dir.join("console.log"))?;
        let console: Box<dyn Write + Send> = if opts.show_kernel { Box::new(Tee(std::io::stdout(), log)) } else { Box::new(log) };

        Ok(emu_vmm::MachineConfig {
            cpus: plan.cpus,
            ram_bytes: plan.ram_bytes,
            kernel: plan.kernel,
            initrd: Some(initrd),
            cmdline: plan.cmdline,
            disks: plan.disks,
            disk_fsync: opts.disk_fsync,
            rng: true,
            adb_port: Some(self.console_port + 1),
            gpu: self.gpu_lib64.as_ref().zip(self.display.as_ref()).map(|(lib64, display)| emu_vmm::GpuSpec {
                lib64: lib64.clone(),
                width: self.avd.lcd().0,
                height: self.avd.lcd().1,
                verbose: opts.verbose,
                display: display.clone(),
            }),
            boot_props: plan.boot_props,
            input: Some((self.avd.lcd().0, self.avd.lcd().1)),
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
        })
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
/// First even console port whose console and adb ports are both free, like the stock emulator.
fn free_port_pair() -> Option<u16> {
    let free = |p: u16| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, p)).is_ok();
    (5554..=5682).step_by(2).find(|&p| free(p) && free(p + 1))
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
    /// `qemud:xr_service` and the dual-mode mouse.
    pub xr: bool,
    /// The vehicle HAL channel, `qemud:car`.
    pub car: bool,
    /// Sensor handles the guest's sensors HAL is offered.
    pub sensor_mask: u32,
}

impl BootPlan {
    pub fn new(avd: &Avd, opts: &Opts, renderer: Option<PathBuf>) -> BootPlan {
        let dir = avd.dir.join("emu-rs");
        let user_mode = if opts.read_only { DiskMode::Ephemeral } else { DiskMode::Persistent };
        let disk = |slot, path: PathBuf, mode, serial: &str| DiskSpec { slot, path, mode, serial: serial.to_string() };
        let mut cmdline = String::from(BASE_CMDLINE);
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
        };
        plan.bootconfig = bootconfig(avd, opts, plan.renderer.is_some());
        plan
    }

    /// Image files the boot reads that aren't there. The AVD's own disks under `emu-rs/` are
    /// created at boot, so only the image's are checked.
    pub fn missing(&self) -> Vec<&Path> {
        let image = self.disks.iter().filter(|d| d.mode == DiskMode::Ephemeral && !d.path.starts_with(self.emu_rs_dir()));
        [self.kernel.as_path(), self.ramdisk.as_path()]
            .into_iter()
            .chain(image.map(|d| d.path.as_path()))
            .filter(|p| !p.is_file())
            .collect()
    }

    fn emu_rs_dir(&self) -> PathBuf {
        self.disks.iter().find(|d| d.serial == "userdata").and_then(|d| d.path.parent()).map(Path::to_path_buf).unwrap_or_default()
    }

    /// The virtio devices and host services the board gives the guest.
    pub fn devices(&self) -> Vec<&'static str> {
        let mut out = vec!["virtio-blk x5", "virtio-console (logcat, modem)", "virtio-rng", "virtio-vsock (adb, qemud pipes)"];
        out.push("virtio-input: multi-touch, keyboard (qwerty2), rotary");
        if self.xr {
            out.push("virtio-input: dual-mode mouse");
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
        .set("androidboot.logcat", "*:V")
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
