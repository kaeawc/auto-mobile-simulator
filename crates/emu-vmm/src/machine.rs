//! The machine: builds the board, its devices and vCPUs, and runs the guest (Apple silicon only).
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

static RENDERER: std::sync::OnceLock<Arc<emu_gfxstream::Renderer>> = std::sync::OnceLock::new();

/// The renderer is process-wide: loaded and initialized once, reused across guest reboots.
fn renderer(spec: &GpuSpec) -> Result<Arc<emu_gfxstream::Renderer>> {
    if let Some(r) = RENDERER.get() {
        return Ok(r.clone());
    }
    let r = emu_gfxstream::Renderer::load(&spec.lib64).context("loading the gfxstream renderer")?;
    let mut flags = emu_gfxstream::flags::USE_EGL | emu_gfxstream::flags::USE_GLES | emu_gfxstream::flags::USE_VK;
    if std::env::var_os("EMU_GL_COMPOSITION").is_none() {
        flags |= emu_gfxstream::flags::VULKAN_NATIVE_SWAPCHAIN;
    }
    // What the ranchu gralloc expects of the host:
    // - color buffer lifetimes follow its `refcount` pipes (RefCountPipe); otherwise the
    //   allocator's own release frees buffers SurfaceFlinger and apps are still using;
    // - CPU reads of color buffers (screencap, software readers) by DMA into shared-slot host
    //   memory (`ANDROID_EMU_read_color_buffer_dma` = GlDirectMem + shared slots), resolved
    //   through the DMA translator `run` installs.
    // EMU_RENDERER_FEATURES adds overrides, e.g. "HostComposition:disabled".
    let mut features = String::from("RefCountPipe:enabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled");
    if let Ok(extra) = std::env::var("EMU_RENDERER_FEATURES") {
        features.push(',');
        features.push_str(&extra);
    }
    r.init_with_features(spec.width, spec.height, flags, Some(&features), spec.verbose, virtio::gpu::fence_handler())
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
    /// A silent virtio-snd card (ALSA card 0).
    pub sound: bool,
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

/// Boots the machine and blocks until it powers off, resets, faults, or `stop` becomes true.
pub fn run(cfg: MachineConfig, stop: Arc<AtomicBool>) -> Result<Outcome> {
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
    let kernel = boot::parse_kernel(std::fs::read(&cfg.kernel).with_context(|| format!("reading {}", cfg.kernel.display()))?)?;
    let entry = KERNEL_ADDR + kernel.text_offset;
    if entry + kernel.image_size > INITRD_ADDR {
        bail!("kernel image ({} bytes) overlaps the initrd area", kernel.image_size);
    }
    mem.write(entry, &kernel.image)?;
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
        // Port 0 is the console port; named ports follow (qemu-props publishes them as
        // vendor.qemu.vport.<name>).
        use virtio::console::{Console, PortSpec, Sink};
        let (vcon, handles) = Console::new(vec![
            PortSpec { name: String::new(), backend: Box::new(Sink) },
            PortSpec { name: "modem".into(), backend: cfg.telephony.backend() },
        ]);
        if let Some(h) = handles.get(1) {
            cfg.telephony.attach(h.clone());
        }
        slots[slots::VPORTS as usize] = Some(Box::new(vcon));
    }
    if cfg.rng {
        slots[slots::RNG as usize] = Some(Box::new(virtio::rng::Rng::new()));
    }
    if cfg.sound {
        slots[slots::SOUND as usize] = Some(Box::new(virtio::snd::Snd::new()));
    }
    let mut dma_renderer = None;
    let mut gpu_renderer = None;
    if let Some(spec) = &cfg.gpu {
        let r = renderer(spec)?;
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

    let reason = loop {
        if let Some(r) = cpus.wait_stop(Duration::from_millis(100)) {
            break r;
        }
        if stop.load(Ordering::SeqCst) {
            cpus.request_stop(StopReason::Requested);
        }
    };
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
    Ok(Outcome { reason, uptime: started.elapsed(), exits })
}
