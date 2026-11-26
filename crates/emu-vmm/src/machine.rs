//! The machine: builds the board, its devices and vCPUs, and runs the guest (Apple silicon only).
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use emu_hvf::{GuestRam, Vm};

use crate::bus::Bus;
use crate::disk::{DiskMode, DiskSpec};
use crate::irq::{GicSpi, IrqLine};
use crate::layout::*;
pub use crate::vcpu::StopReason;
use crate::vcpu::{Cpus, HvVcpus, Wakers};
use crate::virtio::Interrupt;
use crate::virtio::input::Inputs;
use crate::virtio::mmio::MmioTransport;
use crate::wake::Mode as WakeMode;
use crate::{board, boot, devices, disk, pipe, telephony, virtio};

/// Host GPU rendering through gfxstream.
pub struct GpuSpec {
    /// The renderer's `lib64` (holds `libgfxstream_backend.dylib`): the SDK's `emulator/lib64`
    /// or `<sdk>/simee/renderer/<revision>/lib64`.
    pub lib64: PathBuf,
    pub width: u32,
    pub height: u32,
    pub verbose: bool,
    pub display: Arc<dyn crate::display::DisplaySink>,
    /// The guest's GLES is ANGLE on its Vulkan, so the renderer can run without a GL backend
    /// (see `init_renderer`).
    pub vulkan_only: bool,
    /// Offer a graphics queue family with more than one queue (gfxstream's VulkanVirtualQueue),
    /// which HWUI on Vulkan asserts: for ANGLE guests on images that advertise the feature (the
    /// ones emu gives `skiavk`).
    pub virtual_queue: bool,
}

static RENDERER: std::sync::OnceLock<Arc<emu_gfxstream::Renderer>> = std::sync::OnceLock::new();

/// A Vulkan-only renderer's post notices (see `init_renderer`).
static TICKER: std::sync::OnceLock<Arc<crate::display::PostTicker>> = std::sync::OnceLock::new();

/// Post notices of a Vulkan-only renderer ([`crate::display::PostTicker`]): at most one per
/// display frame (60 Hz) while the renderer signals fences, and for 50 ms (three frames) after.
const POST_NOTICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
const POST_NOTICE_TAIL: std::time::Duration = std::time::Duration::from_millis(50);

type RendererInit = std::sync::mpsc::Receiver<Result<Arc<emu_gfxstream::Renderer>>>;

/// An initialization [`start_renderer`] began, not yet claimed by [`run`].
static PENDING: std::sync::Mutex<Option<RendererInit>> = std::sync::Mutex::new(None);

/// Loads and initializes the renderer on its own thread, so the dlopen chain and EGL/GLES/Vulkan
/// setup (hundreds of ms) overlap the rest of the launch; [`run`] waits for it before it creates
/// the GPU device, and an init failure surfaces there as before. Call after the renderer's
/// environment is configured (`emu_gfxstream::configure_environment`). The thread stays parked
/// after init, like the main thread that initialized it before: thread-local EGL state made
/// during init never sees its thread exit.
pub fn start_renderer(spec: GpuSpec) {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if RENDERER.get().is_some() || pending.is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new().name("renderer-init".into()).spawn(move || {
        let t = Instant::now();
        let r = init_renderer(&spec);
        if spec.verbose {
            eprintln!("emu: renderer initialized in {} ms (off the boot path)", t.elapsed().as_millis());
        }
        let _ = tx.send(r);
        loop {
            std::thread::park();
        }
    });
    if spawned.is_ok() {
        *pending = Some(rx);
    }
}

/// The renderer is process-wide: loaded and initialized once, reused across guest reboots.
/// Claims an init [`start_renderer`] began, or initializes it here.
fn renderer(spec: &GpuSpec) -> Result<Arc<emu_gfxstream::Renderer>> {
    if let Some(r) = RENDERER.get() {
        return Ok(r.clone());
    }
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
    match pending {
        Some(rx) => rx.recv().unwrap_or_else(|_| Err(anyhow::anyhow!("the renderer init thread died"))),
        None => init_renderer(spec),
    }
}

/// Waits out an init [`start_renderer`] began that no boot claimed (a launch that fails before
/// it reaches the GPU), so the process never exits while the renderer is inside dlopen or EGL
/// init. The result is dropped: the launch is failing anyway.
fn settle_renderer() {
    let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(rx) = pending {
        let _ = rx.recv();
    }
}

/// The renderer features emu turns on (see [`init_renderer`]), with VulkanVirtualQueue when
/// `virtual_queue` ([`GpuSpec::virtual_queue`]), then `extra` overrides.
fn renderer_features(virtual_queue: bool, extra: Option<&str>) -> String {
    let mut features = String::from("RefCountPipe:enabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled");
    if virtual_queue {
        features.push_str(",VulkanVirtualQueue:enabled");
    }
    if let Some(extra) = extra {
        features.push(',');
        features.push_str(extra);
    }
    features
}

fn init_renderer(spec: &GpuSpec) -> Result<Arc<emu_gfxstream::Renderer>> {
    let r = emu_gfxstream::Renderer::load(&spec.lib64).context("loading the gfxstream renderer")?;
    let gl_composition = std::env::var_os("EMU_GL_COMPOSITION").is_some();
    // With a GL backend, every color buffer has a GL texture beside its Vulkan image, and each
    // guest queue submit copies the image into it on the CPU (a Vulkan readback, then
    // glTexSubImage2D, which Apple's driver retiles on the CPU): two full-frame copies per frame,
    // 16 MB each on a 2560x1600 tablet. Guests on ANGLE never read the GL side, so the renderer
    // runs without GL (gfxstream's GuestVulkanOnly). A Pixel Tablet swiping through the app
    // drawer went from 37% janky frames (p90 53 ms) to none (p90 5 ms), and emu from 163% to 62%
    // of a core; see docs/benchmarks.md. Host frames are then read only through the direct
    // Vulkan readback, so a renderer build without it keeps GL. EMU_RENDERER_GL=1 keeps GL anyway.
    let vulkan_only = spec.vulkan_only && !gl_composition && std::env::var_os("EMU_RENDERER_GL").is_none() && r.can_read_vulkan_directly();
    let mut flags = emu_gfxstream::flags::USE_EGL | emu_gfxstream::flags::USE_VK;
    if !vulkan_only {
        flags |= emu_gfxstream::flags::USE_GLES;
    }
    if !gl_composition {
        flags |= emu_gfxstream::flags::VULKAN_NATIVE_SWAPCHAIN;
    }
    // What the ranchu gralloc expects of the host:
    // - color buffer lifetimes follow its `refcount` pipes (RefCountPipe); otherwise the
    //   allocator's own release frees buffers SurfaceFlinger and apps are still using;
    // - CPU reads of color buffers (screencap, software readers) by DMA into shared-slot host
    //   memory (`ANDROID_EMU_read_color_buffer_dma` = GlDirectMem + shared slots), resolved
    //   through the DMA translator `run` installs.
    // - for images that advertise it, a graphics queue family with more than one queue
    //   (VulkanVirtualQueue): HWUI on Vulkan (`skiavk`, what those guests draw their UI with on
    //   ANGLE) asserts it and aborts without.
    // EMU_RENDERER_FEATURES adds overrides, e.g. "HostComposition:disabled".
    let features = renderer_features(spec.virtual_queue, std::env::var("EMU_RENDERER_FEATURES").ok().as_deref());
    // Without GL there's no post callback: fences drive the post notices instead.
    let ticker = vulkan_only
        .then(|| crate::display::PostTicker::start(spec.display.clone(), spec.width, spec.height, POST_NOTICE_INTERVAL, POST_NOTICE_TAIL));
    let fences = virtio::gpu::fence_handler();
    if let Some(t) = &ticker {
        let _ = TICKER.set(t.clone());
    }
    let fences: Arc<dyn Fn(emu_gfxstream::Fence) + Send + Sync> = match ticker {
        Some(t) => Arc::new(move |f| {
            fences(f);
            t.poke();
        }),
        None => fences,
    };
    r.init_with_features(spec.width, spec.height, flags, Some(&features), spec.verbose, fences)
        .context("initializing the gfxstream renderer")?;
    // Host composition posts frames to the renderer's display. A post is only a notice: the
    // callback's pixels come from the renderer's GL readback, which is stale under Vulkan
    // composition (and unfilled with async readback), so they're copied only when the renderer
    // can't be read directly. Otherwise the post records the frame's size, and its pixels are
    // read back from the renderer when a stream or screenshot wants them, once per frame.
    let display = spec.display.clone();
    let direct = r.can_read_last_posted();
    if direct {
        display.set_readback(Box::new(|w, h, buf| RENDERER.get().is_some_and(|r| r.read_last_posted_to(w, h, buf).is_ok())));
    }
    if vulkan_only {
        return Ok(RENDERER.get_or_init(|| Arc::new(r)).clone());
    }
    let primary = std::sync::atomic::AtomicU32::new(u32::MAX);
    let posted = r.set_post_callback(Box::new(move |f| {
        // The first display that posts is the primary one.
        let _ = primary.compare_exchange(u32::MAX, f.display, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed);
        if f.display != primary.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if direct {
            display.posted(f.width, f.height);
            return;
        }
        // 0x80E1 = GL_BGRA → virtio-gpu B8G8R8A8 (1); GL_RGBA → R8G8B8A8 (67).
        let format = if f.gl_format == 0x80e1 { 1 } else { 67 };
        if f.top_down {
            display.present(f.width, f.height, f.width * 4, format, f.pixels);
        } else {
            display.present_bottom_up(f.width, f.height, format, f.pixels);
        }
    }));
    if let Err(e) = posted {
        eprintln!("emu: no host frame readback ({e}); screenshots come from the guest only");
    }
    Ok(RENDERER.get_or_init(|| Arc::new(r)).clone())
}

pub struct MachineConfig {
    pub cpus: u32,
    pub ram_bytes: u64,
    pub kernel: PathBuf,
    /// Where an inflated gzip kernel is cached between launches (none: inflate every launch).
    pub kernel_cache: Option<PathBuf>,
    /// Initrd bytes, bootconfig already appended.
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    pub disks: Vec<DiskSpec>,
    /// fsync persistent disks when the guest flushes (`-disk-cache fsync`); default off.
    pub disk_fsync: bool,
    pub rng: bool,
    /// The host TCP port bridged to the guest's adbd (stock: console port + 1), bound by the
    /// caller once and kept across guest resets.
    pub adb: Option<Arc<pipe::adb::AdbListener>>,
    pub gpu: Option<GpuSpec>,
    /// Served to the guest's `qemu-props` over the `qemud:boot-properties` pipe.
    pub boot_props: Vec<(String, String)>,
    /// Guest notifications over `QemuMiscPipe` (`bootcomplete`, `heartbeat`).
    pub on_guest_event: Option<pipe::misc::GuestEvent>,
    /// Live sensor values (served over `qemud:sensors`, changed by the console).
    pub sensors: Arc<pipe::sensors::Sensors>,
    /// Shared clipboard (served over the `clipboard` pipe).
    pub clipboard: Arc<pipe::clipboard::Clipboard>,
    /// Fingerprint sensor events (console `finger touch|remove`).
    pub fingerprint: Arc<pipe::fingerprint::Fingerprint>,
    /// GNSS position (console `geo fix`), streamed over `qemud:gps`.
    pub gps: Arc<pipe::gps::Gps>,
    /// Battery and AC state (console `power`).
    pub battery: Arc<devices::goldfish_battery::Battery>,
    /// The modem on the `modem` vport (console `sms`, `gsm`).
    pub telephony: Arc<telephony::Telephony>,
    /// The vehicle HAL channel (`qemud:car`, console `car`). Like stock, only for car images;
    /// other guests asking for the pipe are refused.
    pub car: Option<Arc<pipe::car::Car>>,
    /// The virtio-snd card (ALSA card 0) and what it may do on the host.
    pub sound: Option<crate::audio::Options>,
    /// Touchscreen size; enables the virtio-input touchscreen, keyboard and rotary devices.
    pub input: Option<(u32, u32)>,
    /// With `input`: a multi-touch screen (false for `hw.screen=no-touch`, TV's).
    pub touch_screen: bool,
    /// With `input`: a touchpad of this many units (`hw.touchpad0`).
    pub touchpad: Option<(u32, u32)>,
    /// With `input`: the dual-mode mouse (Android XR headsets' `VirtioDualModeMouse`).
    pub dual_mode_mouse: bool,
    /// Android XR: the `qemud:xr_service` link (console `xr`).
    pub xr: Option<Arc<pipe::xr::Xr>>,
    /// Receives the input handles of this run (they change when the guest reboots).
    pub on_input: Option<Arc<dyn Fn(Inputs) + Send + Sync>>,
    /// Where guest console (PL011) output goes.
    pub console: Box<dyn Write + Send>,
    /// Where the guest's `goldfish-logcat` stream (`/dev/hvc1`) goes; None discards it, as
    /// stock's null chardev does without `-logcat`. A spool, so a slow destination drops output
    /// rather than stalling the console worker (which also serves `modem`).
    pub logcat: Option<virtio::console::Spool>,
    /// When the process started; set, `run` prints launch timings (kernel load, renderer wait,
    /// first vCPU start) on stderr (`-verbose`).
    pub launched: Option<Instant>,
}

pub struct Outcome {
    pub reason: StopReason,
    pub uptime: Duration,
    /// Per-vCPU exit counters: (name, count).
    pub exits: Vec<Vec<(&'static str, u64)>>,
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b
}

/// The running machine's vCPUs, so [`request_stop`] reaches its stop loop directly instead of
/// the loop polling the flag. One machine per process (Hypervisor.framework allows one VM).
static RUNNING: Mutex<Option<Weak<Cpus>>> = Mutex::new(None);

/// Asks the machine [`run`] is running (or the next one it starts) to stop: sets `stop`, the flag
/// passed to `run`, and wakes the running machine's stop loop. Callable from any thread,
/// including a signal-watching one.
pub fn request_stop(stop: &AtomicBool) {
    stop.store(true, Ordering::SeqCst);
    // The lock is held throughout: the upgraded reference must not outlive the machine's own, or
    // this thread could end up dropping its devices after the VM and RAM. (`Cpus::request_stop`
    // never takes `RUNNING`.)
    let running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(cpus) = running.as_ref().and_then(Weak::upgrade) {
        cpus.request_stop(StopReason::Requested);
    }
}

/// How often the stop loop wakes on its own to look at the flag: a stop set without
/// [`request_stop`] costs at most this long, not a hang.
const STOP_BACKSTOP: Duration = Duration::from_secs(1);

/// Blocks until `cpus` stop: they power off, reset or fault, or a stop is requested.
/// [`request_stop`] wakes it at once; a bare `stop.store(true)` is seen within
/// [`STOP_BACKSTOP`]. Returns the reason and how many times the thread woke (for tests).
fn wait_for_stop(cpus: &Arc<Cpus>, stop: &AtomicBool) -> (StopReason, u32) {
    *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(cpus));
    // A request made before the machine was published set the flag without reaching it. (The
    // mutex orders the two: either this sees the flag or `request_stop` saw the machine.)
    if stop.load(Ordering::SeqCst) {
        cpus.request_stop(StopReason::Requested);
    }
    let mut wakes = 0;
    let reason = loop {
        wakes += 1;
        if let Some(r) = cpus.wait_stop(STOP_BACKSTOP) {
            break r;
        }
        if stop.load(Ordering::SeqCst) {
            cpus.request_stop(StopReason::Requested);
        }
    };
    *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = None;
    (reason, wakes)
}

/// Boots the machine and blocks until it powers off, resets, faults, or a stop is requested.
/// Stop it with [`request_stop`]`(&stop)`; setting `stop` alone takes up to [`STOP_BACKSTOP`].
/// On an error, an early renderer init ([`start_renderer`]) has finished before this returns.
pub fn run(cfg: MachineConfig, stop: Arc<AtomicBool>) -> Result<Outcome> {
    settled(boot_and_run(cfg, stop))
}

/// `result`, once no renderer init is left mid-flight when it is an error.
fn settled<T>(result: Result<T>) -> Result<T> {
    if result.is_err() {
        settle_renderer();
    }
    result
}

fn boot_and_run(cfg: MachineConfig, stop: Arc<AtomicBool>) -> Result<Outcome> {
    let started = Instant::now();
    if cfg.cpus == 0 || cfg.cpus > 16 {
        bail!("cpus must be in 1..=16 (got {})", cfg.cpus);
    }
    let vm = Vm::new().context("creating the VM (is the binary signed with com.apple.security.hypervisor?)")?;
    let mem = Arc::new(GuestRam::new(RAM_BASE, cfg.ram_bytes)?);
    vm.map_ram(&mem)?;
    let gic = vm.create_gic(GIC_DIST_BASE, GIC_REDIST_BASE)?;
    let redist_size = u64::from(cfg.cpus) * 0x2_0000;
    if gic.redist_base + redist_size > GIC_REDIST_LIMIT || redist_size > gic.redist_region_size {
        bail!("GIC redistributors for {} vCPUs don't fit the memory map", cfg.cpus);
    }
    let max_spi = VIRTIO_MMIO_SPI_BASE + VIRTIO_MMIO_SLOTS;
    if spi_intid(0) < gic.spi_base || spi_intid(max_spi) >= gic.spi_base + gic.spi_count {
        bail!("GIC SPI range {}+{} doesn't cover the board's interrupts", gic.spi_base, gic.spi_count);
    }

    // Kernel, initrd, device tree.
    let t = Instant::now();
    let kernel = boot::load_kernel(&cfg.kernel, &mem, KERNEL_ADDR, INITRD_ADDR, cfg.kernel_cache.as_deref())?;
    let entry = kernel.entry;
    if cfg.launched.is_some() {
        eprintln!("emu: kernel {:?}, {} MiB in {} ms", kernel.source, kernel.len >> 20, t.elapsed().as_millis());
    }
    let initrd = match &cfg.initrd {
        Some(bytes) => {
            if INITRD_ADDR + bytes.len() as u64 > mem.end() {
                bail!("initrd doesn't fit in guest RAM");
            }
            mem.write(INITRD_ADDR, bytes)?;
            Some((INITRD_ADDR, INITRD_ADDR + bytes.len() as u64))
        }
        None => None,
    };
    let dtb = board::build(&board::BoardInfo {
        cpus: cfg.cpus,
        ram_bytes: cfg.ram_bytes,
        gic_dist: (gic.dist_base, gic.dist_size),
        gic_redist: (gic.redist_base, redist_size),
        cmdline: &cfg.cmdline,
        initrd,
        virtio_slots: VIRTIO_MMIO_SLOTS,
        pci: cfg.gpu.is_some(),
        goldfish_pipe: cfg.gpu.is_some(),
        rng_seed: random_bytes(),
        kaslr_seed: u64::from_le_bytes(random_bytes()),
    })?;
    if dtb.len() as u64 > DTB_MAX {
        bail!("device tree too large");
    }
    mem.write(DTB_ADDR, &dtb)?;

    // Devices.
    let wakers = Arc::new(Wakers::new(cfg.cpus as usize, HvVcpus::new(cfg.cpus as usize), WakeMode::from_env()));
    let wake: Arc<dyn Fn(u32) + Send + Sync> = {
        let w = wakers.clone();
        Arc::new(move |intid| w.wake_spi(intid))
    };
    let spi = |n: u32| -> Arc<dyn IrqLine> { Arc::new(GicSpi::new(spi_intid(n), wake.clone())) };

    let mut bus = Bus::new();
    // Goldfish address space (the ranchu gralloc's host memory) on a PCIe host, with graphics.
    let mut as_area = None;
    if cfg.gpu.is_some() {
        let area = Arc::new(GuestRam::new(GOLDFISH_AS_AREA, GOLDFISH_AS_AREA_SIZE)?);
        vm.map_ram(&area)?;
        use devices::pci::{Bar, BarKind, PciFunction};
        let host = devices::pci::PciHost::new(vec![PciFunction {
            vendor: devices::goldfish_as::VENDOR,
            device: devices::goldfish_as::DEVICE,
            class: 0x05_00_00,
            bars: vec![
                (0, Bar { kind: BarKind::Mem32, addr: GOLDFISH_AS_REGS, size: 0x1000 }),
                (1, Bar { kind: BarKind::Mem64Prefetch, addr: GOLDFISH_AS_AREA, size: GOLDFISH_AS_AREA_SIZE }),
            ],
        }]);
        bus.insert(PCI_ECAM_BASE, PCI_ECAM_SIZE, host);
        bus.insert(GOLDFISH_AS_REGS, 0x1000, Arc::new(devices::goldfish_as::GoldfishAddressSpace::new(mem.clone(), area.clone())));
        as_area = Some(area);
    }
    bus.insert(UART_BASE, PERIPH_SIZE, Arc::new(devices::pl011::Pl011::new(spi(UART_SPI), cfg.console)));
    bus.insert(RTC_BASE, PERIPH_SIZE, Arc::new(devices::pl031::Pl031::default()));
    bus.insert(
        GOLDFISH_BATTERY_BASE,
        PERIPH_SIZE,
        Arc::new(devices::goldfish_battery::GoldfishBattery::new(cfg.battery.clone(), spi(GOLDFISH_BATTERY_SPI))),
    );
    let mut slots: Vec<Option<Box<dyn virtio::VirtioDevice>>> = (0..VIRTIO_MMIO_SLOTS).map(|_| None).collect();
    for d in &cfg.disks {
        let disk = match d.mode {
            DiskMode::Persistent => disk::Disk::persistent(&d.path).map(|mut disk| {
                disk.set_fsync(cfg.disk_fsync);
                disk
            }),
            DiskMode::Ephemeral => disk::Disk::ephemeral(&d.path),
        }
        .with_context(|| format!("opening {}", d.path.display()))?;
        let slot = slots.get_mut(d.slot as usize).with_context(|| format!("virtio slot {} out of range", d.slot))?;
        *slot = Some(Box::new(virtio::blk::Blk::new(disk, d.serial.clone())));
    }
    {
        // Port 0 is the console port (hvc0); named ports follow (qemu-props publishes them as
        // vendor.qemu.vport.<name>); the unnamed port 2 becomes hvc1, logcat's (init.ranchu.rc's
        // goldfish-logcat runs `logcat -f /dev/hvc1` and restarted every ~5 s without it).
        use virtio::console::{Console, PortBackend, PortSpec, Sink, Writer};
        let logcat: Box<dyn PortBackend> = match cfg.logcat {
            Some(w) => Box::new(Writer(w)),
            None => Box::new(Sink),
        };
        let (vcon, handles) = Console::new(vec![
            PortSpec { name: String::new(), backend: Box::new(Sink) },
            PortSpec { name: "modem".into(), backend: cfg.telephony.backend() },
            PortSpec { name: String::new(), backend: logcat },
            // A named console port, so the guest's third console (hvc2), where the UWB HAL looks.
            PortSpec { name: "uwb".into(), backend: Box::new(crate::bt::UwbPort::default()) },
            // Stable per AVD: its disks' paths.
            PortSpec {
                name: "bluetooth".into(),
                backend: Box::new(crate::bt::BluetoothPort::new(crate::bt::address_for(
                    cfg.disks.iter().map(|d| d.path.as_os_str().as_encoded_bytes()).collect::<Vec<_>>().concat().as_slice(),
                ))),
            },
        ]);
        if let Some(h) = handles.get(1) {
            cfg.telephony.attach(h.clone());
        }
        slots[slots::VPORTS as usize] = Some(Box::new(vcon));
    }
    if cfg.rng {
        slots[slots::RNG as usize] = Some(Box::new(virtio::rng::Rng::new()));
    }
    // eth0, which the radio HAL configures from the modem's data-call reply, behind the
    // user-mode NAT (10.0.2.2 the host's loopback, 10.0.2.3 its resolver). `EMU_NET=link`: the
    // link alone, to measure what the NAT costs (scripts/net-bench.sh).
    let net: Box<dyn virtio::net::Backend> = if std::env::var("EMU_NET").is_ok_and(|v| v == "link") {
        Box::new(virtio::net::LinkOnly)
    } else {
        match crate::net::Nat::new(crate::net::Config::default()) {
            Ok(n) => Box::new(n),
            Err(e) => {
                eprintln!("emu: net: no NAT ({e}); the network is link only");
                Box::new(virtio::net::LinkOnly)
            }
        }
    };
    slots[slots::NET as usize] = Some(Box::new(virtio::net::Net::new(net)));
    if let Some(opts) = cfg.sound {
        slots[slots::SOUND as usize] = Some(Box::new(virtio::snd::Snd::new(opts)));
    }
    let mut dma_renderer = None;
    let mut gpu_renderer = None;
    if let Some(spec) = &cfg.gpu {
        let t = Instant::now();
        let r = renderer(spec)?;
        if cfg.launched.is_some() {
            eprintln!("emu: renderer ready (boot waited {} ms)", t.elapsed().as_millis());
        }
        gpu_renderer = Some(r.clone());
        // Guest-physical → host for the renderer's DMA paths (gralloc CPU reads land in the
        // goldfish address-space area; direct-mapped buffers may be anywhere in RAM).
        let (ram, area) = (mem.clone(), as_area.clone());
        let translate: emu_gfxstream::DmaTranslator = Box::new(move |gpa| {
            ram.host_range(gpa, 1).or_else(|| area.as_ref().and_then(|a| a.host_range(gpa, 1))).map_or(std::ptr::null_mut(), |h| h.as_ptr())
        });
        match r.set_dma_translator(Some(translate)) {
            Ok(()) => dma_renderer = Some(r.clone()),
            Err(e) => eprintln!("emu: renderer DMA hooks unavailable ({e}); guest CPU reads of color buffers will fail"),
        }
        let gpu = virtio::gpu::Gpu::new(virtio::gpu::GpuConfig {
            width: spec.width,
            height: spec.height,
            renderer: r,
            shm: (GPU_SHM_BASE, GPU_SHM_SIZE),
            display: spec.display.clone(),
            verbose: spec.verbose,
        });
        slots[slots::GPU as usize] = Some(Box::new(gpu));
    }
    if let Some((w, h)) = cfg.input {
        use virtio::input::{Input, Kind};
        let th = cfg.touch_screen.then(|| {
            let (touch, th) = Input::new(Kind::Touch { width: w, height: h }, "virtio_input_multi_touch_1");
            slots[slots::TOUCH as usize] = Some(Box::new(touch));
            th
        });
        let (kbd, kh) = Input::new(Kind::Keyboard, virtio::input::KEYBOARD_NAME);
        let (rot, rh) = Input::new(Kind::Rotary, "virtio_input_rotary");
        slots[slots::KEYBOARD as usize] = Some(Box::new(kbd));
        slots[slots::ROTARY as usize] = Some(Box::new(rot));
        let mouse = cfg.dual_mode_mouse.then(|| {
            let (mouse, mh) = Input::new(Kind::DualModeMouse, virtio::input::DUAL_MODE_MOUSE_NAME);
            slots[slots::MOUSE as usize] = Some(Box::new(mouse));
            mh
        });
        let touchpad = cfg.touchpad.map(|(tw, th)| {
            let (pad, ph) = Input::new(Kind::Touch { width: tw, height: th }, virtio::input::TOUCHPAD_NAME);
            slots[slots::TOUCHPAD as usize] = Some(Box::new(pad));
            ph
        });
        if let Some(cb) = &cfg.on_input {
            cb(Inputs { screen: (w, h), touch: th, keyboard: kh, rotary: rh, mouse, touchpad });
        }
    }
    if let Some(adb) = &cfg.adb {
        let (events, rx) = virtio::vsock::event_channel();
        let (svc, port) = (adb.attach(events.clone()), adb.port);
        let mut registry = pipe::PipeRegistry::default();
        registry.register("qemud:adb", Box::new(svc));
        registry.register("qemud:boot-properties", Box::new(pipe::qemud::BootProperties { props: cfg.boot_props.clone() }));
        let on_last_ref = gpu_renderer.clone().map(|r| -> devices::goldfish_pipe::OnLastRef {
            Arc::new(move |handle| {
                let _ = r.release_color_buffer(handle);
            })
        });
        registry.register("refcount", Box::new(devices::goldfish_pipe::RefCount { on_last_ref }));
        registry.register("QemuMiscPipe", Box::new(pipe::misc::MiscService { on_event: cfg.on_guest_event.clone() }));
        registry.register("qemud:sensors", Box::new(pipe::sensors::SensorService { sensors: cfg.sensors.clone() }));
        registry.register("clipboard", Box::new(pipe::clipboard::ClipboardService { clipboard: cfg.clipboard.clone() }));
        registry
            .register("qemud:fingerprintlisten", Box::new(pipe::fingerprint::FingerprintService { fingerprint: cfg.fingerprint.clone() }));
        registry.register("qemud:gps", Box::new(pipe::gps::GpsService { gps: cfg.gps.clone() }));
        if let Some(xr) = &cfg.xr {
            registry.register("qemud:xr_service", Box::new(pipe::xr::XrService { xr: xr.clone() }));
        }
        if let Some(car) = &cfg.car {
            registry.register("qemud:car", Box::new(pipe::car::CarService { car: car.clone() }));
        }
        registry.register("multidisplay", Box::new(pipe::misc::Silent("multidisplay")));
        registry.register("qemud:camera", Box::new(pipe::fingerprint::CameraService));
        registry.register("GLProcessPipe", Box::new(devices::goldfish_pipe::GlProcess::default()));
        let registry = Arc::new(registry);
        // Graphics pipes never use vsock; they need the goldfish pipe device.
        if cfg.gpu.is_some() {
            let gp = devices::goldfish_pipe::GoldfishPipe::new(mem.clone(), registry.clone(), spi(GOLDFISH_PIPE_SPI));
            bus.insert(GOLDFISH_PIPE_BASE, GOLDFISH_PIPE_SIZE, gp);
        }
        slots[slots::VSOCK as usize] = Some(Box::new(virtio::vsock::Vsock::new(3, registry, events, rx)));
        pipe::adb::AdbService::announce_port(port);
    }
    for (i, dev) in slots.into_iter().enumerate() {
        let n = i as u32;
        let irq = Arc::new(Interrupt::new(spi(VIRTIO_MMIO_SPI_BASE + n)));
        bus.insert(virtio_slot_addr(n), VIRTIO_MMIO_STRIDE, Arc::new(MmioTransport::new(dev, irq, mem.clone())));
    }

    // Guest counter starts near zero, identical on every vCPU.
    let cpus = Arc::new(Cpus::new(bus, wakers, emu_hvf::host_ticks()));
    let mut threads = Vec::new();
    for idx in 0..cpus.count() {
        let c = cpus.clone();
        let boot = (idx == 0).then_some((entry, DTB_ADDR));
        threads.push(std::thread::Builder::new().name(format!("vcpu{idx}")).spawn(move || c.thread_main(idx, boot))?);
    }
    if let Some(launched) = cfg.launched {
        eprintln!(
            "emu: vCPUs started {} ms after launch ({} ms in machine setup)",
            launched.elapsed().as_millis(),
            started.elapsed().as_millis()
        );
    }

    let (reason, _) = wait_for_stop(&cpus, &stop);
    cpus.request_stop(reason.clone());
    for t in threads {
        let _ = t.join();
    }
    let exits = cpus.stats.iter().map(|s| s.snapshot().to_vec()).collect();
    // Devices (and their worker threads) go first, then the VM, then guest RAM.
    drop(cpus);
    if let Some(r) = dma_renderer {
        let _ = r.set_dma_translator(None);
    }
    drop(vm);
    drop(as_area);
    drop(mem);
    if let (StopReason::Reset, Some(spec)) = (&reason, &cfg.gpu) {
        blank_until_next_post(&*spec.display, spec.verbose);
    }
    Ok(Outcome { reason, uptime: started.elapsed(), exits })
}

/// After a guest reset, the screen is black until the next boot posts, not the last boot's final
/// frame. A Vulkan-only renderer's post notices are held until then too: they come with fences,
/// which the next boot signals well before its first post, and would have readers read back the
/// last boot's final post, which the reset freed. The next boot has posted once the last post is
/// another color buffer, or the same handle on a buffer it created (handles can be resource ids,
/// which every boot allocates alike); in the second case a reader may see that buffer before its
/// first post.
fn blank_until_next_post(display: &dyn crate::display::DisplaySink, verbose: bool) {
    let hold = next_post_hold(verbose);
    hold_then_blank(display, hold.as_ref().map(|(t, h)| (&**t, h.clone())));
}

/// The Vulkan-only renderer's ticker and a hold met once the next boot posts (none for another
/// renderer).
fn next_post_hold(verbose: bool) -> Option<(Arc<crate::display::PostTicker>, crate::display::Hold)> {
    let (ticker, r) = (TICKER.get()?, RENDERER.get()?);
    let stale = r.last_posted_color_buffer().ok()?;
    virtio::gpu::watch_for_resource(stale);
    let r = r.clone();
    let hold: crate::display::Hold = Arc::new(move || {
        let Ok(cb) = r.last_posted_color_buffer() else { return false };
        let posted = cb != 0 && (cb != stale || virtio::gpu::resource_created(stale));
        if posted && verbose {
            eprintln!("emu: host frames resume: color buffer {cb} posted (the last boot's last was {stale})");
        }
        posted
    });
    Some((ticker.clone(), hold))
}

/// Holds the ticker's notices, then blanks: in the other order a notice between the two would
/// have readers read the last boot's post back over the black frame.
fn hold_then_blank(display: &dyn crate::display::DisplaySink, hold: Option<(&crate::display::PostTicker, crate::display::Hold)>) {
    if let Some((ticker, until)) = hold {
        ticker.hold_until(until);
    }
    display.blank();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(lib64: PathBuf) -> GpuSpec {
        GpuSpec {
            lib64,
            width: 720,
            height: 1280,
            verbose: true,
            display: Arc::new(crate::display::FrameStore::default()),
            vulkan_only: false,
            virtual_queue: false,
        }
    }

    fn idle_cpus() -> Arc<Cpus> {
        let wakers = Arc::new(Wakers::new(1, HvVcpus::new(1), WakeMode::from_env()));
        Arc::new(Cpus::new(Bus::new(), wakers, 0))
    }

    /// A display that notes, when it is blanked, whether the ticker was already holding.
    #[derive(Default)]
    struct Blanks {
        ticker: std::sync::OnceLock<Arc<crate::display::PostTicker>>,
        held_at_blank: Mutex<Vec<bool>>,
    }

    impl crate::display::DisplaySink for Blanks {
        fn present(&self, _: u32, _: u32, _: u32, _: u32, _: &[u8]) {}
        fn blank(&self) {
            let held = self.ticker.get().is_some_and(|t| t.holding());
            self.held_at_blank.lock().unwrap_or_else(PoisonError::into_inner).push(held);
        }
    }

    #[test]
    fn a_reset_holds_post_notices_before_it_blanks() {
        let display = Arc::new(Blanks::default());
        let ticker = crate::display::PostTicker::start(display.clone(), 720, 1280, Duration::from_millis(1), Duration::from_millis(20));
        let _ = display.ticker.set(ticker.clone());
        hold_then_blank(&*display, Some((&ticker, Arc::new(|| false))));
        assert_eq!(*display.held_at_blank.lock().unwrap_or_else(PoisonError::into_inner), [true]);
        ticker.stop();
        // Without a Vulkan-only renderer there is nothing to hold; the screen still goes black.
        let other = Blanks::default();
        hold_then_blank(&other, None);
        assert_eq!(*other.held_at_blank.lock().unwrap_or_else(PoisonError::into_inner), [false]);
    }

    #[test]
    fn the_renderer_offers_the_queues_hwui_on_vulkan_needs_only_to_images_that_take_them() {
        let has = |f: &str, want: &str| f.split(',').any(|x| x == want);
        let f = renderer_features(true, None);
        for want in
            ["RefCountPipe:enabled", "GlDirectMem:enabled", "HasSharedSlotsHostMemoryAllocator:enabled", "VulkanVirtualQueue:enabled"]
        {
            assert!(has(&f, want), "{want} missing from {f}");
        }
        // An image that doesn't advertise it, or a guest on the GLES encoder: one queue.
        let one = renderer_features(false, None);
        assert!(has(&one, "RefCountPipe:enabled") && !one.contains("VulkanVirtualQueue"), "{one}");
        // Overrides come last, so they win.
        assert!(renderer_features(true, Some("VulkanVirtualQueue:disabled")).ends_with(",VulkanVirtualQueue:disabled"));
    }

    /// Serializes the tests that use the process-wide `RUNNING` slot.
    static RUNNING_SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_stop_request_wakes_the_idle_stop_loop() {
        let _slot = RUNNING_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let cpus = idle_cpus();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn({
            let (cpus, stop) = (cpus.clone(), stop.clone());
            move || {
                let r = wait_for_stop(&cpus, &stop);
                tx.send(Instant::now()).expect("send");
                r
            }
        });
        // Idle for 300 ms: the old loop woke three times here; this one sleeps (its backstop is 1 s).
        std::thread::sleep(Duration::from_millis(300));
        assert!(rx.try_recv().is_err(), "still waiting");
        let asked = Instant::now();
        request_stop(&stop);
        let stopped = rx.recv_timeout(Duration::from_secs(5)).expect("stopped");
        let (reason, wakes) = waiter.join().expect("waiter");
        assert!(matches!(reason, StopReason::Requested), "{reason:?}");
        assert!(stopped.duration_since(asked) < Duration::from_millis(100), "took {:?}", stopped.duration_since(asked));
        assert_eq!(wakes, 1, "slept through the idle period");
        assert!(RUNNING.lock().expect("running").is_none(), "unpublished after stopping");
    }

    #[test]
    fn a_bare_flag_still_stops_within_the_backstop() {
        let _slot = RUNNING_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let cpus = idle_cpus();
        let stop = Arc::new(AtomicBool::new(false));
        let setter = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
            Instant::now()
        });
        let (reason, wakes) = wait_for_stop(&cpus, &stop);
        let took = t.join().expect("setter").elapsed();
        assert!(matches!(reason, StopReason::Requested), "{reason:?}");
        assert!(took <= STOP_BACKSTOP + Duration::from_millis(200), "took {took:?}");
        assert_eq!(wakes, 2, "one backstop wake");
    }

    #[test]
    fn stops_requested_before_the_loop_or_from_the_guest_still_end_it() {
        let _slot = RUNNING_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        // Requested before the machine was published (between resets, say): seen at the start.
        let stop = AtomicBool::new(false);
        request_stop(&stop);
        let (reason, _) = wait_for_stop(&idle_cpus(), &stop);
        assert!(matches!(reason, StopReason::Requested), "{reason:?}");
        // The guest's own reset or power-off, with no stop requested: that reason, unchanged.
        let cpus = idle_cpus();
        let guest = cpus.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            guest.request_stop(StopReason::Reset);
        });
        let (reason, _) = wait_for_stop(&cpus, &AtomicBool::new(false));
        t.join().expect("guest");
        assert!(matches!(reason, StopReason::Reset), "{reason:?}");
        // A request with no machine running only sets the flag.
        let idle = AtomicBool::new(false);
        request_stop(&idle);
        assert!(idle.load(Ordering::SeqCst));
    }

    /// Serializes the tests that use the process-wide `PENDING` slot.
    static SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A launch that fails before it claims the renderer returns only after the init thread is
    /// done: exiting mid-dlopen or mid-EGL-init could crash or hang the process.
    #[test]
    fn a_failed_launch_waits_out_the_pending_renderer_init() {
        let _slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        *PENDING.lock().expect("lock") = Some(rx);
        let init = std::thread::spawn({
            let done = done.clone();
            move || {
                // A slow init, still running when the launch fails.
                std::thread::sleep(Duration::from_millis(200));
                done.store(true, Ordering::SeqCst);
                let _ = tx.send(Err(anyhow::anyhow!("never claimed")));
            }
        });
        let err = settled::<()>(Err(anyhow::anyhow!("missing kernel-ranchu"))).expect_err("still the launch's error");
        assert_eq!(err.to_string(), "missing kernel-ranchu");
        assert!(done.load(Ordering::SeqCst), "returned while the renderer init was still running");
        assert!(PENDING.lock().expect("lock").is_none());
        init.join().expect("init thread");
        // A successful run leaves nothing to wait for and returns at once.
        assert_eq!(settled(Ok(7)).expect("ok"), 7);
    }

    /// An init started early that fails reports its error where the boot claims it.
    #[test]
    fn early_renderer_failure_surfaces_at_claim() {
        let _slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let bad = spec(std::env::temp_dir().join(format!("emu-no-renderer-{}", std::process::id())));
        start_renderer(spec(bad.lib64.clone()));
        let err = renderer(&bad).err().expect("no renderer there");
        assert!(format!("{err:#}").contains("loading the gfxstream renderer"), "{err:#}");
        // Nothing left pending or cached: a later claim tries again.
        assert!(PENDING.lock().expect("lock").is_none() && RENDERER.get().is_none());
        assert!(renderer(&bad).is_err());
    }

    /// The renderer initialized on `start_renderer`'s thread serves the boot's threads, as one
    /// initialized on the main thread did. Needs a renderer (`EMU_RENDERER_LIB64`, else the
    /// SDK's `emulator/lib64`); no guest. The test binary lacks `emu`'s rpaths, so gfxstream's
    /// bare-name dlopen of ANGLE needs a fallback path (without it, init crashes in the renderer):
    ///     L=~/Library/Android/sdk/emulator/lib64; DYLD_FALLBACK_LIBRARY_PATH=$L:$L/gles_angle:/usr/lib \
    ///       cargo test --release -p emu-vmm renderer_init_off_thread -- --ignored --nocapture
    #[test]
    #[ignore = "needs a gfxstream renderer and a host GPU"]
    fn renderer_init_off_thread() {
        let _slot = SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let lib64 = std::env::var_os("EMU_RENDERER_LIB64")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join("Library/Android/sdk/emulator/lib64"));
        emu_gfxstream::link_beside_exe(&lib64);
        emu_gfxstream::configure_environment(lib64.parent().expect("launcher dir"), emu_gfxstream::Icd::MoltenVk);
        let t = Instant::now();
        start_renderer(spec(lib64.clone()));
        let started = t.elapsed();
        // What `run` does meanwhile (VM, kernel, devices) happens here; then it claims.
        let r = renderer(&spec(lib64.clone())).expect("renderer");
        println!(
            "start_renderer returned in {:.2} ms; renderer ready after {:.0} ms",
            started.as_secs_f64() * 1e3,
            t.elapsed().as_secs_f64() * 1e3
        );
        // Used from threads other than the one that initialized it.
        let caps = std::thread::spawn({
            let r = r.clone();
            move || emu_gfxstream::capset::ALL.iter().filter(|&&id| r.capset_info(id).is_some()).count()
        })
        .join()
        .expect("caps thread");
        assert!(caps > 0, "no capability sets");
        let ram = Arc::new(GuestRam::new(RAM_BASE, 16 << 20).expect("ram"));
        let translate: emu_gfxstream::DmaTranslator =
            Box::new(move |gpa| ram.host_range(gpa, 1).map_or(std::ptr::null_mut(), |h| h.as_ptr()));
        assert!(r.set_dma_translator(Some(translate)).is_ok());
        let _ = r.set_dma_translator(None);
        println!("vulkan ok: {}, capsets: {caps}", emu_gfxstream::vulkan_ok());
        // Claimed once; later boots (guest reboots) reuse it.
        assert!(Arc::ptr_eq(&r, &renderer(&spec(lib64)).expect("again")));
    }
}
