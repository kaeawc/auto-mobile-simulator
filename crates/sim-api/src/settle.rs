//! When a booted simulator has settled, and how many to boot at once: pure logic over `ps`
//! output and clocks, so it is tested on any host.
//!
//! A device has finished booting (the service's terminal boot status, a display surface) long
//! before it is quiet: a first boot migrates data and indexes for a minute or more, and every boot
//! starts a few hundred daemons. A farm wants the moment the device stops competing for the host,
//! so "settled" here is: the device's whole process tree (its `launchd_sim` and every descendant)
//! used less than [`Rule::cpu_pct`] percent of one core over the last [`Rule::window`], with
//! SpringBoard running. CPU is read as each process's accumulated CPU time (`ps -o time`), so a
//! process that exits doesn't make the tree look quieter than it was, and the figure is exact over
//! the window rather than `ps`'s decaying `%cpu` average.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

/// The `ps` invocation [`parse_ps`] reads.
pub const PS_ARGS: &[&str] = &["-Ao", "pid=,ppid=,time=,args="];

/// One process from `ps -Ao pid=,ppid=,time=,args=`.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub pid: i32,
    pub ppid: i32,
    /// Accumulated CPU time, seconds.
    pub cpu_s: f64,
    pub args: String,
}

/// Seconds in a `ps` `time` column: `M:SS.ss`, `H:MM:SS.ss` or `D-HH:MM:SS`.
pub fn cpu_seconds(t: &str) -> Option<f64> {
    let (days, rest) = match t.split_once('-') {
        Some((d, rest)) => (d.parse::<f64>().ok()?, rest),
        None => (0.0, t),
    };
    let mut total = 0.0;
    for part in rest.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + total)
}

/// Parses `ps -Ao pid=,ppid=,time=,args=` output; lines that don't parse are skipped.
pub fn parse_ps(text: &str) -> Vec<Row> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cpu_s = cpu_seconds(it.next()?)?;
            let args = it.collect::<Vec<_>>().join(" ");
            Some(Row { pid, ppid, cpu_s, args })
        })
        .collect()
}

/// One device's process tree at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tree {
    /// Accumulated CPU seconds by pid.
    pub cpu: HashMap<i32, f64>,
    /// Whether SpringBoard (the home screen and the process that answers touches) is running.
    pub springboard: bool,
}

impl Tree {
    pub fn procs(&self) -> usize {
        self.cpu.len()
    }
}

/// The process tree of each device in `udids`, from one `ps` listing: `None` for a device with no
/// `launchd_sim` (not booted, or not yet). A device's `launchd_sim` names its data directory, which
/// contains its UDID.
pub fn trees(rows: &[Row], udids: &[&str]) -> Vec<Option<Tree>> {
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        children.entry(r.ppid).or_default().push(i);
    }
    udids
        .iter()
        .map(|udid| {
            let needle = format!("/{udid}/");
            let root = rows.iter().position(|r| is_launchd_sim(&r.args) && r.args.contains(&needle))?;
            let mut tree = Tree::default();
            let mut todo = vec![root];
            while let Some(i) = todo.pop() {
                let r = &rows[i];
                if tree.cpu.insert(r.pid, r.cpu_s).is_some() {
                    continue;
                }
                // The runtime's path has spaces ("iOS 27.2.simruntime"): match the bundle, not argv[0].
                tree.springboard |= r.args.contains("/SpringBoard.app/SpringBoard");
                if r.pid != r.ppid {
                    todo.extend(children.get(&r.pid).into_iter().flatten().copied());
                }
            }
            Some(tree)
        })
        .collect()
}

fn program(args: &str) -> &str {
    args.split_whitespace().next().unwrap_or("")
}

fn is_launchd_sim(args: &str) -> bool {
    let p = program(args);
    p == "launchd_sim" || p.ends_with("/launchd_sim")
}

/// What counts as settled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rule {
    /// The tree's CPU use, percent of one core, must stay under this…
    pub cpu_pct: f64,
    /// …over a window this long.
    pub window: Duration,
}

impl Default for Rule {
    /// 10 % of one core over 10 s. A booted iPhone 17 (iOS 27.2) idles at 1-6 % with no client
    /// attached; its first boot runs at 100-500 % for a minute or more, a later boot for 10-20 s.
    fn default() -> Self {
        Rule { cpu_pct: 10.0, window: Duration::from_secs(10) }
    }
}

/// Watches one device's samples and says when it has settled.
#[derive(Debug)]
pub struct Settler {
    rule: Rule,
    last: Option<(f64, Tree)>,
    /// (seconds, CPU seconds) per interval between samples, newest last, spanning at most the
    /// window.
    intervals: VecDeque<(f64, f64)>,
}

impl Settler {
    pub fn new(rule: Rule) -> Settler {
        Settler { rule, last: None, intervals: VecDeque::new() }
    }

    /// Adds a sample taken at `t` seconds (any monotonic origin); true once the device is settled.
    pub fn push(&mut self, t: f64, tree: Tree) -> bool {
        let springboard = tree.springboard;
        if let Some((t0, prev)) = self.last.take() {
            let dt = t - t0;
            if dt > 0.0 {
                // A process seen in both samples adds what it used since; a new one, all it has
                // used (it started in the interval, or was missed by the last listing).
                let used: f64 = tree.cpu.iter().map(|(pid, now)| (now - prev.cpu.get(pid).copied().unwrap_or(0.0)).max(0.0)).sum();
                self.intervals.push_back((dt, used));
            }
        }
        self.last = Some((t, tree));
        let window = self.rule.window.as_secs_f64();
        while self.intervals.len() > 1 && self.span() - self.intervals.front().map_or(0.0, |i| i.0) >= window {
            self.intervals.pop_front();
        }
        springboard && self.span() >= window && self.cpu_pct().is_some_and(|pct| pct < self.rule.cpu_pct)
    }

    fn span(&self) -> f64 {
        self.intervals.iter().map(|i| i.0).sum()
    }

    /// The tree's CPU use over the window so far, percent of one core.
    pub fn cpu_pct(&self) -> Option<f64> {
        let span = self.span();
        (span > 0.0).then(|| 100.0 * self.intervals.iter().map(|i| i.1).sum::<f64>() / span)
    }

    /// Processes in the last sample.
    pub fn procs(&self) -> usize {
        self.last.as_ref().map_or(0, |(_, t)| t.procs())
    }
}

/// How many devices `boot_many` boots at once by default on a host with `cpus` logical CPUs: a
/// boot storm keeps about four cores busy, so a quarter of the host's CPUs, at least 2.
pub fn default_concurrency(cpus: usize) -> usize {
    (cpus / 4).max(2)
}

/// How long a settle wait may take by default with `concurrent` devices booting at once: 600 s,
/// and 150 s per device beyond four. Measured settle times under load were 250-537 s per device
/// (docs/farm.md), and K storms at once make each one longer.
pub fn default_settle_timeout(concurrent: usize) -> Duration {
    Duration::from_secs(600.max(150 * concurrent as u64))
}

/// One host-wide `ps` listing shared by every settle wait in the process: a wait asks for a
/// listing at most `max_age` old and gets the latest one, or takes a new one (while the others
/// wait for it). N concurrent waits polling once a second cost one listing a second, not N.
pub struct Sampler {
    last: std::sync::Mutex<Option<(std::time::Instant, std::sync::Arc<Vec<Row>>)>>,
}

impl Sampler {
    pub const fn new() -> Sampler {
        Sampler { last: std::sync::Mutex::new(None) }
    }

    /// The latest listing if it is younger than `max_age`, else a new one from `take`; with when
    /// it was taken (a wait times its sample by the listing, not by when it asked).
    pub fn rows(
        &self,
        max_age: Duration,
        take: impl FnOnce() -> Result<Vec<Row>, String>,
    ) -> Result<(std::time::Instant, std::sync::Arc<Vec<Row>>), String> {
        let mut last = self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, rows)) = last.as_ref()
            && at.elapsed() < max_age
        {
            return Ok((*at, rows.clone()));
        }
        let rows = std::sync::Arc::new(take()?);
        let at = std::time::Instant::now();
        *last = Some((at, rows.clone()));
        Ok((at, rows))
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_times_are_read_in_every_form() {
        assert_eq!(cpu_seconds("0:00.43"), Some(0.43));
        assert_eq!(cpu_seconds("12:34.50"), Some(754.5));
        assert_eq!(cpu_seconds("1:02:03"), Some(3723.0));
        assert_eq!(cpu_seconds("2-01:00:00"), Some(2.0 * 86_400.0 + 3600.0));
        assert_eq!(cpu_seconds("x"), None);
        assert_eq!(cpu_seconds(""), None);
    }

    const UDID: &str = "8B529975-BB5F-4ED5-8309-0FF04764BAE6";
    const OTHER: &str = "386312BE-1691-4DAF-AF58-CC74CCCB7ECC";

    fn listing(sb_cpu: &str, extra: &str) -> String {
        format!(
            "    1     0   1:00.00 /sbin/launchd\n  100     1   0:01.00 launchd_sim /Users/u/Library/Developer/CoreSimulator/Devices/{UDID}/data/var/run/launchd_bootstrap.plist\n  \
             110   100   {sb_cpu} /Library/Developer/CoreSimulator/Volumes/iOS_24B8117/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS 27.2.simruntime/Contents/Resources/RuntimeRoot/System/Library/CoreServices/SpringBoard.app/SpringBoard\n  \
             111   110   0:00.50 /x/child with spaces\n  \
             200     1   0:09.00 /Applications/Unrelated.app/Contents/MacOS/Unrelated --udid {UDID}\n{extra}"
        )
    }

    #[test]
    fn a_device_tree_is_its_launchd_sim_and_descendants() {
        let rows = parse_ps(&listing("0:02.00", ""));
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[3].args, "/x/child with spaces");
        let t = trees(&rows, &[UDID, OTHER]);
        let tree = t[0].as_ref().expect("booted");
        assert_eq!(tree.procs(), 3, "launchd_sim, SpringBoard and its child; not a host app naming the UDID");
        assert!(tree.springboard);
        assert_eq!(tree.cpu[&110], 2.0);
        assert_eq!(t[1], None, "no launchd_sim: not booted");
    }

    fn tree(sb_cpu: &str) -> Tree {
        trees(&parse_ps(&listing(sb_cpu, "")), &[UDID]).remove(0).expect("tree")
    }

    #[test]
    fn a_busy_device_is_not_settled_and_a_quiet_one_is_after_the_window() {
        let mut s = Settler::new(Rule { cpu_pct: 10.0, window: Duration::from_secs(3) });
        // 50 % for 3 s, then 2 % (0.02 s of CPU per second).
        let mut cpu = 0.0;
        let mut settled_at = None;
        for t in 0..12 {
            cpu += if t <= 3 { 0.5 } else { 0.02 };
            if s.push(f64::from(t), tree(&format!("0:{cpu:05.2}"))) && settled_at.is_none() {
                settled_at = Some(t);
            }
        }
        // The window is quiet once it no longer contains the busy intervals (the last ends at 3 s).
        assert_eq!(settled_at, Some(6));
        assert!(s.cpu_pct().is_some_and(|p| (p - 2.0).abs() < 0.01), "{:?}", s.cpu_pct());
        assert_eq!(s.procs(), 3);
    }

    #[test]
    fn exiting_processes_dont_make_a_tree_look_quiet() {
        let mut s = Settler::new(Rule { cpu_pct: 10.0, window: Duration::from_secs(2) });
        let busy = |t: f64| {
            let mut tree = tree("0:01.00");
            // A short-lived process each second that used 0.5 s and then is gone.
            tree.cpu.insert(1000 + t as i32, 0.5);
            tree
        };
        for t in 0..6 {
            assert!(!s.push(f64::from(t), busy(f64::from(t))), "t={t}");
        }
        assert!(s.cpu_pct().is_some_and(|p| p >= 49.0), "{:?}", s.cpu_pct());
    }

    #[test]
    fn no_springboard_is_never_settled() {
        let mut s = Settler::new(Rule { cpu_pct: 10.0, window: Duration::from_secs(1) });
        let mut quiet = tree("0:01.00");
        quiet.springboard = false;
        for t in 0..5 {
            assert!(!s.push(f64::from(t), quiet.clone()));
        }
        assert!(s.push(5.0, tree("0:01.00")), "SpringBoard up and quiet");
    }

    #[test]
    fn concurrent_waits_share_one_listing_per_interval() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sampler = Sampler::new();
        let taken = AtomicUsize::new(0);
        let (waiters, polls) = (16, 5);
        let take = || {
            taken.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            Ok(parse_ps(&listing("0:01.00", "")))
        };
        std::thread::scope(|s| {
            for _ in 0..waiters {
                s.spawn(|| {
                    let mut last = None;
                    for _ in 0..polls {
                        let (at, rows) = sampler.rows(Duration::from_millis(90), take).expect("rows");
                        assert_eq!(trees(&rows, &[UDID])[0].as_ref().map(Tree::procs), Some(3));
                        assert!(last.is_none_or(|l| at >= l), "listings only move forward");
                        last = Some(at);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                });
            }
        });
        let n = taken.load(Ordering::SeqCst);
        // One listing per poll interval serves every waiter (a few more where intervals straddle).
        assert!(n <= 2 * polls, "{n} listings for {waiters} waiters × {polls} polls");
        assert!(n >= polls - 1, "{n}");
    }

    #[test]
    fn a_failed_listing_is_not_kept() {
        let sampler = Sampler::new();
        assert_eq!(sampler.rows(Duration::from_secs(60), || Err("ps: no".into())).map(|_| ()), Err("ps: no".into()));
        let (_, rows) = sampler.rows(Duration::from_secs(60), || Ok(parse_ps(&listing("0:01.00", "")))).expect("retried");
        assert_eq!(rows.len(), 5);
        let (_, again) = sampler.rows(Duration::from_secs(60), || Err("not called".into())).expect("cached");
        assert!(std::sync::Arc::ptr_eq(&rows, &again));
    }

    #[test]
    fn the_settle_timeout_covers_loaded_hosts_and_grows_with_k() {
        assert_eq!(default_settle_timeout(1), Duration::from_secs(600));
        assert_eq!(default_settle_timeout(4), Duration::from_secs(600));
        assert_eq!(default_settle_timeout(8), Duration::from_secs(1200));
    }

    #[test]
    fn concurrency_defaults_to_a_quarter_of_the_cpus() {
        assert_eq!(default_concurrency(16), 4);
        assert_eq!(default_concurrency(4), 2);
        assert_eq!(default_concurrency(1), 2);
        assert_eq!(default_concurrency(64), 16);
    }
}
