//! What every hypervisor backend's vCPU threads share: the stop protocol, exit counters and
//! profile, MMIO dispatch onto the [`Bus`], and decoding the trapped accesses (backend-free, so
//! it builds and tests on any unix host). The run loops themselves are in [`crate::hv`].
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::bus::{Bus, ExitProfile, le_value};

/// Why the machine stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    PowerOff,
    Reset,
    /// Host asked (signal, console `kill`).
    Requested,
    Fault(String),
}

/// How [`Cpus::request_stop`] reaches vCPUs: the backend forces each one out of the guest and
/// out of any host-side wait, so it sees the stop. Callable from any thread.
pub trait ForceExit: Send + Sync {
    fn force_exit_all(&self);
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
    count: usize,
    force: Arc<dyn ForceExit>,
    pub stats: Arc<[ExitStats]>,
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
    /// `count` vCPUs on `bus`; `force` is the backend's way to force them out of the guest.
    pub fn new(bus: Bus, count: usize, force: Arc<dyn ForceExit>) -> Cpus {
        let target = crate::bus::profile_target(std::env::var("EMU_EXIT_PROFILE").ok().as_deref());
        let profile = target.as_ref().map(|_| Arc::new(ExitProfile::new(&bus, count)));
        let profile_file = target.flatten();
        if let (Some(p), Some(path)) = (&profile, &profile_file) {
            spawn_profile_writer(p, path.clone());
        }
        Cpus {
            profile,
            profile_file,
            bus,
            count,
            force,
            stats: (0..count).map(|_| ExitStats::default()).collect(),
            stopping: AtomicBool::new(false),
            stop: Mutex::new(None),
            stop_cv: Condvar::new(),
            unmapped_logged: AtomicU64::new(0),
            sysreg_logged: SeenOnce::new(),
        }
    }

    pub fn count(&self) -> usize {
        self.count
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
        self.force.force_exit_all();
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

    /// Ends vCPU thread `idx`: a backend error stops the machine with it as the fault.
    pub fn finish<E: std::fmt::Display>(&self, idx: usize, result: Result<(), E>) {
        if let Err(e) = result {
            self.request_stop(StopReason::Fault(format!("vcpu{idx}: {e}")));
        }
    }

    /// Call right before vCPU `idx` enters the guest; hand the result to [`Cpus::left_guest`].
    pub fn entering_guest(&self) -> Option<Instant> {
        self.profile.as_ref().map(|p| {
            p.entered();
            Instant::now()
        })
    }

    /// vCPU `idx` is back from the guest it entered at `entered` ([`Cpus::entering_guest`]).
    pub fn left_guest(&self, idx: usize, entered: Option<Instant>) {
        if let (Some(p), Some(t)) = (&self.profile, entered) {
            let c = p.cpu(idx);
            c.runs.fetch_add(1, Ordering::Relaxed);
            c.guest_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Times a host-side wait of vCPU `idx` (WFI, PSCI CPU_SUSPEND) for the profile.
    pub fn parked<T>(&self, idx: usize, wait: impl FnOnce() -> T) -> T {
        let t = self.profile.as_ref().map(|_| Instant::now());
        let r = wait();
        if let (Some(p), Some(t)) = (&self.profile, t) {
            p.cpu(idx).parked_ns.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        r
    }

    /// A guest store of `data` (little-endian, 1 to 8 bytes) to device address `ipa` by vCPU `idx`.
    pub fn mmio_write(&self, idx: usize, ipa: u64, data: &[u8]) {
        self.stats[idx].mmio_write.fetch_add(1, Ordering::Relaxed);
        let t = self.profile.as_ref().map(|_| Instant::now());
        let hit = self.bus.write_at(ipa, data);
        if let (Some(p), Some(t)) = (&self.profile, t) {
            p.record(idx, hit, true, t.elapsed().as_nanos() as u64);
        }
        if hit.is_none() {
            self.log_unmapped(ipa, true);
        }
    }

    /// A guest load of `data.len()` bytes from device address `ipa` by vCPU `idx`; unmapped
    /// addresses read as zeroes.
    pub fn mmio_read(&self, idx: usize, ipa: u64, data: &mut [u8]) {
        self.stats[idx].mmio_read.fetch_add(1, Ordering::Relaxed);
        let t = self.profile.as_ref().map(|_| Instant::now());
        let hit = self.bus.read_at(ipa, data);
        if let (Some(p), Some(t)) = (&self.profile, t) {
            p.record(idx, hit, false, t.elapsed().as_nanos() as u64);
        }
        if hit.is_none() {
            data.fill(0);
            self.log_unmapped(ipa, false);
        }
    }

    fn log_unmapped(&self, ipa: u64, write: bool) {
        if self.unmapped_logged.fetch_add(1, Ordering::Relaxed) < 16 {
            eprintln!("emu: guest {} unmapped address {ipa:#x}", if write { "wrote" } else { "read" });
        }
    }

    /// Reports a trapped system register (ESR ISS encoding, Rt and direction masked off) the
    /// first time it is seen.
    pub fn log_sysreg(&self, enc: u64, is_read: bool) {
        if self.sysreg_logged.first_sighting(enc) {
            let (op0, op2, op1, crn, crm) = ((enc >> 20) & 3, (enc >> 17) & 7, (enc >> 14) & 7, (enc >> 10) & 0xf, (enc >> 1) & 0xf);
            eprintln!(
                "emu: trapped sysreg S{op0}_{op1}_C{crn}_C{crm}_{op2} ({}), treating as RAZ/WI",
                if is_read { "read" } else { "write" }
            );
        }
    }
}

/// A data abort's instruction syndrome (ESR_EL2 ISS with ISV set): what a trapped load or store
/// did, for backends that decode MMIO themselves (Hypervisor.framework).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataAbort {
    /// Access size in bytes: 1, 2, 4 or 8.
    pub size: usize,
    pub sign_extend: bool,
    /// The transfer register; 31 is XZR.
    pub rt: u32,
    /// The load targets a 64-bit register (else the result is zero-extended from 32 bits).
    pub sixty_four: bool,
    pub write: bool,
}

impl DataAbort {
    /// Decodes `iss`; `None` without a valid syndrome (ISV clear: a load/store pair, say).
    pub fn decode(iss: u64) -> Option<DataAbort> {
        if (iss >> 24) & 1 == 0 {
            return None;
        }
        Some(DataAbort {
            size: 1usize << ((iss >> 22) & 3),
            sign_extend: (iss >> 21) & 1 == 1,
            rt: ((iss >> 16) & 0x1f) as u32,
            sixty_four: (iss >> 15) & 1 == 1,
            write: (iss >> 6) & 1 == 1,
        })
    }

    /// The register value a load of `data` (little-endian, `size` bytes) leaves in Rt.
    pub fn load_value(&self, data: &[u8]) -> u64 {
        let mut v = le_value(&data[..self.size]);
        if self.sign_extend && self.size < 8 {
            let shift = 64 - self.size * 8;
            v = (((v << shift) as i64) >> shift) as u64;
        }
        if !self.sixty_four {
            v &= 0xffff_ffff;
        }
        v
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

/// A [`ForceExit`] with no vCPUs to force (tests).
#[cfg(test)]
pub(crate) struct NoVcpus;

#[cfg(test)]
impl ForceExit for NoVcpus {
    fn force_exit_all(&self) {}
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
        Cpus::new(Bus::new(), n, Arc::new(NoVcpus))
    }

    /// ESR ISS for a data abort with a valid syndrome.
    fn iss(size_log2: u64, sign_extend: bool, rt: u64, sixty_four: bool, write: bool) -> u64 {
        1 << 24 | size_log2 << 22 | u64::from(sign_extend) << 21 | rt << 16 | u64::from(sixty_four) << 15 | u64::from(write) << 6
    }

    #[test]
    fn data_aborts_decode_size_register_and_direction() {
        // `str w3, [x0]`
        assert_eq!(
            DataAbort::decode(iss(2, false, 3, false, true)),
            Some(DataAbort { size: 4, sign_extend: false, rt: 3, sixty_four: false, write: true })
        );
        // `ldrsb x30, [x1]`
        let ldrsb = DataAbort::decode(iss(0, true, 30, true, false)).expect("valid");
        assert_eq!((ldrsb.size, ldrsb.rt, ldrsb.write), (1, 30, false));
        // No valid syndrome (ISV clear): the VMM can't emulate it.
        assert_eq!(DataAbort::decode(iss(2, false, 3, false, true) & !(1 << 24)), None);
    }

    #[test]
    fn loads_sign_or_zero_extend_into_the_register() {
        let load = |size_log2, sign, sixty_four, data: &[u8]| {
            DataAbort::decode(iss(size_log2, sign, 0, sixty_four, false)).expect("valid").load_value(data)
        };
        assert_eq!(load(0, true, true, &[0x80]), 0xffff_ffff_ffff_ff80, "ldrsb x");
        assert_eq!(load(0, true, false, &[0x80]), 0xffff_ff80, "ldrsb w");
        assert_eq!(load(1, false, true, &[0x34, 0x92]), 0x9234, "ldrh");
        assert_eq!(load(2, true, true, &[0, 0, 0, 0x80]), 0xffff_ffff_8000_0000, "ldrsw");
        assert_eq!(load(3, false, true, &[1, 2, 3, 4, 5, 6, 7, 8]), 0x0807_0605_0403_0201, "ldr x");
        // Bytes past the access size are ignored.
        assert_eq!(load(1, false, true, &[1, 2, 0xff, 0xff, 0, 0, 0, 0]), 0x0201);
    }

    /// A register that remembers the last write and reads back its offset.
    #[derive(Default)]
    struct Reg(Mutex<Vec<(u64, Vec<u8>)>>);

    impl crate::bus::MmioDevice for Reg {
        fn read(&self, offset: u64, data: &mut [u8]) {
            data.copy_from_slice(&offset.to_le_bytes()[..data.len()]);
        }
        fn write(&self, offset: u64, data: &[u8]) {
            self.0.lock().expect("lock").push((offset, data.to_vec()));
        }
    }

    #[test]
    fn mmio_goes_to_the_device_and_is_counted_per_vcpu() {
        let reg = Arc::new(Reg::default());
        let mut bus = Bus::new();
        bus.insert(0x900_0000, 0x1000, reg.clone());
        let c = Cpus::new(bus, 2, Arc::new(NoVcpus));
        c.mmio_write(1, 0x900_0010, &[1, 2, 3, 4]);
        let mut buf = [0xaa; 4];
        c.mmio_read(1, 0x900_0024, &mut buf);
        assert_eq!(buf, [0x24, 0, 0, 0]);
        // Unmapped: reads as zero, writes go nowhere; both still count.
        let mut buf = [0xaa; 2];
        c.mmio_read(0, 0x10, &mut buf);
        assert_eq!(buf, [0, 0]);
        c.mmio_write(0, 0x10, &[9]);
        assert_eq!(*reg.0.lock().expect("lock"), [(0x10, vec![1, 2, 3, 4])]);
        let counts = |i: usize| (c.stats[i].mmio_read.load(Ordering::Relaxed), c.stats[i].mmio_write.load(Ordering::Relaxed));
        assert_eq!((counts(0), counts(1)), ((1, 1), (1, 1)));
    }

    #[derive(Default)]
    struct Forced(AtomicU64);

    impl ForceExit for Forced {
        fn force_exit_all(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_stop_keeps_the_first_reason_and_forces_the_vcpus_out_each_time() {
        let forced = Arc::new(Forced::default());
        let c = Cpus::new(Bus::new(), 1, forced.clone());
        assert_eq!(c.wait_stop(Duration::from_millis(1)), None);
        assert!(!c.is_stopping());
        c.finish(0, Ok::<(), String>(()));
        assert!(!c.is_stopping(), "a vCPU thread that ends cleanly doesn't stop the machine");
        c.finish(0, Err("hv_vcpu_run failed"));
        c.request_stop(StopReason::PowerOff);
        assert!(c.is_stopping());
        assert_eq!(c.wait_stop(Duration::ZERO), Some(StopReason::Fault("vcpu0: hv_vcpu_run failed".into())));
        assert_eq!(forced.0.load(Ordering::SeqCst), 2);
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
