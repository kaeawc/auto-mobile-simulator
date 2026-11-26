//! `emu` carries a bundle identifier (an `__info_plist` section), so Metal keeps its shader cache
//! for emu apart from the one every unbundled process shares (docs/android.md, "Renderer init and
//! Metal's shader cache").
#![cfg(target_os = "macos")]

/// Kept in step with `apps/emu/build.rs`.
const BUNDLE_ID: &str = "dev.simee.emu";

#[test]
fn the_binary_names_its_own_bundle_so_metal_caches_its_shaders_apart() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_emu")).expect("emu binary");
    let has = |s: &str| exe.windows(s.len()).any(|w| w == s.as_bytes());
    assert!(has("__info_plist"), "no __info_plist section in emu");
    assert!(has(&format!("<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>")), "emu's Info.plist doesn't name {BUNDLE_ID}");
}
