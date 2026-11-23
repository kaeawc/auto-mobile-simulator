// The renderer_caps example resolves libraries gfxstream dlopens by bare name the way `emu` does:
// through `@executable_path/lib64`, which `link_beside_exe` points at the renderer.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-examples=-Wl,-rpath,@executable_path/lib64");
    }
}
