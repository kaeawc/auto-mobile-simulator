//! Software mode (#55): booting an x86_64 AVD on a Linux host without hardware virtualization.
//! The guest's CPU runs in QEMU's multi-threaded TCG on stock's x86 board; emu serves everything
//! else (`emu_vmm::soft`), and the console port, `display.sock` and adb work as on a Mac.
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::{args, console, display_server, instance};

/// What to tell someone without a usable QEMU.
const NEED_QEMU: &str =
    "software mode runs the guest's CPU in QEMU 11.1 or newer: install it (qemu-system-x86) or point EMU_QEMU at a qemu-system-x86_64";

/// `-accel-check` in software mode: usable when a new enough QEMU is there, with what runs the
/// guest, as stock prints its accelerator.
pub fn accel_check() -> (bool, String) {
    let qemu = emu_vmm::soft::find_qemu(std::env::var("EMU_QEMU").ok().as_deref(), &std::env::var("PATH").unwrap_or_default())
        .ok_or_else(|| NEED_QEMU.to_string())
        .and_then(|q| emu_vmm::soft::check_qemu(&q).map(|v| (q, v)).map_err(|e| e.to_string()));
    accel_line(qemu, std::path::Path::new("/dev/kvm").exists())
}

/// [`accel_check`]'s verdict from what it found.
fn accel_line(qemu: std::result::Result<(std::path::PathBuf, (u32, u32)), String>, kvm: bool) -> (bool, String) {
    match qemu {
        Ok((path, (major, minor))) => {
            let kvm = if kvm { "; /dev/kvm is present, but emu has no x86_64 KVM backend yet" } else { "" };
            (
                true,
                format!("software mode: QEMU {major}.{minor} TCG at {} with emu's devices (no hardware acceleration{kvm})", path.display()),
            )
        }
        Err(why) => (false, why),
    }
}

pub fn boot(
    env: &emu_avd::Env,
    name: &str,
    opts: &args::Opts,
    signals: std::sync::mpsc::Receiver<i32>,
    _launched: std::time::Instant,
) -> Result<ExitCode> {
    let avd = env.load(name)?;
    // Before anything is created or started, as on a Mac: refusing takes milliseconds.
    if !instance::allow_old_images()
        && let Some(why) = instance::unsupported(&avd)
    {
        anyhow::bail!(why);
    }
    let qemu = emu_vmm::soft::find_qemu(std::env::var("EMU_QEMU").ok().as_deref(), &std::env::var("PATH").unwrap_or_default())
        .context(NEED_QEMU)?;
    let (major, minor) = emu_vmm::soft::check_qemu(&qemu)?;
    if opts.verbose {
        eprintln!("emu: software mode: QEMU {major}.{minor} at {}", qemu.display());
        if !opts.ignored.is_empty() {
            eprintln!("emu: accepted but not implemented yet: {}", opts.ignored.join(" "));
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::Builder::new().name("stop-watch".into()).spawn(move || {
            if signals.recv().is_ok() {
                stop.store(true, Ordering::SeqCst);
                // QEMU gets 10 s to quit (`emu_vmm::soft`); don't outlive that by much.
                std::thread::sleep(std::time::Duration::from_secs(15));
                eprintln!("emu: shutdown took over 15 s; exiting");
                std::process::exit(0);
            }
        })?;
    }

    let inst = instance::Instance::prepare(&avd, opts)?;
    let adb_port = inst.console_port + 1;
    let _ = std::thread::Builder::new().name("adb-announce".into()).spawn(move || {
        loop {
            std::thread::sleep(crate::ADB_REANNOUNCE);
            emu_vmm::pipe::adb::AdbService::announce_port(adb_port);
        }
    });
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
            network: inst.network.clone(),
            // Foldable postures aren't wired in software mode yet.
            fold: None,
            rotation_unknown: instance::rotation_unknown(&avd),
        },
    )?;
    let _advert = console::Advertisement::write(&avd.name, &avd.dir, inst.console_port);
    let _display = inst.display.clone().and_then(|d| display_server::start(&avd.dir, d, inst.inputs.clone(), stop.clone()));
    // On stdout: launchers watch for the first line and the `emulator-<port>` serial.
    println!("emu: {} on emulator-{} (adb port {adb_port})", avd.name, inst.console_port);
    let cfg = inst.soft_config(opts, qemu)?;
    match emu_vmm::soft::run(cfg, stop)? {
        emu_vmm::soft::Exit::PowerOff | emu_vmm::soft::Exit::Stopped => Ok(ExitCode::SUCCESS),
        emu_vmm::soft::Exit::Failed(why) => {
            eprintln!("emu: {why}");
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accel_check_says_what_runs_the_guest_or_what_is_missing() {
        let found = Ok((std::path::PathBuf::from("/q/qemu-system-x86_64"), (11, 1)));
        let (ok, line) = accel_line(found.clone(), false);
        assert!(ok);
        assert_eq!(line, "software mode: QEMU 11.1 TCG at /q/qemu-system-x86_64 with emu's devices (no hardware acceleration)");
        assert!(accel_line(found, true).1.contains("/dev/kvm is present, but emu has no x86_64 KVM backend yet"));
        let (ok, line) = accel_line(Err(NEED_QEMU.to_string()), false);
        assert!(!ok && line.contains("QEMU 11.1 or newer"), "{line}");
    }
}
