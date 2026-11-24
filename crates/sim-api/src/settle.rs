//! When a booted simulator has settled, and how many to boot at once: pure logic over a device's
//! process tree and clocks, so it is tested on any host.
//!
//! A device has finished booting (the service's terminal boot status, a display surface) long
//! before it is quiet: a first boot migrates data and indexes for a minute or more, and every boot
//! starts a few hundred daemons. A farm wants the moment the device stops competing for the host,
//! so "settled" here is: the device's whole process tree (its `launchd_sim` and every descendant)
//! used less than [`Rule::cpu_pct`] percent of one core over the last [`Rule::window`], with
//! SpringBoard running. CPU is read as each process's accumulated CPU time, so a process that
//! exits doesn't make the tree look quieter than it was, and the figure is exact over the window
//! rather than a decaying `%cpu` average.
//!
//! The tree is read through [`Procs`] (libproc on macOS: the host's process table, then the CPU
//! time of the device's processes only), not from a host-wide `ps` listing. Each process's
//! CPU is split into background (maintenance and background QoS: darwinbg work, which runs on the
//! efficiency cores) and the rest; [`Rule::foreground`] judges a tree on the rest only.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

/// One process's accumulated CPU time, seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cpu {
    pub total_s: f64,
    /// The part of `total_s` that ran at background QoS.
    pub background_s: f64,
}

/// One device's process tree at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tree {
    /// Accumulated CPU by pid.
    pub cpu: HashMap<i32, Cpu>,
    /// Whether SpringBoard (the home screen and the process that answers touches) is running.
    pub springboard: bool,
}

impl Tree {
    pub fn procs(&self) -> usize {
        self.cpu.len()
    }
}

/// One row of the host's process table, as a tree walk needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub pid: i32,
    pub ppid: i32,
    /// Whether the process is SpringBoard.
    pub springboard: bool,
}

/// What a tree walk reads about the host's processes.
pub trait Procs {
    /// The host's process table (cheap: no CPU figures).
    fn list(&mut self) -> Vec<Entry>;
    /// `pid`'s accumulated CPU; `None` when it has exited.
    fn cpu(&mut self, pid: i32) -> Option<Cpu>;
}

/// The tree under `root` (a device's `launchd_sim`), from one table and the CPU of the tree's
/// processes only: `None` when the root isn't running (the device shut down). A process that
/// exits during the walk is left out, as from a listing.
pub fn tree(procs: &mut impl Procs, root: i32) -> Option<Tree> {
    let rows = procs.list();
    rows.iter().find(|r| r.pid == root)?;
    let mut children: HashMap<i32, Vec<&Entry>> = HashMap::new();
    for r in &rows {
        children.entry(r.ppid).or_default().push(r);
    }
    let mut tree = Tree::default();
    tree.cpu.insert(root, procs.cpu(root)?);
    let mut todo = vec![root];
    while let Some(pid) = todo.pop() {
        for kid in children.get(&pid).into_iter().flatten() {
            if tree.cpu.contains_key(&kid.pid) {
                continue;
            }
            let Some(cpu) = procs.cpu(kid.pid) else { continue };
            tree.cpu.insert(kid.pid, cpu);
            tree.springboard |= kid.springboard;
            todo.push(kid.pid);
        }
    }
    Some(tree)
}

/// What counts as settled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rule {
    /// The tree's CPU use, percent of one core, must stay under this…
    pub cpu_pct: f64,
    /// …over a window this long.
    pub window: Duration,
    /// Judge only the CPU that didn't run at background QoS: a device whose remaining work is
    /// background-priority daemons on the efficiency cores counts as settled.
    pub foreground: bool,
}

impl Default for Rule {
    /// 10 % of one core over 10 s, all CPU. A booted iPhone 17 (iOS 27.2) idles at 1-6 % with no
    /// client attached; its first boot runs at 100-500 % for a minute or more, a later boot for
    /// 10-20 s.
    fn default() -> Self {
        Rule { cpu_pct: 10.0, window: Duration::from_secs(10), foreground: false }
    }
}

/// A tree's CPU over a window, percent of one core: all of it, and the background part.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Load {
    pub total_pct: f64,
    pub background_pct: f64,
}

impl Load {
    pub fn foreground_pct(self) -> f64 {
        (self.total_pct - self.background_pct).max(0.0)
    }

    /// The figure `rule` compares with its threshold.
    pub fn judged_pct(self, rule: &Rule) -> f64 {
        if rule.foreground { self.foreground_pct() } else { self.total_pct }
    }

    /// Whether this load is quiet enough for `rule`.
    pub fn quiet(self, rule: &Rule) -> bool {
        self.judged_pct(rule) < rule.cpu_pct
    }
}

/// Watches one device's samples and says when it has settled.
#[derive(Debug)]
pub struct Settler {
    rule: Rule,
    last: Option<(f64, Tree)>,
    /// (seconds, CPU used) per interval between samples, newest last, spanning at most the window.
    intervals: VecDeque<(f64, Cpu)>,
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
                // used (it started in the interval, or was missed by the last walk).
                let mut used = Cpu::default();
                for (pid, now) in &tree.cpu {
                    let before = prev.cpu.get(pid).copied().unwrap_or_default();
                    used.total_s += (now.total_s - before.total_s).max(0.0);
                    used.background_s += (now.background_s - before.background_s).max(0.0);
                }
                self.intervals.push_back((dt, used));
            }
        }
        self.last = Some((t, tree));
        let window = self.rule.window.as_secs_f64();
        while self.intervals.len() > 1 && self.span() - self.intervals.front().map_or(0.0, |i| i.0) >= window {
            self.intervals.pop_front();
        }
        springboard && self.span() >= window && self.load().is_some_and(|l| l.quiet(&self.rule))
    }

    fn span(&self) -> f64 {
        self.intervals.iter().map(|i| i.0).sum()
    }

    /// The tree's CPU over the window so far.
    pub fn load(&self) -> Option<Load> {
        let span = self.span();
        (span > 0.0).then(|| {
            let total: f64 = self.intervals.iter().map(|i| i.1.total_s).sum();
            let background: f64 = self.intervals.iter().map(|i| i.1.background_s.min(i.1.total_s)).sum();
            Load { total_pct: 100.0 * total / span, background_pct: 100.0 * background / span }
        })
    }

    /// The tree's CPU use over the window so far, percent of one core (all of it).
    pub fn cpu_pct(&self) -> Option<f64> {
        self.load().map(|l| l.total_pct)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's processes as a table: pid → (ppid, CPU, name).
    #[derive(Default)]
    struct Fake {
        procs: HashMap<i32, (i32, Cpu, &'static str)>,
        lists: usize,
        cpu_reads: usize,
    }

    impl Fake {
        fn with(mut self, pid: i32, ppid: i32, total_s: f64, name: &'static str) -> Fake {
            self.procs.insert(pid, (ppid, Cpu { total_s, background_s: 0.0 }, name));
            self
        }
    }

    impl Procs for Fake {
        fn list(&mut self) -> Vec<Entry> {
            self.lists += 1;
            self.procs.iter().map(|(&pid, p)| Entry { pid, ppid: p.0, springboard: p.2 == "SpringBoard" }).collect()
        }
        fn cpu(&mut self, pid: i32) -> Option<Cpu> {
            self.cpu_reads += 1;
            self.procs.get(&pid).map(|p| p.1)
        }
    }

    /// The host: launchd, a device's launchd_sim (100) with SpringBoard (110) and its child, a
    /// second device (300), and a host app.
    fn host(sb_cpu: f64) -> Fake {
        Fake::default()
            .with(1, 0, 60.0, "launchd")
            .with(100, 1, 1.0, "launchd_sim")
            .with(110, 100, sb_cpu, "SpringBoard")
            .with(111, 110, 0.5, "child")
            .with(300, 1, 1.0, "launchd_sim")
            .with(310, 300, 9.0, "SpringBoard")
            .with(200, 1, 9.0, "Unrelated")
    }

    #[test]
    fn a_device_tree_is_its_launchd_sim_and_descendants() {
        let mut h = host(2.0);
        let tree = tree(&mut h, 100).expect("booted");
        assert_eq!(tree.procs(), 3, "launchd_sim, SpringBoard and its child; not another device or a host app");
        assert!(tree.springboard);
        assert_eq!(tree.cpu[&110].total_s, 2.0);
        assert_eq!(super::tree(&mut h, 999), None, "no root: not booted");
    }

    #[test]
    fn a_walk_reads_only_its_own_devices_processes() {
        // 2000 host processes under launchd, one device of 3: the walk touches the device's 3.
        let mut h = host(2.0);
        for pid in 1000..3000 {
            h = h.with(pid, 1, 0.1, "host");
        }
        tree(&mut h, 100).expect("booted");
        assert_eq!((h.lists, h.cpu_reads), (1, 3), "one table, and the CPU of the tree's processes only");
    }

    #[test]
    fn a_process_that_exits_mid_walk_and_a_cycle_are_left_out() {
        let mut h = host(2.0);
        // A child listed but gone when its CPU is read, and a (bogus) self-parent.
        h.procs.insert(112, (110, Cpu::default(), "gone"));
        h.procs.insert(100, (100, Cpu { total_s: 1.0, background_s: 0.0 }, "launchd_sim"));
        struct Gone(Fake);
        impl Procs for Gone {
            fn list(&mut self) -> Vec<Entry> {
                self.0.list()
            }
            fn cpu(&mut self, pid: i32) -> Option<Cpu> {
                if pid == 112 { None } else { self.0.cpu(pid) }
            }
        }
        let t = tree(&mut Gone(h), 100).expect("booted");
        assert_eq!(t.procs(), 3);
        assert!(!t.cpu.contains_key(&112));
    }

    fn sample(sb_cpu: f64) -> Tree {
        tree(&mut host(sb_cpu), 100).expect("tree")
    }

    #[test]
    fn a_busy_device_is_not_settled_and_a_quiet_one_is_after_the_window() {
        let mut s = Settler::new(Rule { cpu_pct: 10.0, window: Duration::from_secs(3), ..Rule::default() });
        // 50 % for 3 s, then 2 % (0.02 s of CPU per second).
        let mut cpu = 0.0;
        let mut settled_at = None;
        for t in 0..12 {
            cpu += if t <= 3 { 0.5 } else { 0.02 };
            if s.push(f64::from(t), sample(cpu)) && settled_at.is_none() {
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
        let mut s = Settler::new(Rule { cpu_pct: 10.0, window: Duration::from_secs(2), ..Rule::default() });
        let busy = |t: f64| {
            let mut tree = sample(1.0);
            // A short-lived process each second that used 0.5 s and then is gone.
            tree.cpu.insert(1000 + t as i32, Cpu { total_s: 0.5, background_s: 0.0 });
            tree
        };
        for t in 0..6 {
            assert!(!s.push(f64::from(t), busy(f64::from(t))), "t={t}");
        }
        assert!(s.cpu_pct().is_some_and(|p| p >= 49.0), "{:?}", s.cpu_pct());
    }

    #[test]
    fn no_springboard_is_never_settled() {
        let mut s = Settler::new(Rule { cpu_pct: 10.0, window: Duration::from_secs(1), ..Rule::default() });
        let mut quiet = sample(1.0);
        quiet.springboard = false;
        for t in 0..5 {
            assert!(!s.push(f64::from(t), quiet.clone()));
        }
        assert!(s.push(5.0, sample(1.0)), "SpringBoard up and quiet");
    }

    /// A tree whose one daemon (pid 120) has used `fg` s of CPU at normal QoS and `bg` s at
    /// background QoS.
    fn split(fg: f64, bg: f64) -> Tree {
        let mut tree = sample(1.0);
        tree.cpu.insert(120, Cpu { total_s: fg + bg, background_s: bg });
        tree
    }

    /// Feeds a device that runs 5 % foreground and 40 % background for 20 s.
    fn background_heavy(rule: Rule) -> (Option<u32>, Settler) {
        let mut s = Settler::new(rule);
        let mut settled_at = None;
        for t in 0..20 {
            let t = f64::from(t);
            if s.push(t, split(0.05 * t, 0.4 * t)) && settled_at.is_none() {
                settled_at = Some(t as u32);
            }
        }
        (settled_at, s)
    }

    #[test]
    fn background_cpu_is_reported_apart_and_judged_only_when_asked() {
        let window = Duration::from_secs(3);
        let (all, s) = background_heavy(Rule { cpu_pct: 10.0, window, foreground: false });
        assert_eq!(all, None, "by default background work keeps the device unsettled");
        let load = s.load().expect("load");
        assert!((load.total_pct - 45.0).abs() < 0.01, "{load:?}");
        assert!((load.background_pct - 40.0).abs() < 0.01, "{load:?}");
        assert!((load.foreground_pct() - 5.0).abs() < 0.01, "{load:?}");
        let (fg, s) = background_heavy(Rule { cpu_pct: 10.0, window, foreground: true });
        assert_eq!(fg, Some(3), "only background work left: settled once the window is full");
        assert!(s.cpu_pct().is_some_and(|p| (p - 45.0).abs() < 0.01), "cpu_pct still reports all of it");
    }

    #[test]
    fn foreground_judging_still_waits_out_foreground_work() {
        let rule = Rule { cpu_pct: 10.0, window: Duration::from_secs(2), foreground: true };
        let mut s = Settler::new(rule);
        let (mut fg, mut bg) = (0.0, 0.0);
        let mut settled_at = None;
        for t in 0..10 {
            // 60 % foreground until 4 s, then 1 %; background 30 % throughout.
            fg += if t <= 4 { 0.6 } else { 0.01 };
            bg += 0.3;
            if s.push(f64::from(t), split(fg, bg)) && settled_at.is_none() {
                settled_at = Some(t);
            }
        }
        assert_eq!(settled_at, Some(6));
    }

    #[test]
    fn a_load_is_judged_by_the_rule() {
        let load = Load { total_pct: 30.0, background_pct: 25.0 };
        let all = Rule::default();
        let fg = Rule { foreground: true, ..Rule::default() };
        assert_eq!(load.judged_pct(&all), 30.0);
        assert_eq!(load.judged_pct(&fg), 5.0);
        assert!(!load.quiet(&all) && load.quiet(&fg));
        // Background never exceeds the total: a foreground figure is never negative.
        assert_eq!(Load { total_pct: 1.0, background_pct: 2.0 }.foreground_pct(), 0.0);
        assert!(!Rule::default().foreground, "the default judges all CPU");
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
