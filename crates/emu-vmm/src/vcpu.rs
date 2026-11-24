//! vCPU threads: entering the guest and handling its exits (MMIO, PSCI, WFI, sysreg traps).
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use emu_hvf::{Exit, Reg, SysReg, Vcpu, VcpuHandle};

use crate::bus::{Bus, ExitProfile, le_value};
use crate::wake::{self, VcpuControl};

const EC_WFX: u64 = 0x01;
const EC_HVC64: u64 = 0x16;
const EC_SMC64: u64 = 0x17;
const EC_SYSREG: u64 = 0x18;
const EC_IABT_LOWER: u64 = 0x20;
const EC_DABT_LOWER: u64 = 0x24;

/// EL1h with D, A, I and F masked: the state the arm64 boot protocol requires.
const CPSR_EL1H_DAIF: u64 = 0x3c5;
const VTIMER_PPI: u32 = 27;

mod psci {
    pub const VERSION: u32 = 0x8400_0000;
    pub const CPU_SUSPEND_32: u32 = 0x8400_0001;
    pub const CPU_SUSPEND_64: u32 = 0xc400_0001;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON_32: u32 = 0x8400_0003;
    pub const CPU_ON_64: u32 = 0xc400_0003;
    pub const AFFINITY_INFO_32: u32 = 0x8400_0004;
    pub const AFFINITY_INFO_64: u32 = 0xc400_0004;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;

    pub const SUCCESS: i64 = 0;
    pub const NOT_SUPPORTED: i64 = -1;
    pub const INVALID_PARAMS: i64 = -2;
    pub const ALREADY_ON: i64 = -4;
    pub const ON_PENDING: i64 = -5;
}

/// Why the machine stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    PowerOff,
    Reset,
    /// Host asked (signal, console `kill`).
    Requested,
    Fault(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Power {
    Off,
    Pending { entry: u64, ctx: u64 },
    On,
}

struct CpuSlot {
    power: Mutex<Power>,
    cv: Condvar,
}

/// What device threads use to rouse vCPUs after raising an interrupt (see [`wake`]).
pub type Wakers = wake::Wakers<HvVcpus>;

/// The framework side of [`Wakers`]: each vCPU's handle (registered by its thread) and the
/// GIC's SPI routing.
pub struct HvVcpus {
    handles: Vec<OnceLock<VcpuHandle>>,
}

impl HvVcpus {
    pub fn new(cpus: usize) -> HvVcpus {
        HvVcpus { handles: (0..cpus).map(|_| OnceLock::new()).collect() }
    }
}

impl VcpuControl for HvVcpus {
    fn kick(&self, cpus: u64) {
        let handles = self.handles.iter().enumerate().filter(|(i, _)| cpus & (1 << i) != 0);
        emu_hvf::kick_vcpus(handles.filter_map(|(_, h)| h.get().copied()));
    }

    fn spi_router(&self, intid: u32) -> Option<u64> {
        emu_hvf::Vm::spi_router(intid).ok()
    }
}

/// Per-vCPU exit counters, each vCPU's alone in its cache line (128 bytes on Apple silicon):
/// one is bumped on every exit, and neighbours sharing the line would bounce it between cores.
#[derive(Default)]
#[repr(align(128))]
pub struct ExitStats {
    pub mmio_read: AtomicU64,
    pub mmio_write: AtomicU64,
    pub wfi: AtomicU64,
    pub hvc: AtomicU64,
    pub sysreg: AtomicU64,
    pub canceled: AtomicU64,
    pub vtimer: AtomicU64,
}

impl ExitStats {
    pub fn snapshot(&self) -> [(&'static str, u64); 7] {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        [
            ("mmio_read", l(&self.mmio_read)),
            ("mmio_write", l(&self.mmio_write)),
            ("wfi", l(&self.wfi)),
            ("hvc", l(&self.hvc)),
            ("sysreg", l(&self.sysreg)),
            ("canceled", l(&self.canceled)),
            ("vtimer", l(&self.vtimer)),
        ]
    }
}

pub struct Cpus {
    pub bus: Bus,
    slots: Vec<CpuSlot>,
    wakers: Arc<Wakers>,
    pub stats: Vec<ExitStats>,
    vtimer_offset: u64,
    stopping: AtomicBool,
    stop: Mutex<Option<StopReason>>,
    stop_cv: Condvar,
    unmapped_logged: AtomicU64,
    sysreg_logged: SeenOnce,
    /// `EMU_EXIT_PROFILE`: per-device exit attribution and per-vCPU time (see [`ExitProfile`]).
    profile: Option<Arc<ExitProfile>>,
    profile_file: Option<PathBuf>,
}

impl Drop for Cpus {
    fn drop(&mut self) {
        if let Some(p) = &self.profile {
            eprint!("{}", p.summary());
            if let Some(path) = &self.profile_file {
                write_profile(p, path);
            }
        }
    }
}

/// Replaces `path` with the profile's dump, atomically, so a reader never sees half of one.
fn write_profile(p: &ExitProfile, path: &std::path::Path) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, p.dump()).and_then(|()| std::fs::rename(&tmp, path)).is_err() {
        eprintln!("emu: could not write the exit profile to {}", path.display());
    }
}

/// Rewrites the profile file every second until the machine (the last strong reference) goes.
fn spawn_profile_writer(p: &Arc<ExitProfile>, path: PathBuf) {
    let weak = Arc::downgrade(p);
    let _ = std::thread::Builder::new().name("exit-profile".into()).spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(p) = weak.upgrade() else { return };
            write_profile(&p, &path);
        }
    });
}

impl Cpus {
    pub fn new(bus: Bus, wakers: Arc<Wakers>, vtimer_offset: u64) -> Cpus {
        let n = wakers.len();
        let target = crate::bus::profile_target(std::env::var("EMU_EXIT_PROFILE").ok().as_deref());
        let profile = target.as_ref().map(|_| Arc::new(ExitProfile::new(&bus, n)));
        let profile_file = target.flatten();
        if let (Some(p), Some(path)) = (&profile, &profile_file) {
            spawn_profile_writer(p, path.clone());
        }
        Cpus {
            profile,
            profile_file,
            bus,
            slots: (0..n).map(|_| CpuSlot { power: Mutex::new(Power::Off), cv: Condvar::new() }).collect(),
            wakers,
            stats: (0..n).map(|_| ExitStats::default()).collect(),
            vtimer_offset,
            stopping: AtomicBool::new(false),
            stop: Mutex::new(None),
            stop_cv: Condvar::new(),
            unmapped_logged: AtomicU64::new(0),
            sysreg_logged: SeenOnce::new(),
        }
    }

    pub fn count(&self) -> usize {
        self.slots.len()
    }

    /// Records the first stop reason and forces every vCPU out of the guest.
    pub fn request_stop(&self, reason: StopReason) {
        {
            let mut s = self.stop.lock().unwrap_or_else(|e| e.into_inner());
            if s.is_none() {
                *s = Some(reason);
            }
            self.stopping.store(true, Ordering::SeqCst);
            self.stop_cv.notify_all();
        }
        self.wakers.wake_all();
        for slot in &self.slots {
            let _g = slot.power.lock().unwrap_or_else(|e| e.into_inner());
            slot.cv.notify_all();
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Blocks until a stop is requested or `timeout` passes.
    pub fn wait_stop(&self, timeout: Duration) -> Option<StopReason> {
        let s = self.stop.lock().unwrap_or_else(|e| e.into_inner());
        let s = self.stop_cv.wait_timeout_while(s, timeout, |s| s.is_none()).unwrap_or_else(|e| e.into_inner()).0;
        s.clone()
    }

    /// Body of vCPU thread `idx`. The boot CPU starts at `boot`; the rest wait for PSCI CPU_ON.
    pub fn thread_main(&self, idx: usize, boot: Option<(u64, u64)>) {
        if let Err(e) = self.thread_inner(idx, boot) {
            self.request_stop(StopReason::Fault(format!("vcpu{idx}: {e}")));
        }
    }

    fn thread_inner(&self, idx: usize, boot: Option<(u64, u64)>) -> Result<(), emu_hvf::Error> {
        let vcpu = Vcpu::new()?;
        vcpu.set_sys_reg(SysReg::MPIDR_EL1, idx as u64)?;
        vcpu.set_vtimer_offset(self.vtimer_offset)?;
        let sctlr_reset = vcpu.sys_reg(SysReg::SCTLR_EL1)?;
        let slot = &self.slots[idx];
        let _ = self.wakers.control().handles[idx].set(vcpu.handle());
        if let Some((entry, x0)) = boot {
            *slot.power.lock().unwrap_or_else(|e| e.into_inner()) = Power::Pending { entry, ctx: x0 };
        }
        loop {
            let (entry, ctx) = {
                let mut p = slot.power.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if self.is_stopping() {
                        return Ok(());
                    }
                    if let Power::Pending { entry, ctx } = *p {
                        *p = Power::On;
                        self.wakers.online(idx);
                        break (entry, ctx);
                    }
                    p = slot.cv.wait(p).unwrap_or_else(|e| e.into_inner());
                }
            };
            // Architectural reset state, then the PSCI/boot entry contract: x0 = context.
            for r in 0..=30 {
                vcpu.set_reg(Reg::x(r), 0)?;
            }
            vcpu.set_sys_reg(SysReg::SCTLR_EL1, sctlr_reset)?;
            vcpu.set_reg(Reg::x(0), ctx)?;
            vcpu.set_reg(Reg::PC, entry)?;
            vcpu.set_reg(Reg::CPSR, CPSR_EL1H_DAIF)?;
            match self.run(idx, &vcpu)? {
                Flow::CpuOff => {
                    self.wakers.offline(idx);
                    *slot.power.lock().unwrap_or_else(|e| e.into_inner()) = Power::Off;
                }
                Flow::Stop => return Ok(()),
            }
        }
    }

    fn run(&self, idx: usize, vcpu: &Vcpu) -> Result<Flow, emu_hvf::Error> {
        let stats = &self.stats[idx];
        let mut vtimer_masked = false;
        loop {
            if self.is_stopping() {
                return Ok(Flow::Stop);
            }
            if vtimer_masked {
                // Unmask once the guest has handled the tick (timer no longer asserting).
                let ctl = vcpu.sys_reg(SysReg::CNTV_CTL_EL0)?;
                if ctl & 0b111 != 0b101 {
                    vcpu.set_vtimer_mask(false)?;
                    vtimer_masked = false;
                }
            }
            let t = self.profile.as_ref().map(|p| {
                p.entered();
                Instant::now()
            });
            self.wakers.enter(idx);
            let exit = vcpu.run();
            self.wakers.leave(idx);
            if let (Some(p), Some(t)) = (&self.profile, t) {
                let c = p.cpu(idx);
                c.runs.fetch_add(1, Ordering::Relaxed);
                c.guest_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
            match exit? {
                Exit::Canceled => {
                    stats.canceled.fetch_add(1, Ordering::Relaxed);
                }
                Exit::VtimerActivated => {
                    // Only seen without an in-kernel GIC; raise the PPI ourselves.
                    stats.vtimer.fetch_add(1, Ordering::Relaxed);
                    vcpu.set_ppi_pending(VTIMER_PPI)?;
                    vtimer_masked = true;
                }
                Exit::Exception { syndrome, ipa, .. } => {
                    if let Some(flow) = self.exception(idx, vcpu, syndrome, ipa)? {
                        return Ok(flow);
                    }
                }
                Exit::Unknown(r) => {
                    self.request_stop(StopReason::Fault(format!("vcpu{idx}: unknown exit reason {r}")));
                    return Ok(Flow::Stop);
                }
            }
        }
    }

    fn advance_pc(vcpu: &Vcpu) -> Result<(), emu_hvf::Error> {
        let pc = vcpu.reg(Reg::PC)?;
        vcpu.set_reg(Reg::PC, pc + 4)
    }

    fn exception(&self, idx: usize, vcpu: &Vcpu, syndrome: u64, ipa: u64) -> Result<Option<Flow>, emu_hvf::Error> {
        let stats = &self.stats[idx];
        let ec = (syndrome >> 26) & 0x3f;
        let iss = syndrome & 0x01ff_ffff;
        match ec {
            EC_DABT_LOWER => {
                if (iss >> 24) & 1 == 0 {
                    let pc = vcpu.reg(Reg::PC)?;
                    self.request_stop(StopReason::Fault(format!(
                        "vcpu{idx}: data abort without syndrome at ipa {ipa:#x} (pc {pc:#x}, esr {syndrome:#x})"
                    )));
                    return Ok(Some(Flow::Stop));
                }
                let size = 1usize << ((iss >> 22) & 3);
                let sign_extend = (iss >> 21) & 1 == 1;
                let rt = ((iss >> 16) & 0x1f) as u32;
                let sixty_four = (iss >> 15) & 1 == 1;
                let write = (iss >> 6) & 1 == 1;
                let t = self.profile.as_ref().map(|_| Instant::now());
                if write {
                    stats.mmio_write.fetch_add(1, Ordering::Relaxed);
                    let v = if rt == 31 { 0 } else { vcpu.reg(Reg::x(rt))? };
                    let hit = self.bus.write_at(ipa, &v.to_le_bytes()[..size]);
                    if let (Some(p), Some(t)) = (&self.profile, t) {
                        p.record(idx, hit, true, t.elapsed().as_nanos() as u64);
                    }
                    if hit.is_none() {
                        self.log_unmapped(ipa, true);
                    }
                } else {
                    stats.mmio_read.fetch_add(1, Ordering::Relaxed);
                    let mut buf = [0u8; 8];
                    let hit = self.bus.read_at(ipa, &mut buf[..size]);
                    if let (Some(p), Some(t)) = (&self.profile, t) {
                        p.record(idx, hit, false, t.elapsed().as_nanos() as u64);
                    }
                    if hit.is_none() {
                        self.log_unmapped(ipa, false);
                    }
                    let mut v = le_value(&buf[..size]);
                    if sign_extend && size < 8 {
                        let shift = 64 - size * 8;
                        v = (((v << shift) as i64) >> shift) as u64;
                    }
                    if !sixty_four {
                        v &= 0xffff_ffff;
                    }
                    if rt != 31 {
                        vcpu.set_reg(Reg::x(rt), v)?;
                    }
                }
                Self::advance_pc(vcpu)?;
            }
            EC_HVC64 | EC_SMC64 => {
                stats.hvc.fetch_add(1, Ordering::Relaxed);
                // HVC leaves PC past the instruction already; SMC traps with PC on it.
                if ec == EC_SMC64 {
                    Self::advance_pc(vcpu)?;
                }
                return self.psci(idx, vcpu);
            }
            EC_WFX => {
                stats.wfi.fetch_add(1, Ordering::Relaxed);
                let is_wfe = iss & 1 == 1;
                Self::advance_pc(vcpu)?;
                if !is_wfe {
                    self.wfi(idx, vcpu)?;
                }
            }
            EC_SYSREG => {
                stats.sysreg.fetch_add(1, Ordering::Relaxed);
                // Registers the framework doesn't handle (debug, PMU, ...): RAZ/WI.
                let is_read = iss & 1 == 1;
                let rt = ((iss >> 5) & 0x1f) as u32;
                let enc = iss & !(0x1f << 5) & !1;
                self.log_sysreg(enc, is_read);
                if is_read && rt != 31 {
                    vcpu.set_reg(Reg::x(rt), 0)?;
                }
                Self::advance_pc(vcpu)?;
            }
            EC_IABT_LOWER => {
                let pc = vcpu.reg(Reg::PC)?;
                self.request_stop(StopReason::Fault(format!(
                    "vcpu{idx}: instruction fetch from unmapped ipa {ipa:#x} (pc {pc:#x}, esr {syndrome:#x})"
                )));
                return Ok(Some(Flow::Stop));
            }
            _ => {
                let pc = vcpu.reg(Reg::PC)?;
                self.request_stop(StopReason::Fault(format!(
                    "vcpu{idx}: unhandled exception class {ec:#x} (pc {pc:#x}, esr {syndrome:#x})"
                )));
                return Ok(Some(Flow::Stop));
            }
        }
        Ok(None)
    }

    /// Host-side WFI: waits for the timer deadline, an interrupt for this vCPU, or (while
    /// another vCPU could send an SGI, which never reaches the VMM) at most [`wake::SGI_POLL`].
    fn wfi(&self, idx: usize, vcpu: &Vcpu) -> Result<(), emu_hvf::Error> {
        let ctl = vcpu.sys_reg(SysReg::CNTV_CTL_EL0)?;
        let mut timer = None;
        if ctl & 1 == 1 && ctl & 2 == 0 {
            let cval = vcpu.sys_reg(SysReg::CNTV_CVAL_EL0)?;
            let now = emu_hvf::host_ticks().wrapping_sub(self.vtimer_offset);
            if cval <= now {
                return Ok(());
            }
            timer = Some(Duration::from_nanos(emu_hvf::ticks_to_nanos(cval - now)));
        }
        let t = self.profile.as_ref().map(|_| Instant::now());
        self.wakers.park(idx, timer);
        if let (Some(p), Some(t)) = (&self.profile, t) {
            p.cpu(idx).parked_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    fn psci(&self, idx: usize, vcpu: &Vcpu) -> Result<Option<Flow>, emu_hvf::Error> {
        let func = vcpu.reg(Reg::x(0))? as u32;
        let arg = |n| vcpu.reg(Reg::x(n));
        let ret: i64 = match func {
            psci::VERSION => 0x2, // PSCI 0.2, matching the device tree
            psci::CPU_SUSPEND_32 | psci::CPU_SUSPEND_64 => {
                self.wfi(idx, vcpu)?;
                psci::SUCCESS
            }
            psci::CPU_OFF => return Ok(Some(Flow::CpuOff)),
            psci::CPU_ON_32 | psci::CPU_ON_64 => {
                let target = (arg(1)? & 0xff) as usize;
                let (entry, ctx) = (arg(2)?, arg(3)?);
                match self.slots.get(target) {
                    None => psci::INVALID_PARAMS,
                    Some(slot) => {
                        let mut p = slot.power.lock().unwrap_or_else(|e| e.into_inner());
                        match *p {
                            Power::On => psci::ALREADY_ON,
                            Power::Pending { .. } => psci::ON_PENDING,
                            Power::Off => {
                                // Online before it runs: from now on it can send SGIs.
                                self.wakers.online(target);
                                *p = Power::Pending { entry, ctx };
                                slot.cv.notify_all();
                                psci::SUCCESS
                            }
                        }
                    }
                }
            }
            psci::AFFINITY_INFO_32 | psci::AFFINITY_INFO_64 => {
                let target = (arg(1)? & 0xff) as usize;
                match self.slots.get(target) {
                    None => psci::INVALID_PARAMS,
                    Some(slot) => match *slot.power.lock().unwrap_or_else(|e| e.into_inner()) {
                        Power::On => 0,
                        Power::Off => 1,
                        Power::Pending { .. } => 2,
                    },
                }
            }
            psci::MIGRATE_INFO_TYPE => 2, // no trusted OS
            psci::SYSTEM_OFF => {
                self.request_stop(StopReason::PowerOff);
                return Ok(Some(Flow::Stop));
            }
            psci::SYSTEM_RESET => {
                self.request_stop(StopReason::Reset);
                return Ok(Some(Flow::Stop));
            }
            _ => psci::NOT_SUPPORTED,
        };
        vcpu.set_reg(Reg::x(0), ret as u64)?;
        Ok(None)
    }

    fn log_unmapped(&self, ipa: u64, write: bool) {
        if self.unmapped_logged.fetch_add(1, Ordering::Relaxed) < 16 {
            eprintln!("emu: guest {} unmapped address {ipa:#x}", if write { "wrote" } else { "read" });
        }
    }

    fn log_sysreg(&self, enc: u64, is_read: bool) {
        if self.sysreg_logged.first_sighting(enc) {
            let (op0, op2, op1, crn, crm) = ((enc >> 20) & 3, (enc >> 17) & 7, (enc >> 14) & 7, (enc >> 10) & 0xf, (enc >> 1) & 0xf);
            eprintln!(
                "emu: trapped sysreg S{op0}_{op1}_C{crn}_C{crm}_{op2} ({}), treating as RAZ/WI",
                if is_read { "read" } else { "write" }
            );
        }
    }
}

/// The first [`SeenOnce::CAP`] distinct values, each reported once. A guest polling a trapped
/// register (PMU, debug) asks on every exit for a value already seen: that answer is a lock-free
/// scan; only a new value takes the lock.
struct SeenOnce {
    values: [AtomicU64; SeenOnce::CAP],
    /// Entries below this are written (published with Release after the write).
    len: AtomicUsize,
    insert: Mutex<()>,
}

impl SeenOnce {
    const CAP: usize = 64;

    fn new() -> SeenOnce {
        SeenOnce { values: std::array::from_fn(|_| AtomicU64::new(0)), len: AtomicUsize::new(0), insert: Mutex::new(()) }
    }

    fn contains(&self, value: u64, len: usize) -> bool {
        self.values[..len].iter().any(|v| v.load(Ordering::Relaxed) == value)
    }

    /// True the first time `value` is offered, while fewer than `CAP` values have been.
    fn first_sighting(&self, value: u64) -> bool {
        let len = self.len.load(Ordering::Acquire);
        if len >= Self::CAP || self.contains(value, len) {
            return false;
        }
        let _g = self.insert.lock().unwrap_or_else(|e| e.into_inner());
        let len = self.len.load(Ordering::Acquire);
        if len >= Self::CAP || self.contains(value, len) {
            return false;
        }
        self.values[len].store(value, Ordering::Relaxed);
        self.len.store(len + 1, Ordering::Release);
        true
    }
}

enum Flow {
    CpuOff,
    Stop,
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn each_value_is_seen_once_up_to_the_cap() {
        let s = SeenOnce::new();
        assert!(s.first_sighting(0), "zero is a value like any other");
        assert!(!s.first_sighting(0));
        for v in 1..SeenOnce::CAP as u64 {
            assert!(s.first_sighting(v), "{v}");
        }
        assert!(!s.first_sighting(5));
        assert!(!s.first_sighting(1000), "full");
    }

    /// vCPU threads trapping on the same registers at once: each is still reported once.
    #[test]
    fn racing_sightings_report_each_value_once() {
        let s = SeenOnce::new();
        let firsts: Vec<u64> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..8)
                .map(|_| sc.spawn(|| (0..40u64).flat_map(|_| 0..40u64).filter(|&v| s.first_sighting(v)).collect::<Vec<_>>()))
                .collect();
            hs.into_iter().flat_map(|h| h.join().expect("thread")).collect()
        });
        let mut sorted = firsts.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..40).collect::<Vec<_>>());
    }

    fn cpus(n: usize) -> Cpus {
        Cpus::new(Bus::new(), Arc::new(Wakers::new(n, HvVcpus::new(n), wake::Mode::Targeted)), 0)
    }

    /// Per-exit bookkeeping with N vCPU threads: each one's exit counter, and the sysreg trap
    /// log for an encoding already logged (the PMU and debug registers a guest polls).
    /// `cargo test --release -p emu-vmm exit_bookkeeping_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn exit_bookkeeping_cost() {
        const ITERS: u32 = 2_000_000;
        fn worst(c: &Cpus, f: impl Fn(&Cpus, usize) + Sync) -> f64 {
            let barrier = std::sync::Barrier::new(c.count());
            std::thread::scope(|s| {
                let hs: Vec<_> = (0..c.count())
                    .map(|i| {
                        let (barrier, f) = (&barrier, &f);
                        s.spawn(move || {
                            barrier.wait();
                            let t = Instant::now();
                            for _ in 0..ITERS {
                                f(c, i);
                            }
                            t.elapsed().as_nanos() as f64 / f64::from(ITERS)
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().expect("thread")).fold(0.0, f64::max)
            })
        }
        for n in [1usize, 2, 4, 8] {
            let c = cpus(n);
            c.log_sysreg(0x30_0000, true);
            let stats = worst(&c, |c, i| {
                c.stats[i].sysreg.fetch_add(1, Ordering::Relaxed);
            });
            let sysreg = worst(&c, |c, _| c.log_sysreg(0x30_0000, true));
            println!("exit_bookkeeping {n} vCPUs: {stats:.1} ns per counter bump, {sysreg:.1} ns per logged sysreg trap");
        }
    }
}
