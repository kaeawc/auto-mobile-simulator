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
//! `-load-only` skips the link and stops after loading: the backend's `@rpath` dependencies then
//! resolve through preloading alone (this binary's only rpath is `@executable_path/lib64`).
//! `-emu` initializes the renderer as `emu` does for an API 36 guest (no GL, Vulkan composition,
//! emu's features), and every run prints how long the init took: the cold init's Metal shader
//! compiles show there (docs/android.md, "Renderer init and Metal's shader cache").
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    const USAGE: &str = "usage: renderer_caps [-load-only | -emu] <lib64> [moltenvk|swiftshader|lavapipe]";
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let load_only = args.first().is_some_and(|a| a == "-load-only");
    let as_emu = args.first().is_some_and(|a| a == "-emu");
    if load_only || as_emu {
        args.remove(0);
    }
    let lib64 = PathBuf::from(args.first().ok_or(USAGE)?);
    let icd = match args.get(1).map(String::as_str) {
        Some("lavapipe") => emu_gfxstream::Icd::Lavapipe,
        Some("swiftshader") => emu_gfxstream::Icd::SwiftShader,
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
    let deps = emu_gfxstream::bundled_deps(&lib64, emu_gfxstream::BACKEND);
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
        r.init(720, 1280, flags::USE_GLES | flags::USE_VK, false, std::sync::Arc::new(|_| {}))?;
    }
    println!("initialized in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
    for id in emu_gfxstream::capset::ALL {
        match r.capset_info(id) {
            Some((version, size)) => println!("capset {id}: version {version}, {size} bytes"),
            None => println!("capset {id}: unsupported"),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("renderer_caps runs on macOS only");
}
