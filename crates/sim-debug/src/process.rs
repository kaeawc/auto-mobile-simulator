//! The device's process tree.

use serde::{Deserialize, Serialize};

/// One process running inside the device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub cpu: f64,
    /// Resident memory in KiB.
    pub rss_kb: u64,
    pub elapsed: String,
    pub name: String,
    pub path: String,
}

/// Parses `ps -Ao pid=,ppid=,pcpu=,rss=,etime=,comm=` output.
pub fn parse_ps(text: &str) -> Vec<Process> {
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

/// Finds the device's `launchd_sim` pid in `ps -Ao pid=,args=` output.
pub fn find_launchd_sim(args_listing: &str, udid: &str) -> Option<i32> {
    args_listing.lines().find(|l| l.contains("launchd_sim") && l.contains(udid)).and_then(|l| l.split_whitespace().next()?.parse().ok())
}

/// `root` and all of its descendants, sorted by CPU then memory, descending.
pub fn descendants(all: &[Process], root: i32) -> Vec<Process> {
    let mut keep = vec![root];
    let mut i = 0;
    while i < keep.len() {
        let parent = keep[i];
        keep.extend(all.iter().filter(|p| p.ppid == parent && p.pid != parent).map(|p| p.pid));
        i += 1;
    }
    let mut out: Vec<Process> = all.iter().filter(|p| keep.contains(&p.pid)).cloned().collect();
    out.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.rss_kb.cmp(&a.rss_kb)));
    out
}

/// The pid of the device's `launchd_sim` (the root of its process tree).
pub fn launchd_sim_pid(udid: &str) -> Result<i32, String> {
    let args = crate::run("/bin/ps", &["-Ao", "pid=,args="])?;
    find_launchd_sim(&args, udid).ok_or_else(|| format!("no launchd_sim for {udid}; is the device booted?"))
}

/// Every process of the device with `udid`.
pub fn collect(udid: &str) -> Result<Vec<Process>, String> {
    let args = crate::run("/bin/ps", &["-Ao", "pid=,args="])?;
    let root = find_launchd_sim(&args, udid).ok_or_else(|| format!("no launchd_sim for {udid}; is the device booted?"))?;
    let all = parse_ps(&crate::run("/bin/ps", &["-Ao", "pid=,ppid=,pcpu=,rss=,etime=,comm="])?);
    Ok(descendants(&all, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "  10     1  0.0  1000   01:00 /sbin/launchd\n  20    10  1.5  2000   00:30 /path/with space/launchd_sim\n  30    20 12.0  5000   00:10 /x/SpringBoard\n  31    30  0.1   100   00:01 /x/child\n  40    10  0.0   100   00:01 /other\n";

    #[test]
    fn parses_paths_with_spaces() {
        let ps = parse_ps(PS);
        assert_eq!(ps.len(), 5);
        assert_eq!(ps[1].path, "/path/with space/launchd_sim");
        assert_eq!(ps[1].name, "launchd_sim");
        assert_eq!(ps[2].cpu, 12.0);
    }

    #[test]
    fn walks_the_tree_from_launchd_sim() {
        let ps = parse_ps(PS);
        let tree = descendants(&ps, 20);
        assert_eq!(tree.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![30, 20, 31]);
    }

    #[test]
    fn finds_the_devices_launchd_sim() {
        let args = "  5 /usr/sbin/foo\n 20 launchd_sim /Users/x/Devices/ABCD/data/var/run/launchd_bootstrap.plist\n 21 launchd_sim /Users/x/Devices/EFGH/data/x\n";
        assert_eq!(find_launchd_sim(args, "EFGH"), Some(21));
        assert_eq!(find_launchd_sim(args, "ZZZZ"), None);
    }
}
