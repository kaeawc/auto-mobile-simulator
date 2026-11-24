// Lets the binary find the gfxstream renderer's libraries at runtime: next to the binary (an SDK
// overlay installs emu as emulator/emulator; a target/ build gets a lib64 symlink at startup),
// then in the build machine's SDK, either its emulator package or the renderer that
// `emu -provision-renderer` keeps under simee/renderer/current. The renderer's @rpath
// dependencies are preloaded by emu-gfxstream and don't need these; libraries gfxstream
// dlopens by bare name (libshadertranslator.dylib) do.
fn main() {
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_SDK_ROOT");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/lib64");
    // Host GLES is ANGLE on Metal; gfxstream dlopens libEGL/libGLESv2 by bare name.
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/lib64/gles_angle");
    let sdk = std::env::var("ANDROID_SDK_ROOT")
        .or_else(|_| std::env::var("ANDROID_HOME"))
        .unwrap_or_else(|_| format!("{}/Library/Android/sdk", std::env::var("HOME").unwrap_or_default()));
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/emulator/lib64");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/simee/renderer/current/lib64");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/emulator/lib64/gles_angle");
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{sdk}/simee/renderer/current/lib64/gles_angle");
}
