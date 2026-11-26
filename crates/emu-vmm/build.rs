//! Which hypervisor backend this target gets (`crate::hv`): `hv_hvf` (Hypervisor.framework on
//! Apple silicon) or `hv_kvm` (KVM on arm64 Linux), and `hv` for either: the targets that can boot
//! a guest. Everything else builds the device models only.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(hv, hv_hvf, hv_kvm)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let backend = match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => Some("hv_hvf"),
        ("linux", "aarch64") => Some("hv_kvm"),
        _ => None,
    };
    if let Some(b) = backend {
        println!("cargo::rustc-cfg=hv");
        println!("cargo::rustc-cfg={b}");
    }
}
