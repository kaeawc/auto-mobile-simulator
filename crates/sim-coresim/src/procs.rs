//! Host processes read through libproc: the host's process table (pid, parent, short name), a
//! process's arguments, and its accumulated CPU time split by QoS. A simulator's processes are
//! ordinary host processes under its `launchd_sim`, so a settle wait reads the table (one cheap
//! syscall per host process) and the CPU of its own device's processes only, instead of spawning
//! a host-wide `ps` (~38 ms of CPU per listing of ~1200 processes, measured by `poll_cost`).
//!
//! Every call is a plain query of the kernel: a process that has exited (or that this user may not
//! inspect) is `None` or left out, never an error.

use std::ffi::c_void;
use std::sync::OnceLock;

/// A process's accumulated CPU time, nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// User and system time.
    pub cpu_ns: u64,
    /// The part of it that ran at the maintenance or background QoS (what a launchd job with
    /// `ProcessType` Background, or a darwinbg task, runs at: priority 4 on the efficiency cores).
    pub background_ns: u64,
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    // libc's binding is deprecated in favour of the mach2 crate; this one call isn't worth it.
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

/// Mach absolute time units to nanoseconds (1:1 on Intel, 125:3 on Apple silicon).
fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = Timebase { numer: 0, denom: 0 };
        // SAFETY: `info` is a valid, writable mach_timebase_info_data_t for the call's duration.
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc == 0 && info.numer != 0 && info.denom != 0 { (u64::from(info.numer), u64::from(info.denom)) } else { (1, 1) }
    })
}

fn to_ns(ticks: u64) -> u64 {
    let (numer, denom) = timebase();
    // In u128 so `ticks * numer` can't overflow.
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

/// The accumulated CPU time of `pid`, `None` when it can't be read (exited, or not ours).
pub fn usage(pid: i32) -> Option<Usage> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a writable rusage_info_v4, the size the V4 flavor fills; the kernel writes
    // at most that many bytes. libc types the buffer as `*mut rusage_info_t` (itself a pointer).
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: zero-initialised and filled by a successful call; every field is a plain integer.
    let info = unsafe { info.assume_init() };
    Some(Usage {
        cpu_ns: to_ns(info.ri_user_time.saturating_add(info.ri_system_time)),
        background_ns: to_ns(info.ri_cpu_time_qos_maintenance.saturating_add(info.ri_cpu_time_qos_background)),
    })
}

/// Every pid on the host.
pub fn all() -> Vec<i32> {
    // SAFETY: a null buffer asks only for the number of pids.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids: Vec<i32> = Vec::with_capacity(estimate.max(0) as usize + 256);
    let bytes = (pids.capacity() * size_of::<i32>()).min(i32::MAX as usize) as i32;
    // SAFETY: the buffer is `pids`'s spare capacity, `bytes` long; the kernel writes at most that
    // and returns the number of pids written.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    if n > 0 {
        // SAFETY: the first `n` pids were written, and `n` is at most the capacity.
        unsafe { pids.set_len((n as usize).min(pids.capacity())) };
    }
    pids
}

/// One row of the host's process table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// `p_comm`, up to 16 bytes, NUL-padded ("SpringBoard", "launchd_sim").
    comm: [u8; 16],
}

impl Proc {
    /// The short name (`p_comm`, up to 16 bytes).
    pub fn name(&self) -> &str {
        let end = self.comm.iter().position(|b| *b == 0).unwrap_or(self.comm.len());
        std::str::from_utf8(&self.comm[..end]).unwrap_or("")
    }
}

/// `pid`'s row: parent and short name.
pub fn proc(pid: i32) -> Option<Proc> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = size_of::<libc::proc_bsdshortinfo>() as i32;
    // SAFETY: `info` is a writable proc_bsdshortinfo, `size` bytes, what this flavor fills.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: zeroed and filled by a call that wrote the whole struct; every field is plain data.
    let info = unsafe { info.assume_init() };
    let mut comm = [0u8; 16];
    for (d, s) in comm.iter_mut().zip(info.pbsi_comm.iter()) {
        *d = *s as u8;
    }
    Some(Proc { pid, ppid: info.pbsi_ppid as i32, comm })
}

/// The host's process table (processes that exit while it is read are left out).
pub fn list() -> Vec<Proc> {
    all().into_iter().filter_map(proc).collect()
}

/// The process's arguments, NUL-separated, from `KERN_PROCARGS2` (after `argc`: the executable
/// path, then argv). `None` when the process has exited or can't be inspected.
pub fn args(pid: i32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut len: libc::size_t = 0;
    // SAFETY: a null old pointer asks for the size only; `mib` and `len` are valid.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len < size_of::<i32>() {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is `len` bytes, writable; the kernel writes at most `len` and updates it.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    Some(buf.split_off(size_of::<i32>().min(buf.len())))
}

/// The `launchd_sim` of the device whose data directory contains `udid` (its arguments name the
/// device's `launchd_bootstrap.plist`), if one is running.
pub fn launchd_sim(udid: &str) -> Option<i32> {
    let needle = format!("/{udid}/");
    all().into_iter().find(|&pid| is_launchd_sim(pid) && args(pid).is_some_and(|a| contains(&a, needle.as_bytes())))
}

/// Whether `pid` is a `launchd_sim` (a cached root is checked with this before reuse).
pub fn is_launchd_sim(pid: i32) -> bool {
    proc(pid).is_some_and(|p| p.name() == "launchd_sim")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy(ms: u64) {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut x = 0u64;
        while std::time::Instant::now() < end {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
        }
    }

    fn cpu_s(who: i32) -> f64 {
        let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: a writable rusage for RUSAGE_SELF or RUSAGE_CHILDREN.
        assert_eq!(unsafe { libc::getrusage(who, ru.as_mut_ptr()) }, 0);
        // SAFETY: filled by the successful call.
        let ru = unsafe { ru.assume_init() };
        let tv = |t: libc::timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }

    #[test]
    fn own_usage_matches_getrusage() {
        busy(200);
        let u = usage(std::process::id() as i32).expect("own usage");
        let rusage_s = cpu_s(libc::RUSAGE_SELF);
        let ours = u.cpu_ns as f64 / 1e9;
        assert!(ours >= 0.15, "{ours} s after 200 ms busy");
        // Both count the same CPU in different units: within 20 % (other test threads run too).
        assert!((ours - rusage_s).abs() <= 0.2 * rusage_s.max(0.2), "libproc {ours} s, getrusage {rusage_s} s");
        assert!(u.background_ns <= u.cpu_ns);
    }

    #[test]
    fn a_spawned_child_is_in_the_table_under_us() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().expect("sleep");
        let pid = child.id() as i32;
        let me = std::process::id() as i32;
        let row = list().into_iter().find(|p| p.pid == pid).expect("listed");
        assert_eq!((row.ppid, row.name()), (me, "sleep"));
        assert_eq!(proc(pid), Some(row));
        let a = args(pid).expect("args");
        assert!(contains(&a, b"/bin/sleep\0") && contains(&a, b"\x005"), "{:?}", String::from_utf8_lossy(&a));
        assert!(usage(pid).is_some());
        assert!(!is_launchd_sim(pid));
        child.kill().ok();
        child.wait().ok();
        assert_eq!(proc(pid), None, "an exited process has no row");
        assert_eq!(usage(pid), None, "an exited process has no usage");
    }

    #[test]
    fn the_table_has_every_process_and_no_device_is_found_for_a_made_up_udid() {
        let table = list();
        let me = std::process::id() as i32;
        assert!(table.iter().any(|p| p.pid == 1 && p.name() == "launchd"), "{} rows", table.len());
        assert!(table.iter().any(|p| p.pid == me));
        assert_eq!(launchd_sim("00000000-0000-0000-0000-000000000000"), None);
    }

    #[test]
    fn a_name_stops_at_the_first_nul_and_a_needle_is_found_only_whole() {
        let mut comm = [0u8; 16];
        comm[..11].copy_from_slice(b"SpringBoard");
        assert_eq!(Proc { pid: 1, ppid: 0, comm }.name(), "SpringBoard");
        assert_eq!(Proc { pid: 1, ppid: 0, comm: *b"sixteen-chars-xx" }.name(), "sixteen-chars-xx");
        assert!(contains(b"a /U/ b", b"/U/"));
        assert!(!contains(b"a /U b", b"/U/"));
        assert!(!contains(b"abc", b""));
    }

    /// Host CPU of one settle poll: the host-wide `ps -Ao pid=,ppid=,time=,args=` the settle wait
    /// used to spawn, against what it does now (the process table, then the CPU of a device's
    /// processes; here the CPU of every process this user can read, an upper bound).
    /// `cargo test -p sim-coresim --release poll_cost -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "a benchmark: spawns ps 20 times"]
    fn poll_cost() {
        let n = 20;
        let per = |s0: f64, t0: std::time::Instant| {
            ((cpu_s(libc::RUSAGE_SELF) - s0) * 1e3 / f64::from(n), t0.elapsed().as_secs_f64() * 1e3 / f64::from(n))
        };

        let (s0, c0, t0) = (cpu_s(libc::RUSAGE_SELF), cpu_s(libc::RUSAGE_CHILDREN), std::time::Instant::now());
        let mut lines = 0;
        for _ in 0..n {
            let out = std::process::Command::new("/bin/ps").args(["-Ao", "pid=,ppid=,time=,args="]).output().expect("ps");
            lines = out.stdout.split(|b| *b == b'\n').count();
        }
        let ps_cpu = (cpu_s(libc::RUSAGE_SELF) - s0 + cpu_s(libc::RUSAGE_CHILDREN) - c0) * 1e3 / f64::from(n);
        let ps_wall = t0.elapsed().as_secs_f64() * 1e3 / f64::from(n);

        let (s0, t0) = (cpu_s(libc::RUSAGE_SELF), std::time::Instant::now());
        let mut rows = 0;
        for _ in 0..n {
            rows = std::hint::black_box(list()).len();
        }
        let (table_cpu, table_wall) = per(s0, t0);

        let pids = all();
        let (s0, t0) = (cpu_s(libc::RUSAGE_SELF), std::time::Instant::now());
        let mut read = 0;
        for _ in 0..n {
            read = pids.iter().filter(|p| usage(**p).is_some()).count();
        }
        let (usage_cpu, usage_wall) = per(s0, t0);

        let (s0, t0) = (cpu_s(libc::RUSAGE_SELF), std::time::Instant::now());
        for _ in 0..n {
            assert_eq!(launchd_sim("00000000-0000-0000-0000-000000000000"), None);
        }
        let (scan_cpu, scan_wall) = per(s0, t0);
        eprintln!(
            "ps listing ({lines} lines): {ps_cpu:.2} ms CPU, {ps_wall:.2} ms wall per poll\n\
             process table ({rows} rows): {table_cpu:.3} ms CPU, {table_wall:.3} ms wall per poll\n\
             CPU of {read} processes: {usage_cpu:.3} ms CPU, {usage_wall:.3} ms wall ({:.2} µs each)\n\
             launchd_sim scan (once per wait): {scan_cpu:.3} ms CPU, {scan_wall:.3} ms wall",
            usage_cpu * 1e3 / read.max(1) as f64,
        );
    }

    /// CPU that ran at background QoS (`taskpolicy -b`, darwinbg) is counted as background; CPU at
    /// the default QoS isn't. Host-only and timing-dependent, so not in the default run.
    #[test]
    #[ignore = "spins two host processes for a second each"]
    fn background_cpu_is_attributed_to_background_qos() {
        let spin = |bg: bool| {
            let mut cmd = if bg {
                let mut c = std::process::Command::new("/usr/sbin/taskpolicy");
                c.args(["-b", "/usr/bin/yes"]);
                c
            } else {
                std::process::Command::new("/usr/bin/yes")
            };
            let mut child = cmd.stdout(std::process::Stdio::null()).spawn().expect("spawn");
            std::thread::sleep(std::time::Duration::from_secs(1));
            let u = usage(child.id() as i32).expect("usage");
            child.kill().ok();
            child.wait().ok();
            u
        };
        let fg = spin(false);
        let bg = spin(true);
        eprintln!("default: {fg:?}\nbackground: {bg:?}");
        assert!(fg.cpu_ns > 100_000_000 && fg.background_ns < fg.cpu_ns / 10, "{fg:?}");
        assert!(bg.cpu_ns > 10_000_000 && bg.background_ns > bg.cpu_ns * 9 / 10, "{bg:?}");
    }
}
