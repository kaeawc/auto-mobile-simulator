//! `emu`: boots an Android Virtual Device with the emu-rs VMM.
//!
//! Accepts the SDK `emulator` command line (single-dash flags, `-avd NAME` or `@NAME`) so
//! existing tooling can launch it unchanged; flags that don't apply yet are accepted and
//! reported once with `-verbose`.

mod args;
// The VMM, the renderer and everything that drives them exist only on Apple silicon Macs (the
// Hypervisor.framework); elsewhere `emu` still lists, creates and provisions AVDs.
mod console;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod display_server;
mod instance;
mod provision;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vkprobe;

use std::process::ExitCode;

use anyhow::{Context, Result};

/// Blocks SIGINT/SIGTERM/SIGHUP in this thread (and so in every thread created after it,
/// including native libraries' threads) and returns a receiver that fires once any of them
/// arrives. A dedicated thread `sigwait`s for them, so a library that installs its own handlers
/// (the GPU renderer does) can't swallow the shutdown signal.
#[allow(unsafe_code)]
fn shutdown_signals() -> std::sync::mpsc::Receiver<i32> {
    // SAFETY: plain libc signal-mask calls on a stack-allocated, initialized sigset.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    // Native libraries may set these signals to SIG_IGN (the GPU renderer does), and ignored
    // signals are discarded even while blocked. Keep their disposition at SIG_DFL: blocked
    // everywhere, the default action never runs, and sigwait receives them.
    let _ = std::thread::Builder::new().name("signal-guard".into()).spawn(|| {
        loop {
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                // SAFETY: querying and restoring a disposition with zero-initialized sigactions.
                unsafe {
                    let mut cur: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, std::ptr::null(), &mut cur) == 0 && cur.sa_sigaction != libc::SIG_DFL {
                        let mut dfl: libc::sigaction = std::mem::zeroed();
                        dfl.sa_sigaction = libc::SIG_DFL;
                        libc::sigaction(sig, &dfl, std::ptr::null_mut());
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("signals".into()).spawn(move || {
        let mut sig = 0;
        // SAFETY: `set` is a valid sigset; sigwait writes the delivered signal number.
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
            let _ = tx.send(sig);
        }
    });
    rx
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("emu: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `-create-avd`: writes the AVD shaped like its image's form factor (a round watch, a TV, an XR
/// headset, ...), with the requested hardware on top, warning if its image is missing.
fn create_avd(env: &emu_avd::Env, name: &str, sysdir: &str, opts: &args::Opts) -> Result<()> {
    let mut spec = emu_avd::NewAvd::for_image(name, sysdir);
    if let Some(lcd) = opts.lcd {
        spec.lcd = lcd;
    }
    if let Some(c) = opts.cores {
        spec.cores = c;
    }
    if let Some(m) = opts.memory_mb {
        spec.ram_mb = m;
    }
    let dir = env.create_avd(&spec)?;
    // Fail now, not at first boot, if the image isn't installed.
    if let Err(e) = env.load(name) {
        eprintln!("emu: warning: {e} (install it with emu -provision <api>)");
    }
    println!("created {name} at {}", dir.display());
    Ok(())
}

fn real_main() -> Result<ExitCode> {
    // First, before any thread exists.
    let signals = shutdown_signals();
    // Internal: `emu -probe-vulkan <lib64>` (see vkprobe.rs).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let raw: Vec<String> = std::env::args().collect();
        if raw.get(1).map(String::as_str) == Some("-probe-vulkan") {
            let ok = raw.get(2).is_some_and(|p| vkprobe::probe(std::path::Path::new(p)));
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
    }
    let opts = args::parse(std::env::args().skip(1))?;
    let env = emu_avd::Env::from_env();

    if opts.help {
        print!("{}", args::USAGE);
        return Ok(ExitCode::SUCCESS);
    }
    if opts.version {
        println!("emu-rs {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    if opts.list_avds {
        for name in env.list_avds() {
            println!("{name}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if opts.list_images {
        provision::list_images(&env, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(api) = &opts.provision {
        provision::provision(&env, api, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if opts.provision_renderer {
        provision::provision_renderer(&env, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &opts.create_avd {
        let sysdir = opts.sysdir.clone().context("-create-avd needs -sysdir system-images/<target>/<tag>/<abi>")?;
        create_avd(&env, name, &sysdir, &opts)?;
        return Ok(ExitCode::SUCCESS);
    }
    if opts.accel_check {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            println!("accel:\n0\nHypervisor.framework (arm64) is usable\naccel");
            return Ok(ExitCode::SUCCESS);
        }
        println!("accel:\n1\nemu-rs needs Hypervisor.framework on an Apple silicon Mac\naccel");
        return Ok(ExitCode::FAILURE);
    }
    let name = opts.avd.clone().context("no AVD given (use -avd NAME, or -list-avds)")?;
    if opts.dry_run {
        return dry_run(&env, &name, &opts);
    }
    boot(&env, &name, &opts, signals)
}

/// `-avd NAME -dry-run`: prints the boot plan without booting (on any host), and fails if the
/// image is missing files the boot reads.
fn dry_run(env: &emu_avd::Env, name: &str, opts: &args::Opts) -> Result<ExitCode> {
    let avd = env.load(name)?;
    let renderer = if cfg!(all(target_os = "macos", target_arch = "aarch64")) { instance::renderer(&avd, opts) } else { None };
    let plan = instance::BootPlan::new(&avd, opts, renderer);
    print!("{plan}");
    let missing = plan.missing();
    for path in &missing {
        eprintln!("emu: missing {}", path.display());
    }
    Ok(if missing.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn boot(_: &emu_avd::Env, name: &str, _: &args::Opts, _: std::sync::mpsc::Receiver<i32>) -> Result<ExitCode> {
    anyhow::bail!("cannot boot {name}: emu-rs needs Hypervisor.framework on an Apple silicon Mac")
}

/// Boots the AVD and runs it until the guest powers off or a signal arrives.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn boot(env: &emu_avd::Env, name: &str, opts: &args::Opts, signals: std::sync::mpsc::Receiver<i32>) -> Result<ExitCode> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    if opts.verbose && !opts.ignored.is_empty() {
        eprintln!("emu: accepted but not implemented yet: {}", opts.ignored.join(" "));
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            if signals.recv().is_ok() {
                stop.store(true, Ordering::SeqCst);
                // Device threads blocked in native code (e.g. a wedged renderer) can stall an
                // orderly teardown; don't let that keep the process alive.
                std::thread::sleep(std::time::Duration::from_secs(5));
                eprintln!("emu: shutdown took over 5 s; exiting");
                std::process::exit(0);
            }
        });
    }

    let avd = env.load(name)?;
    let inst = instance::Instance::prepare(&avd, opts)?;
    console::serve(
        inst.console_port,
        console::ConsoleInfo {
            avd_name: avd.name.clone(),
            stop: stop.clone(),
            display: inst.display.clone(),
            inputs: inst.inputs.clone(),
            sensors: inst.sensors.clone(),
            clipboard: inst.clipboard.clone(),
            fingerprint: inst.fingerprint.clone(),
            gps: inst.gps.clone(),
            battery: inst.battery.clone(),
            telephony: inst.telephony.clone(),
            xr: inst.xr.clone(),
            car: inst.car.clone(),
        },
    )?;
    let _advert = console::Advertisement::write(&avd.name, &avd.dir, inst.console_port);
    // Live display + input for the desktop app (<avd>/emu-rs/display.sock).
    let _display = inst.display.clone().and_then(|d| display_server::start(&avd.dir, d, inst.inputs.clone(), stop.clone()));
    // On stdout: launchers watch for the first line and the `emulator-<port>` serial.
    println!("emu: {} on emulator-{} (adb port {})", avd.name, inst.console_port, inst.console_port + 1);
    loop {
        let cfg = inst.machine_config(opts)?;
        let out = emu_vmm::run(cfg, stop.clone())?;
        if opts.verbose {
            eprintln!("emu: stopped after {:.1}s: {:?}", out.uptime.as_secs_f64(), out.reason);
            for (i, exits) in out.exits.iter().enumerate() {
                let line: Vec<String> = exits.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{k}={n}")).collect();
                eprintln!("emu:   vcpu{i}: {}", line.join(" "));
            }
        }
        match out.reason {
            emu_vmm::StopReason::Reset if !stop.load(Ordering::SeqCst) => continue,
            emu_vmm::StopReason::Fault(msg) => {
                eprintln!("emu: guest fault: {msg}");
                return Ok(ExitCode::FAILURE);
            }
            _ => return Ok(ExitCode::SUCCESS),
        }
    }
}
