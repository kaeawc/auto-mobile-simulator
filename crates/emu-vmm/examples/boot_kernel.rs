//! Boots a bare arm64 kernel (and initrd) on the stock board, with no AVD, system image or GPU,
//! and reports how the machine stopped, its vCPUs' exit counts and the time from launch to
//! power-off. A hypervisor smoke test (`scripts/kvm-smoke.sh` runs it on KVM inside an emulated
//! EL2 machine), and a quick way to bring up a new backend.
//!
//!     cargo run --release -p emu-vmm --example boot_kernel -- <Image[.gz]> [initrd] [cpus] [cmdline...]
//!
//! The guest console (PL011) goes to stdout. Exits 0 when the guest powered off.
#[cfg(hv)]
fn main() -> std::process::ExitCode {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(kernel) = args.first() else {
        eprintln!("usage: boot_kernel <Image[.gz]> [initrd] [cpus] [cmdline...]");
        return std::process::ExitCode::FAILURE;
    };
    let initrd = args.get(1).filter(|p| !p.is_empty()).map(|p| std::fs::read(p).expect("reading the initrd"));
    let cpus = args.get(2).and_then(|c| c.parse().ok()).unwrap_or(2);
    let cmdline = if args.len() > 3 { args[3..].join(" ") } else { "console=ttyAMA0 earlycon=pl011,0x9000000 panic=-1".into() };
    let launched = std::time::Instant::now();
    let cfg = emu_vmm::MachineConfig {
        cpus,
        ram_bytes: 512 << 20,
        kernel: kernel.into(),
        kernel_cache: None,
        initrd,
        cmdline,
        disks: Vec::new(),
        disk_fsync: false,
        rng: true,
        adb: None,
        gpu: None,
        boot_props: Vec::new(),
        on_guest_event: None,
        sensors: Arc::default(),
        clipboard: Arc::default(),
        fingerprint: Arc::default(),
        gps: Arc::default(),
        battery: Arc::default(),
        telephony: Arc::default(),
        car: None,
        sound: None,
        input: None,
        touch_screen: false,
        touchpad: None,
        dual_mode_mouse: false,
        xr: None,
        on_input: None,
        console: Box::new(std::io::stdout()),
        logcat: None,
        launched: Some(launched),
    };
    match emu_vmm::run(cfg, Arc::new(AtomicBool::new(false))) {
        Ok(out) => {
            eprintln!("boot_kernel: stopped after {:.2} s: {:?}", out.uptime.as_secs_f64(), out.reason);
            for (i, exits) in out.exits.iter().enumerate() {
                let line: Vec<String> = exits.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{k}={n}")).collect();
                eprintln!("boot_kernel:   vcpu{i}: {}", line.join(" "));
            }
            eprintln!("boot_kernel: queue notifies completed in the kernel: {}", out.kernel_notifies);
            if out.reason == emu_vmm::StopReason::PowerOff { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("boot_kernel: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(hv))]
fn main() {
    eprintln!("boot_kernel: this host has no hypervisor backend (Apple silicon or arm64 Linux with KVM)");
    std::process::exit(1);
}
