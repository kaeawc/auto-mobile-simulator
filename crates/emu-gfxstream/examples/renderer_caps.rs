//! Loads and initializes the gfxstream renderer from a `lib64` directory and prints its
//! capability sets; no VM involved. Checks a renderer install (the SDK's `emulator/lib64`, or
//! `<sdk>/simee/renderer/<revision>/lib64` from `emu -provision-renderer`):
//!
//! ```text
//! cargo run -p emu-gfxstream --example renderer_caps -- <lib64> [moltenvk|swiftshader|lavapipe]
//! cargo run -p emu-gfxstream --example renderer_caps -- -load-only <lib64>
//! cargo run -p emu-gfxstream --example renderer_caps -- -emu <lib64>
//! ```
//!
//! Like `emu`, it links `lib64` beside itself and sets the launcher directory to its parent.
//! `-load-only` skips the link and stops after loading: the backend's dependencies then resolve
//! through preloading alone (this binary's only rpath is `@executable_path/lib64` on macOS; on
//! Linux it has none). The ICD defaults to MoltenVK on macOS and lavapipe on Linux. `-vulkan-only`
//! initializes without the GL backend, as emu does. `-emu` initializes the renderer as `emu` does
//! for an API 36 guest (no GL, Vulkan composition, emu's features), and every run prints how long
//! the init took: the cold init's Metal shader compiles show there (docs/android.md, "Renderer
//! init and Metal's shader cache").
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    const USAGE: &str = "usage: renderer_caps [-load-only|-vulkan-only|-emu] <lib64> [moltenvk|swiftshader|lavapipe]";
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let load_only = args.first().is_some_and(|a| a == "-load-only");
    let vulkan_only = args.first().is_some_and(|a| a == "-vulkan-only");
    let as_emu = args.first().is_some_and(|a| a == "-emu");
    if load_only || vulkan_only || as_emu {
        args.remove(0);
    }
    let lib64 = PathBuf::from(args.first().ok_or(USAGE)?);
    let icd = match args.get(1).map(String::as_str) {
        Some("lavapipe") => emu_gfxstream::Icd::Lavapipe,
        Some("swiftshader") => emu_gfxstream::Icd::SwiftShader,
        Some("moltenvk") => emu_gfxstream::Icd::MoltenVk,
        _ if cfg!(target_os = "linux") => emu_gfxstream::Icd::Lavapipe,
        _ => emu_gfxstream::Icd::MoltenVk,
    };
    let launcher = lib64.parent().ok_or("lib64 has no parent")?;
    if !load_only {
        let link = emu_gfxstream::link_beside_exe(&lib64);
        println!("link     {}", link.map_or_else(|| "(none)".to_string(), |l| l.display().to_string()));
    }
    emu_gfxstream::configure_environment(launcher, icd);
    println!("lib64    {}", lib64.display());
    println!("launcher {}", std::env::var("ANDROID_EMULATOR_LAUNCHER_DIR").unwrap_or_default());
    let deps = emu_gfxstream::preloads(&lib64);
    let names: Vec<String> = deps.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
    println!("preload  {}", names.join(" "));

    let r = emu_gfxstream::Renderer::load(&lib64)?;
    println!("loaded");
    if load_only {
        return Ok(());
    }
    use emu_gfxstream::flags;
    let t = std::time::Instant::now();
    if as_emu {
        let features = "RefCountPipe:enabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled,VulkanVirtualQueue:enabled";
        let flags = flags::USE_EGL | flags::USE_VK | flags::VULKAN_NATIVE_SWAPCHAIN;
        r.init_with_features(720, 1280, flags, Some(features), false, std::sync::Arc::new(|_| {}))?;
    } else {
        let flags = if vulkan_only { flags::USE_VK | flags::VULKAN_NATIVE_SWAPCHAIN } else { flags::USE_GLES | flags::USE_VK };
        r.init(720, 1280, flags, false, std::sync::Arc::new(|_| {}))?;
    }
    println!(
        "initialized in {:.0} ms (Vulkan {})",
        t.elapsed().as_secs_f64() * 1e3,
        if emu_gfxstream::vulkan_ok() { "up" } else { "failed" }
    );
    for id in emu_gfxstream::capset::ALL {
        match r.capset_info(id) {
            Some((version, size)) => println!("capset {id}: version {version}, {size} bytes"),
            None => println!("capset {id}: unsupported"),
        }
    }
    readback(&r)?;
    Ok(())
}

/// Uploads a pattern into a 2D color buffer and reads it back the way frames are read
/// (`Renderer::read_color_buffer`), checking the bytes and timing a full-screen-sized read.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn readback(r: &emu_gfxstream::Renderer) -> Result<(), Box<dyn std::error::Error>> {
    let (w, h) = (720u32, 1280u32);
    let mut src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
    // SAFETY: `src` outlives the resource's use below (it is unreffed before `src` drops).
    let backing = unsafe { emu_gfxstream::HostRange::from_raw(src.as_mut_ptr(), src.len()) };
    // PIPE_TEXTURE_2D, R8G8B8A8 (virgl format 67), render target | sampler view.
    let args = emu_gfxstream::ResourceCreateArgs {
        handle: 1,
        target: 2,
        format: 67,
        bind: (1 << 1) | (1 << 3),
        width: w,
        height: h,
        depth: 1,
        array_size: 1,
        ..Default::default()
    };
    r.resource_create(args, &[backing])?;
    r.transfer_write(1, 0, 0, w * 4, 0, emu_gfxstream::Box3 { x: 0, y: 0, z: 0, w, h, d: 1 }, 0)?;
    let mut out = vec![0u8; src.len()];
    let mut times = Vec::new();
    for _ in 0..9 {
        let t = std::time::Instant::now();
        r.read_color_buffer(1, w, h, &mut out)?;
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(f64::total_cmp);
    let same = out == src;
    r.resource_unref(1);
    println!("readback {w}x{h}: {} (median {:.2} ms of 9)", if same { "matches" } else { "DIFFERS" }, times[4]);
    if !same {
        return Err("read back pixels differ from the upload".into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn main() {
    eprintln!("renderer_caps runs on macOS and Linux only");
}
