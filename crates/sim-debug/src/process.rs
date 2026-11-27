//! The device's process tree, from what the host's kernel reports of each process (its libproc
//! record: `sim_coresim::procs`, which the caller reads and passes in, so this crate stays free of
//! FFI). The rows match what `ps -Ao pid=,ppid=,pcpu=,rss=,etime=,comm=` printed before, without
//! spawning it.

use serde::{Deserialize, Serialize};

/// One process running inside the device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    /// Percent of one CPU: the sum of its non-idle threads' scheduler CPU usage, a decaying average
    /// the kernel keeps per thread. The same figure `ps`'s `%cpu` printed (read the same way, so
    /// unchanged in meaning), above 100 when several threads run.
    pub cpu: f64,
    /// Resident memory in KiB.
    pub rss_kb: u64,
    /// Time since it started, as `ps`'s `etime` prints it (`[[dd-]hh:]mm:ss`).
    pub elapsed: String,
    pub name: String,
    /// The executable it runs, as the kernel resolved it (`proc_pidpath`). `ps`'s `comm` printed
    /// argv[0] here, which for about 5% of a device's processes was a bare name (`launchd_sim`),
    /// the `/var` form of a `/private/var` path, or an in-device path (`/usr/libexec/pkd`).
    pub path: String,
}

/// One process as the host's kernel reports it, before it is shown as a [`Process`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub pid: i32,
    pub ppid: i32,
    /// The sum of its threads' scheduler CPU usage, thousandths of a CPU (`TH_USAGE_SCALE`).
    pub cpu_usage: u32,
    pub resident_bytes: u64,
    /// Start time, seconds since the Unix epoch.
    pub start_s: u64,
    /// The executable's path.
    pub path: String,
}

impl Process {
    /// `sample` as a row, `now_s` seconds since the Unix epoch.
    pub fn from_sample(sample: &Sample, now_s: u64) -> Process {
        let name = sample.path.rsplit('/').next().unwrap_or(&sample.path).to_owned();
        Process {
            pid: sample.pid,
            ppid: sample.ppid,
            // Thousandths of a CPU to percent: exactly one decimal, as `ps` prints it.
            cpu: f64::from(sample.cpu_usage) / 10.0,
            rss_kb: sample.resident_bytes / 1024,
            elapsed: elapsed(now_s.saturating_sub(sample.start_s)),
            name,
            path: sample.path.clone(),
        }
    }
}

/// `secs` as `ps`'s `etime`: `mm:ss`, `hh:mm:ss` from an hour, `dd-hh:mm:ss` from a day.
pub fn elapsed(secs: u64) -> String {
    let (days, hours, mins, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if days > 0 {
        format!("{days:02}-{hours:02}:{mins:02}:{s:02}")
    } else if hours > 0 {
        format!("{hours:02}:{mins:02}:{s:02}")
    } else {
        format!("{mins:02}:{s:02}")
    }
}

/// The device's processes from samples of its tree (its `launchd_sim` and every descendant),
/// sorted by CPU then memory, descending, then by pid. Zombies have no task to sample, so unlike
/// `ps` they aren't listed.
pub fn tree(samples: &[Sample], now_s: u64) -> Vec<Process> {
    let mut out: Vec<Process> = samples.iter().map(|s| Process::from_sample(s, now_s)).collect();
    out.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.rss_kb.cmp(&a.rss_kb)).then(a.pid.cmp(&b.pid)));
    out
}

/// A process's `KERN_PROCARGS2` record, read: the executable path, its arguments and its
/// environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcArgs {
    pub exec_path: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
}

impl ProcArgs {
    /// Reads a `KERN_PROCARGS2` record: `argc` (a native-endian `i32`), the executable path, NUL
    /// padding, then `argc` arguments and the environment, each NUL-terminated (the environment
    /// ends at an empty string; the `apple[]` strings after it are left out). `None` when it is
    /// too short to hold `argc`.
    ///
    /// The kernel pads the path with NULs to a multiple of 8 bytes from its start (`exec_extract_
    /// strings`), so exactly that padding is skipped: an empty argv[0] stays an argument rather
    /// than being taken for padding.
    pub fn parse(buf: &[u8]) -> Option<ProcArgs> {
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?).max(0) as usize;
        let rest = &buf[4..];
        let path_end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
        let exec_path = String::from_utf8_lossy(&rest[..path_end]).into_owned();
        let after = (path_end + 1).min(rest.len());
        let aligned = (path_end + 1).next_multiple_of(8).min(rest.len());
        // Padding is all NULs; anything else means another layout: start right after the path.
        let start = if rest[after..aligned].iter().all(|b| *b == 0) { aligned } else { after };
        let mut strings = rest[start..].split(|b| *b == 0);
        let argv: Vec<String> = strings.by_ref().take(argc).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        let env = strings.take_while(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        Some(ProcArgs { exec_path, argv, env })
    }

    /// The value of the environment variable `key`.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.env.iter().find_map(|e| e.strip_prefix(key)?.strip_prefix('='))
    }

    /// Whether dyld was asked to insert `dylib` (`DYLD_INSERT_LIBRARIES`, colon-separated).
    pub fn inserts(&self, dylib: &str) -> bool {
        self.var("DYLD_INSERT_LIBRARIES").is_some_and(|v| v.split(':').any(|l| l == dylib))
    }
}

/// Parses `ps -Ao pid=,ppid=,pcpu=,rss=,etime=,comm=` output (what [`tree`] is checked against).
#[cfg(test)]
pub(crate) fn parse_ps(text: &str) -> Vec<Process> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cpu = it.next()?.parse().ok()?;
            let rss_kb = it.next()?.parse().ok()?;
            let elapsed = it.next()?.to_owned();
            let path = it.collect::<Vec<_>>().join(" ");
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            Some(Process { pid, ppid, cpu, rss_kb, elapsed, name, path })
        })
        .collect()
}

/// Samples as the fixtures record them: a `now <secs>` line, then one tab-separated
/// `pid ppid cpu_usage resident_bytes start_s path` line per process.
#[cfg(test)]
pub(crate) fn parse_samples(text: &str) -> (u64, Vec<Sample>) {
    let mut lines = text.lines();
    let now = lines.next().and_then(|l| l.strip_prefix("now ")?.parse().ok()).expect("a now line");
    let samples = lines
        .map(|l| {
            let f: Vec<&str> = l.splitn(6, '\t').collect();
            let n = |i: usize| f[i].parse::<u64>().expect("number");
            Sample {
                pid: n(0) as i32,
                ppid: n(1) as i32,
                cpu_usage: n(2) as u32,
                resident_bytes: n(3),
                start_s: n(4),
                path: f[5].to_owned(),
            }
        })
        .collect();
    (now, samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "  10     1  0.0  1000   01:00 /sbin/launchd\n  20    10  1.5  2000   00:30 /path/with space/launchd_sim\n  30    20 12.0  5000   00:10 /x/SpringBoard\n";

    #[test]
    fn parses_paths_with_spaces() {
        let ps = parse_ps(PS);
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[1].path, "/path/with space/launchd_sim");
        assert_eq!(ps[1].name, "launchd_sim");
        assert_eq!(ps[2].cpu, 12.0);
    }

    #[test]
    fn elapsed_is_formatted_like_ps() {
        assert_eq!(elapsed(0), "00:00");
        assert_eq!(elapsed(65), "01:05");
        assert_eq!(elapsed(3599), "59:59");
        assert_eq!(elapsed(3600), "01:00:00");
        assert_eq!(elapsed(86_399), "23:59:59");
        // As `ps` printed launchd's on this host: 6 days, 20:03:37.
        assert_eq!(elapsed(6 * 86_400 + 20 * 3600 + 3 * 60 + 37), "06-20:03:37");
        assert_eq!(elapsed(123 * 86_400), "123-00:00:00");
    }

    #[test]
    fn a_sample_becomes_a_row_and_rows_sort_by_cpu_then_memory_then_pid() {
        let s =
            |pid, cpu_usage, resident_bytes| Sample { pid, ppid: 1, cpu_usage, resident_bytes, start_s: 1000, path: format!("/x/p{pid}") };
        let rows = tree(&[s(5, 0, 4096), s(3, 1269, 1024), s(4, 0, 4096), s(2, 0, 8192)], 1065);
        assert_eq!(rows.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![3, 2, 4, 5]);
        assert_eq!(
            rows[0],
            Process { pid: 3, ppid: 1, cpu: 126.9, rss_kb: 1, elapsed: "01:05".into(), name: "p3".into(), path: "/x/p3".into() }
        );
        assert_eq!(rows[0].cpu, "126.9".parse::<f64>().expect("f64"), "the value ps printed parses to the same f64");
    }

    /// Recorded on an iPhone 17 Pro (iOS 27.2) on macOS 26.6: the device's tree read through
    /// libproc and, between two such reads that agreed on the set of pids, `ps` over the host.
    const SAMPLES: &str = include_str!("../fixtures/ios27.2-processes.libproc.tsv");
    const PS_RECORDED: &str = include_str!("../fixtures/ios27.2-processes.ps.txt");

    #[test]
    fn the_recorded_tree_matches_what_ps_printed() {
        let (now, samples) = parse_samples(SAMPLES);
        let ours = tree(&samples, now);
        assert!(ours.len() > 100, "{} processes", ours.len());
        let pids: std::collections::BTreeSet<i32> = ours.iter().map(|p| p.pid).collect();
        let ps: std::collections::HashMap<i32, Process> =
            parse_ps(PS_RECORDED).into_iter().filter(|p| pids.contains(&p.pid)).map(|p| (p.pid, p)).collect();
        assert_eq!(ps.len(), ours.len(), "every process of the tree was in ps's listing");
        let mut differ = 0;
        for p in &ours {
            let theirs = &ps[&p.pid];
            assert_eq!((p.ppid, &p.name), (theirs.ppid, &theirs.name), "{p:?}\n{theirs:?}");
            // `comm` is argv[0], which is mostly the executable's path. When it isn't, the path is
            // now the executable's (`proc_pidpath`) and ends with what ps printed: a bare name
            // (`launchd_sim`), the `/var` symlink for `/private/var`, or the in-device path
            // (`/usr/libexec/pkd`, which on the host names the host's own `pkd`).
            if p.path != theirs.path {
                differ += 1;
                assert!(p.path.ends_with(&format!("/{}", theirs.path.trim_start_matches('/'))), "{p:?}\n{theirs:?}");
            }
            // The same kernel figures ps reads (its %cpu is the threads' scheduler usage, its rss
            // the task's resident size), read within milliseconds: equal when recorded. ps's clock
            // can tick a second between the two.
            assert_eq!((p.cpu, p.rss_kb), (theirs.cpu, theirs.rss_kb), "{p:?}\n{theirs:?}");
            assert!(elapsed_s(&p.elapsed).abs_diff(elapsed_s(&theirs.elapsed)) <= 1, "{p:?}\n{theirs:?}");
        }
        // 16 of 330 when recorded.
        assert!(differ < ours.len() / 10, "{differ} paths differ");
    }

    fn elapsed_s(e: &str) -> u64 {
        let (days, rest) = e.split_once('-').map_or((0, e), |(d, r)| (d.parse().expect("days"), r));
        rest.split(':').fold(0, |acc, part| acc * 60 + part.parse::<u64>().expect("part")) + days * 86_400
    }

    #[test]
    fn procargs_are_split_into_path_arguments_and_environment() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(
            // 14 bytes of path, its NUL and one of padding: 16.
            b"/x/SpringBoard\0\0/x/SpringBoard\0-flag\0HOME=/h\0DYLD_INSERT_LIBRARIES=/a.dylib:/b.dylib\0\0executable_path=/x\0",
        );
        let a = ProcArgs::parse(&buf).expect("parse");
        assert_eq!(a.exec_path, "/x/SpringBoard");
        assert_eq!(a.argv, vec!["/x/SpringBoard", "-flag"]);
        assert_eq!(a.env, vec!["HOME=/h", "DYLD_INSERT_LIBRARIES=/a.dylib:/b.dylib"]);
        assert_eq!(a.var("HOME"), Some("/h"));
        assert_eq!(a.var("HOM"), None);
        assert!(a.inserts("/b.dylib") && a.inserts("/a.dylib"));
        assert!(!a.inserts("/a.dyli") && !a.inserts("/c.dylib"));
        assert_eq!(ProcArgs::parse(b"\x01\0"), None);
        let none = ProcArgs::parse(&0i32.to_ne_bytes()).expect("empty");
        assert_eq!(none, ProcArgs::default());
    }

    #[test]
    fn an_empty_argv0_is_an_argument_not_padding() {
        // `/bin/sleep` (10 bytes, NUL, 5 of padding) run with argv ["", "30"] and FOO=bar.
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/sleep\0\0\0\0\0\0\0\x330\0FOO=bar\0\0ptr_munge=\0");
        let a = ProcArgs::parse(&buf).expect("parse");
        assert_eq!(
            (a.exec_path.as_str(), a.argv.clone(), a.env.clone()),
            ("/bin/sleep", vec!["".to_owned(), "30".into()], vec!["FOO=bar".to_owned()])
        );
        // A path whose NUL ends on the 8-byte boundary has no padding.
        let mut buf = 1i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/aa\0\0FOO=bar\0\0");
        let a = ProcArgs::parse(&buf).expect("parse");
        assert_eq!((a.argv.clone(), a.var("FOO")), (vec![String::new()], Some("bar")));
    }

    /// SpringBoard's record on an iPhone 17 Pro (iOS 27.2) booted with simee's network shaper in
    /// its launchd environment (`network_shape {inject:"boot"}`), user paths shortened.
    const SPRINGBOARD_INJECTED: &[u8] = include_bytes!("../fixtures/ios27.2-springboard-injected.procargs");

    #[test]
    fn a_recorded_springboard_names_the_injected_shaper() {
        let a = ProcArgs::parse(SPRINGBOARD_INJECTED).expect("parse");
        assert!(a.exec_path.ends_with("/SpringBoard.app/SpringBoard"), "{}", a.exec_path);
        let dylib = a.var("DYLD_INSERT_LIBRARIES").expect("injected");
        assert!(dylib.contains("netshape"), "{dylib}");
        assert!(a.inserts(dylib));
        assert!(!a.inserts(&dylib.replace("netshape", "netshapf")), "an older build's path isn't this one");
    }
}
