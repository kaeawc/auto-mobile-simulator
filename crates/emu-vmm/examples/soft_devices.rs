//! Serves emu's device set for an Android guest whose CPU runs in QEMU (software mode on Linux,
//! #55) over vhost-user, one socket per device, and prints the QEMU arguments that attach them.
//! A prototype of what `emu` will run itself: vsock (adb and the qemud pipes), the virtio-console
//! ports (the modem, logcat, uwb, bluetooth), and virtio-net behind emu's user-mode NAT.
//!
//! With `EMU_LIB64=<renderer lib64>` it serves the GPU too: gfxstream on lavapipe, Vulkan only,
//! blobs exported as fds and mapped by QEMU (`SHMEM_MAP`).
//!
//! ```text
//! cargo run --release -p emu-vmm --example soft_devices -- <socket dir> <adb port> [prop=value ...]
//! ```
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::sync::Arc;

    use emu_vmm::pipe::{PipeRegistry, adb::AdbService, misc::MiscService, qemud::BootProperties, sensors};
    use emu_vmm::virtio::console::{Console, PortSpec, Sink};
    use emu_vmm::virtio::vsock::{Vsock, event_channel};
    use emu_vmm::virtio::{VirtioDevice, net};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(dir), Some(port)) = (args.first().map(PathBuf::from), args.get(1).and_then(|p| p.parse::<u16>().ok())) else {
        return Err("usage: soft_devices <socket dir> <adb port> [prop=value ...]".into());
    };
    std::fs::create_dir_all(&dir)?;
    let props = args[2..].iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();

    let (events, rx) = event_channel();
    let adb = AdbService::bind(port, events.clone())?;
    let mut registry = PipeRegistry::default();
    registry.register("qemud:adb", Box::new(adb));
    registry.register("qemud:boot-properties", Box::new(BootProperties { props }));
    registry.register("qemud:sensors", Box::new(sensors::SensorService { sensors: Arc::new(sensors::Sensors::default()) }));
    registry.register("QemuMiscPipe", Box::new(MiscService { on_event: None }));
    let vsock = Vsock::new(3, Arc::new(registry), events, rx);

    let telephony = Arc::new(emu_vmm::telephony::Telephony::default());
    let (console, handles) = Console::new(vec![
        PortSpec { name: String::new(), backend: Box::new(Sink) },
        PortSpec { name: "modem".into(), backend: telephony.backend() },
        PortSpec { name: String::new(), backend: Box::new(Sink) },
        PortSpec { name: "uwb".into(), backend: Box::new(emu_vmm::bt::UwbPort::default()) },
        PortSpec { name: "bluetooth".into(), backend: Box::new(emu_vmm::bt::BluetoothPort::new(emu_vmm::bt::address_for(b"soft"))) },
    ]);
    if let Some(h) = handles.get(1) {
        telephony.attach(h.clone());
    }
    let nat: Box<dyn net::Backend> = match emu_vmm::net::Nat::new(emu_vmm::net::Config::default()) {
        Ok(n) => Box::new(n),
        Err(e) => {
            eprintln!("no NAT ({e}): link only");
            Box::new(net::LinkOnly)
        }
    };
    let net = net::Net::new(nat);

    // (name, virtio id, device); QEMU's generic frontend takes the id, queue count and config size.
    let mut devices: Vec<(&str, Box<dyn VirtioDevice>, emu_vmm::vhost_user::BackendChannel)> =
        vec![("console", Box::new(console), Default::default()), ("net", Box::new(net), Default::default())];
    let mut qemu = vec![
        format!("-chardev socket,id=vsock,path={}", dir.join("vsock.sock").display()),
        "-device vhost-user-vsock-pci,chardev=vsock".into(),
    ];
    let mut servers = vec![emu_vmm::vhost_user::serve(&dir.join("vsock.sock"), "vsock", Box::new(vsock))?];
    let mut qemu_extra = Vec::new();
    if let Some(lib64) = std::env::var_os("EMU_LIB64").map(PathBuf::from) {
        // The ranchu gralloc's host memory: the goldfish address space, over vfio-user.
        let gas = emu_vmm::vfio_user::GoldfishAs::new(16 << 30)?;
        let (area, ram) = (gas.area(), gas.guest_ram_handle());
        let gas_path = dir.join("goldfish-as.sock");
        servers.push(emu_vmm::vfio_user::serve(&gas_path, "goldfish-as", Box::new(gas))?);
        qemu_extra.push(format!(r#"-device {{"driver":"vfio-user-pci","socket":{{"path":"{}","type":"unix"}}}}"#, gas_path.display()));
        let (dev, chan, frames) = gpu(&lib64, area, ram)?;
        devices.push(("gpu", dev, chan));
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                println!("frames presented: {}", frames.frames());
            }
        });
    }

    for (name, dev, chan) in devices {
        let (id, queues, config) = (dev.device_id(), dev.num_queues(), config_size(dev.as_ref()));
        let path = dir.join(format!("{name}.sock"));
        qemu.push(format!("-chardev socket,id={name},path={}", path.display()));
        qemu.push(format!("-device vhost-user-test-device-pci,chardev={name},virtio-id={id},num_vqs={queues},config_size={config}"));
        servers.push(emu_vmm::vhost_user::serve_with(&path, name, dev, chan)?);
    }
    qemu.extend(qemu_extra);
    println!("adb on 127.0.0.1:{port}; QEMU arguments:\n{}", qemu.join(" "));
    AdbService::announce_port(port);
    for s in servers {
        let _ = s.join();
    }
    Ok(())
}

/// The GPU: the renderer from `lib64` on lavapipe, Vulkan only, with blobs it can export. The
/// features that need goldfish devices QEMU doesn't have (the `refcount` pipe, address-space shared
/// slots) are off. `EMU_RENDERER_FEATURES` adds overrides.
#[cfg(target_os = "linux")]
#[allow(clippy::type_complexity)]
fn gpu(
    lib64: &std::path::Path,
    area: emu_vmm::vfio_user::AreaHandle,
    ram: emu_vmm::vfio_user::AreaHandle,
) -> Result<
    (Box<dyn emu_vmm::virtio::VirtioDevice>, emu_vmm::vhost_user::BackendChannel, std::sync::Arc<emu_vmm::display::FrameStore>),
    Box<dyn std::error::Error>,
> {
    use std::sync::Arc;
    use std::time::Duration;

    use emu_gfxstream::{Icd, Renderer, flags};
    use emu_vmm::display::{DisplaySink, FrameStore, PostTicker};
    use emu_vmm::virtio::gpu::{Gpu, GpuConfig, fence_handler};

    let (w, h) = (720, 1280);
    emu_gfxstream::configure_environment(lib64.parent().ok_or("lib64 has no parent")?, Icd::Lavapipe);
    let r = Arc::new(Renderer::load(lib64)?);
    let store = Arc::new(FrameStore::with_idle(Duration::from_secs(5)));
    let ticker = PostTicker::start(store.clone(), w, h, Duration::from_millis(16), Duration::from_millis(50));
    let fences = fence_handler();
    let on_fence: Arc<dyn Fn(emu_gfxstream::Fence) + Send + Sync> = Arc::new(move |f| {
        fences(f);
        ticker.poke();
    });
    let mut features =
        String::from("RefCountPipe:disabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled,VulkanVirtualQueue:enabled");
    if let Ok(extra) = std::env::var("EMU_RENDERER_FEATURES") {
        features.push(',');
        features.push_str(&extra);
    }
    let rflags = flags::USE_EGL | flags::USE_VK | flags::VULKAN_NATIVE_SWAPCHAIN | flags::USE_EXTERNAL_BLOB;
    r.init_with_features(w, h, rflags, Some(&features), std::env::var_os("EMU_VERBOSE").is_some(), on_fence)?;
    // Guest-physical to host for the renderer's DMA paths: guest RAM and the address-space area.
    let translate: emu_gfxstream::DmaTranslator = Box::new(move |gpa| {
        let hit = |h: &emu_vmm::vfio_user::AreaHandle| h.get().and_then(|m| m.host_range(gpa, 1));
        hit(&ram).or_else(|| hit(&area)).map_or(std::ptr::null_mut(), |h| h.as_ptr())
    });
    r.set_dma_translator(Some(translate))?;
    let reader = r.clone();
    store.set_readback(Box::new(move |w, h, buf| reader.read_last_posted_to(w, h, buf).is_ok()));
    let chan = emu_vmm::vhost_user::BackendChannel::default();
    let window = Arc::new(emu_vmm::vhost_user::ShmemWindow::new(chan.clone(), emu_vmm::virtio::gpu::SHM_HOST_VISIBLE, r.clone()));
    let gpu = Gpu::new(GpuConfig {
        width: w,
        height: h,
        renderer: r,
        shm: (0, 8 << 30),
        window,
        display: store.clone(),
        verbose: std::env::var_os("EMU_VERBOSE").is_some(),
    });
    Ok((Box::new(gpu), chan, store))
}

/// The size of the device's config space: the last offset that reads non-zero, rounded up to 4
/// bytes, as QEMU's generic frontend needs it up front.
#[cfg(target_os = "linux")]
fn config_size(dev: &dyn emu_vmm::virtio::VirtioDevice) -> usize {
    let mut buf = [0u8; 256];
    dev.read_config(0, &mut buf);
    buf.iter().rposition(|&b| b != 0).map_or(0, |i| (i + 1).next_multiple_of(4))
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("soft_devices runs on Linux only");
}
