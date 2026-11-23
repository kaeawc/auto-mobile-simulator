//! One-time check that gfxstream can bring up Vulkan on MoltenVK. The renderer initializes once
//! per process and silently disables Vulkan on failure, so the check runs in a child process
//! (`emu -probe-vulkan <lib64>`) and its verdict is cached per renderer build.
use std::path::{Path, PathBuf};

fn cache_file(lib64: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let stamp = std::fs::metadata(lib64.join("libgfxstream_backend.dylib")).and_then(|m| m.modified()).ok()?;
    let secs = stamp.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(PathBuf::from(home).join(format!("Library/Caches/emu-rs/moltenvk-{secs}")))
}

/// Cached verdict, probing (≈1 s, once) when unknown.
pub fn moltenvk_works(lib64: &Path) -> bool {
    let cache = cache_file(lib64);
    if let Some(c) = &cache
        && let Ok(v) = std::fs::read_to_string(c)
    {
        return v.trim() == "ok";
    }
    let ok = std::env::current_exe()
        .ok()
        .and_then(|exe| std::process::Command::new(exe).arg("-probe-vulkan").arg(lib64).stderr(std::process::Stdio::null()).output().ok())
        .is_some_and(|o| o.status.success());
    if let Some(c) = cache {
        let _ = c.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(c, if ok { "ok" } else { "fail" });
    }
    ok
}

/// Child side: init the renderer on MoltenVK and report whether Vulkan came up.
pub fn probe(lib64: &Path) -> bool {
    let Some(emulator_dir) = lib64.parent() else { return false };
    emu_gfxstream::configure_environment(emulator_dir, emu_gfxstream::Icd::MoltenVk);
    let Ok(r) = emu_gfxstream::Renderer::load(lib64) else { return false };
    let flags = emu_gfxstream::flags::USE_GLES | emu_gfxstream::flags::USE_VK;
    r.init(320, 240, flags, false, std::sync::Arc::new(|_| {})).is_ok() && emu_gfxstream::vulkan_ok()
}
