//! `emu`: boots an Android Virtual Device with the simee VMM.
//!
//! Accepts the SDK `emulator` command line (single-dash flags, `-avd NAME` or `@NAME`) so
//! existing tooling can launch it unchanged; flags that don't apply yet are accepted and
//! reported once with `-verbose`.

mod args;
// The VMM, the renderer and everything that drives them exist only on Apple silicon Macs (the
// Hypervisor.framework); elsewhere `emu` still lists, creates and provisions AVDs.
mod console;
// Builds and tests everywhere; only an Apple silicon boot serves it.
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
mod display_server;
#[cfg_attr(not(all(target_os = "macos", target_arch = "aarch64")), allow(dead_code))]
mod fold;
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
    let started = std::time::Instant::now();
    let _ = std::thread::Builder::new().name("signal-guard".into()).spawn(move || {
        emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
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
            std::thread::sleep(signal_guard_interval(started.elapsed()));
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("signals".into()).spawn(move || {
        emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
        let mut sig = 0;
        // SAFETY: `set` is a valid sigset; sigwait writes the delivered signal number.
        if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
            let _ = tx.send(sig);
        }
    });
    rx
}

/// While native libraries may still be installing handlers, the signal guard checks often.
/// The renderer, which does, initializes in the first seconds (beside kernel load, hundreds of
/// ms) and once per process (guest resets reuse it); the window leaves room for a cold disk and
/// the first boot's GPU device setup.
const SIGNAL_GUARD_FAST: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the signal guard sleeps between checks, `since_start` after it started: 200 ms in
/// [`SIGNAL_GUARD_FAST`], then 5 s (an idle emulator's guard wakes 0.2 times a second, not 5).
fn signal_guard_interval(since_start: std::time::Duration) -> std::time::Duration {
    if since_start < SIGNAL_GUARD_FAST { std::time::Duration::from_millis(200) } else { std::time::Duration::from_secs(5) }
}

/// macOS's `OPEN_MAX`: the most open files `setrlimit` grants a process there whatever its hard
/// limit says, and room for the NAT's flow caps and the rest of emu.
const NOFILE_WANT: u64 = 10240;

/// Raises the soft open-file limit (256 by default on macOS) toward the hard one, up to
/// [`NOFILE_WANT`]: each of the guest's network flows is a host socket of this process, and
/// running out also fails the console's and the adb bridge's `accept`. Never lowers it. Returns
/// the soft limit in force.
fn raise_nofile() -> u64 {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let cur = getrlimit(Resource::Nofile);
    let soft = cur.current.unwrap_or(u64::MAX);
    let want = cur.maximum.unwrap_or(u64::MAX).min(NOFILE_WANT);
    if soft >= want {
        return soft;
    }
    match setrlimit(Resource::Nofile, Rlimit { current: Some(want), maximum: cur.maximum }) {
        Ok(()) => want,
        Err(e) => {
            eprintln!("emu: warning: the open-file limit stays {soft} ({e}): the guest gets fewer network flows");
            soft
        }
    }
}

/// How often a running instance tells the adb server about itself again.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ADB_REANNOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

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
    let launched = std::time::Instant::now();
    // First, before any thread exists.
    let signals = shutdown_signals();
    // Before the NAT sizes its flow caps from it.
    raise_nofile();
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
        println!("simee {}", env!("CARGO_PKG_VERSION"));
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
    if let Some(name) = &opts.clone_avd {
        let golden = opts.from.as_deref().context("-clone-avd needs -from GOLDEN")?;
        let dir = env.clone_avd(golden, name)?;
        println!("cloned {golden} as {name} at {}", dir.display());
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(name) = &opts.delete_avd {
        env.delete_avd(name)?;
        println!("deleted {name}");
        return Ok(ExitCode::SUCCESS);
    }
    if opts.accel_check {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            println!("accel:\n0\nHypervisor.framework (arm64) is usable\naccel");
            return Ok(ExitCode::SUCCESS);
        }
        println!("accel:\n1\nsimee needs Hypervisor.framework on an Apple silicon Mac\naccel");
        return Ok(ExitCode::FAILURE);
    }
    let name = opts.avd.clone().context("no AVD given (use -avd NAME, or -list-avds)")?;
    if opts.dry_run {
        return dry_run(&env, &name, &opts);
    }
    boot(&env, &name, &opts, signals, launched)
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
    let unsupported = instance::unsupported(&avd).filter(|_| !instance::allow_old_images());
    if let Some(why) = &unsupported {
        eprintln!("emu: {why}");
    }
    Ok(if missing.is_empty() && unsupported.is_none() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn boot(_: &emu_avd::Env, name: &str, _: &args::Opts, _: std::sync::mpsc::Receiver<i32>, _: std::time::Instant) -> Result<ExitCode> {
    anyhow::bail!("cannot boot {name}: simee needs Hypervisor.framework on an Apple silicon Mac")
}

/// Once the guest has booted, a foldable tells it its posture (stock sends it after boot).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn announce_fold_at_boot(cfg: &mut emu_vmm::MachineConfig, fold: Option<std::sync::Arc<fold::Foldable>>) {
    let Some(fold) = fold else { return };
    let before = cfg.on_guest_event.take();
    cfg.on_guest_event = Some(std::sync::Arc::new(move |event: &str| {
        if let Some(f) = &before {
            f(event);
        }
        if event == "bootcomplete" {
            fold.announce();
        }
    }));
}

/// Boots the AVD and runs it until the guest powers off or a signal arrives.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn boot(
    env: &emu_avd::Env,
    name: &str,
    opts: &args::Opts,
    signals: std::sync::mpsc::Receiver<i32>,
    launched: std::time::Instant,
) -> Result<ExitCode> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Before anything is created or started: refusing takes milliseconds, and the error is the
    // last line front ends show (`emu exited … before booting: <line>`).
    if !instance::allow_old_images()
        && let Some(why) = instance::unsupported(&env.load(name)?)
    {
        anyhow::bail!(why);
    }
    if opts.verbose && !opts.ignored.is_empty() {
        eprintln!("emu: accepted but not implemented yet: {}", opts.ignored.join(" "));
    }

    let stop = Arc::new(AtomicBool::new(false));
    // A foldable, once there is one: it resets a size it forced before the guest stops.
    let foldable: Arc<std::sync::OnceLock<Arc<fold::Foldable>>> = Arc::default();
    {
        let stop = stop.clone();
        let foldable = foldable.clone();
        // Without it a SIGINT/SIGTERM would never stop the machine: fail the launch instead.
        std::thread::Builder::new().name("stop-watch".into()).spawn(move || {
            if signals.recv().is_ok() {
                if let Some(f) = foldable.get() {
                    f.release();
                }
                emu_vmm::request_stop(&stop);
                // Device threads blocked in native code (e.g. a wedged renderer) can stall an
                // orderly teardown; don't let that keep the process alive.
                std::thread::sleep(std::time::Duration::from_secs(5));
                eprintln!("emu: shutdown took over 5 s; exiting");
                std::process::exit(0);
            }
        })?;
    }

    let avd = env.load(name)?;
    let inst = instance::Instance::prepare(&avd, opts)?;
    {
        // An adb server finds emulators by scanning adb ports 5555..5585 (16 instances, unless
        // started with ADB_LOCAL_TRANSPORT_MAX_PORT), or by being told (`host:emulator:<port>`,
        // sent at boot). A server started later (adb kill-server, a reboot of adb) never hears
        // of an instance above 5585, so it's told again every few seconds: for a device it
        // already has, the server ignores it (one loopback connect and write).
        let adb_port = inst.console_port + 1;
        let _ = std::thread::Builder::new().name("adb-announce".into()).spawn(move || {
            emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
            loop {
                std::thread::sleep(ADB_REANNOUNCE);
                emu_vmm::pipe::adb::AdbService::announce_port(adb_port);
            }
        });
    }
    let fold = fold::for_avd(&avd, inst.sensors.clone(), inst.display.clone(), inst.console_port).map(Arc::new);
    if let Some(f) = &fold {
        let _ = foldable.set(f.clone());
    }
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
            network: Arc::new(std::sync::Mutex::new(opts.network)),
            fold: fold.clone(),
        },
    )?;
    let _advert = console::Advertisement::write(&avd.name, &avd.dir, inst.console_port);
    // Live display + input for the desktop app (<avd>/simee/display.sock).
    let _display = inst.display.clone().and_then(|d| display_server::start(&avd.dir, d, inst.inputs.clone(), stop.clone()));
    // On stdout: launchers watch for the first line and the `emulator-<port>` serial.
    println!("emu: {} on emulator-{} (adb port {})", avd.name, inst.console_port, inst.console_port + 1);
    let mut cfg = inst.machine_config(opts, launched)?;
    announce_fold_at_boot(&mut cfg, fold.clone());
    // The cheap checks passed (console port bound, the boot files exist) and `prepare` has set
    // the renderer's environment: bring the renderer up beside kernel load and VM setup. Nothing
    // fallible runs between here and `run`, which claims it (or, failing before the GPU, waits
    // for it to finish) before it returns.
    if let Some(gpu) = inst.gpu_spec(opts) {
        emu_vmm::start_renderer(gpu);
    }
    loop {
        let out = emu_vmm::run(cfg, stop.clone())?;
        if opts.verbose {
            eprintln!("emu: stopped after {:.1}s: {:?}", out.uptime.as_secs_f64(), out.reason);
            for (i, exits) in out.exits.iter().enumerate() {
                let line: Vec<String> = exits.iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{k}={n}")).collect();
                eprintln!("emu:   vcpu{i}: {}", line.join(" "));
            }
            if let Some(spool) = &inst.logcat {
                eprintln!("emu:   logcat bytes dropped (its output fell behind): {}", spool.dropped());
            }
        }
        match out.reason {
            emu_vmm::StopReason::Reset if !stop.load(Ordering::SeqCst) => {
                cfg = inst.machine_config(opts, launched)?;
                announce_fold_at_boot(&mut cfg, fold.clone());
            }
            emu_vmm::StopReason::Fault(msg) => {
                eprintln!("emu: guest fault: {msg}");
                return Ok(ExitCode::FAILURE);
            }
            _ => return Ok(ExitCode::SUCCESS),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Checks the guard makes in `[from, to)` of its life, following its own schedule.
    fn guard_checks(from: Duration, to: Duration) -> usize {
        let (mut t, mut n) = (Duration::ZERO, 0);
        while t < to {
            if t >= from {
                n += 1;
            }
            t += signal_guard_interval(t);
        }
        n
    }

    #[test]
    fn the_open_file_limit_is_raised_toward_the_hard_limit_and_never_lowered() {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        let was = getrlimit(Resource::Nofile);
        let hard = was.maximum.unwrap_or(u64::MAX);
        let want = hard.min(NOFILE_WANT);
        // From a low soft limit (macOS's default is 256; 1024 leaves the other tests room).
        let low = want.min(1024);
        setrlimit(Resource::Nofile, Rlimit { current: Some(low), maximum: was.maximum }).expect("lower");
        let raised = raise_nofile();
        let now = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
        setrlimit(Resource::Nofile, was).expect("restore");
        assert_eq!(raised, want);
        assert_eq!(now, want, "the process's limit, not only the return value");
        // Already higher (a shell's `ulimit -n`): left alone.
        let soft = was.current.unwrap_or(u64::MAX);
        if soft >= want {
            assert_eq!(raise_nofile(), soft);
        }
    }

    #[test]
    fn the_signal_guard_checks_often_at_startup_then_rarely() {
        // A handler a library installs during startup is undone within 200 ms.
        assert_eq!(signal_guard_interval(Duration::ZERO), Duration::from_millis(200));
        assert_eq!(signal_guard_interval(SIGNAL_GUARD_FAST - Duration::from_millis(1)), Duration::from_millis(200));
        assert_eq!(guard_checks(Duration::from_secs(1), Duration::from_secs(2)), 5);
        // Idle afterwards: at most one wakeup in any second (it was 5), and it never stops.
        for start in [SIGNAL_GUARD_FAST, SIGNAL_GUARD_FAST * 2, SIGNAL_GUARD_FAST * 10] {
            assert!(guard_checks(start, start + Duration::from_secs(1)) <= 1, "{start:?}");
            assert!(guard_checks(start, start + Duration::from_secs(10)) >= 1, "{start:?}");
        }
    }
}
