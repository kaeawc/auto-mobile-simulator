//! Remembered stock-emulator lookups, so resolving a device costs a few file reads rather than a
//! `ps` of the whole host (#256).
//!
//! Finding a stock emulator ([`Env::stock_instances`]) reads each AVD's `hardware-qemu.ini.lock`
//! for a PID, checks that the PID is alive (`kill(pid, 0)`), then asks `ps` for its command line
//! (to tell a qemu for that AVD from a program that reused the PID) and, when it has no `-port`,
//! `lsof` for its listening ports. The lock reads and liveness checks are microseconds; one
//! `ps -axo` is ~60 ms on a host with ~1300 processes and dominated every `devices` call and every
//! `device: "booted"`.
//!
//! A qemu process's command line and console ports never change while it lives, so they are
//! remembered by PID. Every call still reads the locks and checks liveness, so a new emulator (a
//! lock naming a PID not seen before) is asked about at once, and one that stopped (its lock gone
//! or its PID dead) is never listed. What a remembered PID can't rule out alone is reuse: an
//! emulator killed without removing its lock whose PID the kernel hands to another program. macOS
//! hands out PIDs in order, so that takes a wrap of the PID space; entries are re-checked with `ps`
//! after [`STOCK_TTL`] to bound it anyway.

use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use emu_avd::{Env, Probe, Running};

/// How long a PID's command line and ports are trusted before `ps` is asked again.
pub const STOCK_TTL: Duration = Duration::from_secs(10);

/// What the host says about every process at once: one `ps` and one `lsof` per listing, as
/// [`Env::stock_instances`] asks them.
pub trait Host: Send + Sync {
    /// Every process's command line (`ps -axo pid=,args=`).
    fn all_args(&self) -> BTreeMap<i32, String>;
    /// Every process's listening TCP ports (`lsof -nP -iTCP -sTCP:LISTEN -Fpn`).
    fn all_listening(&self) -> BTreeMap<i32, Vec<u16>>;
}

/// The real host: `ps` and `lsof`.
pub struct Ps;

fn run(argv: &[&str]) -> String {
    std::process::Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

impl Host for Ps {
    fn all_args(&self) -> BTreeMap<i32, String> {
        emu_avd::parse_ps_all(&run(&["ps", "-axo", "pid=,args="]))
    }

    fn all_listening(&self) -> BTreeMap<i32, Vec<u16>> {
        emu_avd::parse_lsof_by_pid(&run(&["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"]))
    }
}

/// A qemu process as last seen.
struct Seen {
    args: String,
    /// Its listening ports, once they include a console and adb pair (a booting emulator may not
    /// listen yet, so an incomplete answer is asked again).
    listening: Option<Vec<u16>>,
    at: Instant,
}

#[derive(Default)]
struct State {
    by_pid: HashMap<i32, Seen>,
    /// Stock emulators by (AVD home, AVD name), for one-device lookups.
    by_name: HashMap<(PathBuf, String), (Running, Instant)>,
}

/// Stock emulators found by earlier lookups, shared by every clone of one [`crate::Android`].
pub struct StockCache {
    host: Arc<dyn Host>,
    state: Mutex<State>,
}

impl std::fmt::Debug for StockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.lock();
        f.debug_struct("StockCache").field("pids", &s.by_pid.len()).field("names", &s.by_name.len()).finish()
    }
}

impl StockCache {
    pub fn new(host: Arc<dyn Host>) -> StockCache {
        StockCache { host, state: Mutex::default() }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// [`Env::stock_instances`], asking the host only about PIDs not seen within [`STOCK_TTL`]:
    /// the same instances, with no `ps` or `lsof` when every live lock names a known qemu.
    pub fn instances(&self, env: &Env, skip: &[Running]) -> Vec<Running> {
        let now = Instant::now();
        let mut state = self.lock();
        state.by_pid.retain(|_, s| now.duration_since(s.at) < STOCK_TTL);
        let by_pid = RefCell::new(std::mem::take(&mut state.by_pid));
        let (ps, lsof) = (OnceCell::new(), OnceCell::new());
        let args = |pid: i32| {
            if let Some(s) = by_pid.borrow().get(&pid) {
                return Some(s.args.clone());
            }
            let a = ps.get_or_init(|| self.host.all_args()).get(&pid).cloned()?;
            // Only a qemu's command line is final: a launcher about to exec one is asked again.
            if a.contains("qemu") {
                by_pid.borrow_mut().insert(pid, Seen { args: a.clone(), listening: None, at: now });
            }
            Some(a)
        };
        let listening = |pid: i32| {
            if let Some(ports) = by_pid.borrow().get(&pid).and_then(|s| s.listening.clone()) {
                return ports;
            }
            let ports = lsof.get_or_init(|| self.host.all_listening()).get(&pid).cloned().unwrap_or_default();
            if emu_avd::console_port_from_listening(&ports).is_some()
                && let Some(s) = by_pid.borrow_mut().get_mut(&pid)
            {
                s.listening = Some(ports.clone());
            }
            ports
        };
        let found = env.stock_instances_with(skip, &Probe { args: &args, listening: &listening });
        state.by_pid = by_pid.into_inner();
        // What the listing found is what runs now: one-device lookups start from it.
        state.by_name.retain(|(home, name), _| *home != env.avd_home || skip.iter().any(|r| r.avd_name == *name));
        for r in &found {
            let at = state.by_pid.get(&r.pid).map_or(now, |s| s.at);
            state.by_name.insert((env.avd_home.clone(), r.avd_name.clone()), (r.clone(), at));
        }
        found
    }

    /// [`Env::stock_instance`] for AVD `name`, answered from an earlier lookup while its PID is
    /// alive and was seen within [`STOCK_TTL`].
    pub fn instance(&self, env: &Env, name: &str, skip: &[Running]) -> Option<Running> {
        if skip.iter().any(|r| r.avd_name == name) {
            return None;
        }
        let key = (env.avd_home.clone(), name.to_owned());
        if let Some((r, at)) = self.lock().by_name.get(&key)
            && at.elapsed() < STOCK_TTL
            && r.alive()
        {
            return Some(r.clone());
        }
        let found = env.stock_instance(name, skip);
        let mut state = self.lock();
        match &found {
            Some(r) => state.by_name.insert(key, (r.clone(), Instant::now())),
            None => state.by_name.remove(&key),
        };
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A host whose process table the test writes, counting how often it is asked.
    #[derive(Default)]
    struct FakeHost {
        args: Mutex<BTreeMap<i32, String>>,
        listening: Mutex<BTreeMap<i32, Vec<u16>>>,
        ps: AtomicUsize,
        lsof: AtomicUsize,
    }

    impl Host for FakeHost {
        fn all_args(&self) -> BTreeMap<i32, String> {
            self.ps.fetch_add(1, Ordering::SeqCst);
            self.args.lock().expect("args").clone()
        }
        fn all_listening(&self) -> BTreeMap<i32, Vec<u16>> {
            self.lsof.fetch_add(1, Ordering::SeqCst);
            self.listening.lock().expect("listening").clone()
        }
    }

    impl FakeHost {
        fn qemu(&self, pid: u32, avd: &str, port: Option<u16>) {
            let args = match port {
                Some(p) => format!("/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless -avd {avd} -port {p}"),
                None => format!("/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless -avd {avd} -no-window"),
            };
            self.args.lock().expect("args").insert(pid as i32, args);
        }
        fn asked(&self) -> (usize, usize) {
            (self.ps.load(Ordering::SeqCst), self.lsof.load(Ordering::SeqCst))
        }
    }

    struct Home(PathBuf);

    impl Drop for Home {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// An AVD home with AVDs `names`, none running.
    fn home(tag: &str, names: &[&str]) -> (Home, Env) {
        let root = std::env::temp_dir().join(format!("sa-listing-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in names {
            let dir = root.join(format!("{name}.avd"));
            std::fs::create_dir_all(&dir).expect("avd dir");
            std::fs::write(root.join(format!("{name}.ini")), format!("path={}\n", dir.display())).expect("pointer");
        }
        let env = Env { sdk_root: None, avd_home: root.clone() };
        (Home(root), env)
    }

    /// Stock's lock: the qemu PID, then a NUL.
    fn lock(env: &Env, name: &str, pid: u32) {
        std::fs::write(env.avd_home.join(format!("{name}.avd/hardware-qemu.ini.lock")), format!("{pid}\0")).expect("lock");
    }

    fn unlock(env: &Env, name: &str) {
        std::fs::remove_file(env.avd_home.join(format!("{name}.avd/hardware-qemu.ini.lock"))).expect("unlock");
    }

    fn names(found: &[Running]) -> Vec<&str> {
        found.iter().map(|r| r.avd_name.as_str()).collect()
    }

    /// A process of our own that lives until killed, standing in for a qemu.
    fn process() -> std::process::Child {
        std::process::Command::new("sleep").arg("60").spawn().expect("sleep")
    }

    fn stop(mut child: std::process::Child) {
        child.kill().expect("kill");
        child.wait().expect("wait");
    }

    fn cache(host: &Arc<FakeHost>) -> StockCache {
        StockCache::new(host.clone())
    }

    #[test]
    fn a_known_emulator_is_listed_again_without_asking_the_host() {
        let (_home, env) = home("hit", &["a", "b", "idle"]);
        let (qa, qb) = (process(), process());
        lock(&env, "a", qa.id());
        lock(&env, "b", qb.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(qa.id(), "a", Some(5750));
        // `b` was started without `-port`: its listening sockets say.
        host.qemu(qb.id(), "b", None);
        host.listening.lock().expect("ports").insert(qb.id() as i32, vec![5563, 5562, 51000]);
        let stock = cache(&host);
        let first = stock.instances(&env, &[]);
        assert_eq!(names(&first), ["a", "b"]);
        assert_eq!(host.asked(), (1, 1), "one ps and one lsof for the whole listing");
        for _ in 0..3 {
            assert_eq!(stock.instances(&env, &[]), first);
        }
        assert_eq!(host.asked(), (1, 1), "known qemus are not asked about again");
        // One device: answered from the listing, no host question either.
        let a = stock.instance(&env, "a", &[]).expect("a");
        assert_eq!((a.console_port, a.adb_port, a.pid), (5750, 5751, qa.id() as i32));
        assert_eq!(stock.instance(&env, "b", &[]).map(|r| r.console_port), Some(5562));
        assert_eq!(host.asked(), (1, 1));
        stop(qa);
        stop(qb);
    }

    #[test]
    fn an_emulator_that_stops_is_gone_at_the_next_lookup() {
        let (_home, env) = home("stop", &["a", "b"]);
        let (qa, qb) = (process(), process());
        lock(&env, "a", qa.id());
        lock(&env, "b", qb.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(qa.id(), "a", Some(5750));
        host.qemu(qb.id(), "b", Some(5562));
        let stock = cache(&host);
        assert_eq!(names(&stock.instances(&env, &[])), ["a", "b"]);
        // `a` shuts down cleanly: its lock goes, then its process.
        unlock(&env, "a");
        stop(qa);
        assert_eq!(names(&stock.instances(&env, &[])), ["b"]);
        assert!(stock.instance(&env, "a", &[]).is_none());
        // `b` dies without removing its lock: a dead PID is never an emulator, listed or looked up.
        let pid_b = qb.id();
        stop(qb);
        assert!(stock.instance(&env, "b", &[]).is_none(), "a remembered emulator whose PID died");
        assert!(stock.instances(&env, &[]).is_empty());
        // A new emulator between calls is found by the next one.
        let qc = process();
        lock(&env, "a", qc.id());
        host.qemu(qc.id(), "a", Some(5760));
        let found = stock.instances(&env, &[]);
        assert_eq!(names(&found), ["a"]);
        assert_eq!(found[0].console_port, 5760);
        assert_ne!(found[0].pid, pid_b as i32);
        stop(qc);
    }

    #[test]
    fn a_pid_that_is_not_a_qemu_for_the_avd_is_asked_about_every_time() {
        let (_home, env) = home("reuse", &["a"]);
        let other = process();
        lock(&env, "a", other.id());
        let host = Arc::new(FakeHost::default());
        host.args.lock().expect("args").insert(other.id() as i32, "/usr/bin/vim notes.txt".into());
        let stock = cache(&host);
        assert!(stock.instances(&env, &[]).is_empty());
        assert!(stock.instances(&env, &[]).is_empty());
        assert_eq!(host.asked().0, 2, "a non-qemu answer is not remembered");
        // A launcher that has since exec'd its qemu is found.
        host.qemu(other.id(), "a", Some(5770));
        assert_eq!(names(&stock.instances(&env, &[])), ["a"]);
        stop(other);
    }

    #[test]
    fn ports_are_asked_again_until_a_console_and_adb_pair_listens() {
        let (_home, env) = home("ports", &["a"]);
        let q = process();
        lock(&env, "a", q.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(q.id(), "a", None);
        let stock = cache(&host);
        // Booting: nothing listens yet, so it isn't listed (as stock_instances says too).
        assert!(stock.instances(&env, &[]).is_empty());
        host.listening.lock().expect("ports").insert(q.id() as i32, vec![5554, 5555]);
        assert_eq!(stock.instances(&env, &[]).first().map(|r| r.console_port), Some(5554));
        assert_eq!(host.asked(), (1, 2), "the command line was remembered, the empty ports were not");
        stop(q);
    }

    #[test]
    fn the_listing_matches_an_unremembered_one() {
        let (_home, env) = home("same", &["a", "b", "dead", "idle", "simee"]);
        let (qa, qb, qs) = (process(), process(), process());
        lock(&env, "a", qa.id());
        lock(&env, "b", qb.id());
        lock(&env, "dead", 999_999_999);
        lock(&env, "simee", qs.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(qa.id(), "a", Some(5750));
        host.qemu(qb.id(), "b", None);
        host.qemu(qs.id(), "simee", Some(5600));
        host.listening.lock().expect("ports").insert(qb.id() as i32, vec![5567, 5566]);
        let skip = [Running { pid: 1, avd_name: "simee".into(), console_port: 5600, adb_port: 5601, avd_dir: None, simee: true }];
        let plain = {
            let args = host.args.lock().expect("args").clone();
            let ports = host.listening.lock().expect("ports").clone();
            env.stock_instances_with(
                &skip,
                &Probe { args: &|pid| args.get(&pid).cloned(), listening: &|pid| ports.get(&pid).cloned().unwrap_or_default() },
            )
        };
        assert_eq!(names(&plain), ["a", "b"]);
        let stock = cache(&host);
        assert_eq!(stock.instances(&env, &skip), plain);
        assert_eq!(stock.instances(&env, &skip), plain, "remembered");
        assert!(stock.instance(&env, "simee", &skip).is_none(), "a skipped AVD is simee's");
        // Another AVD home's lookups never answer for this one.
        let other = Env { sdk_root: None, avd_home: Path::new("/nonexistent-avd-home").to_owned() };
        assert!(stock.instance(&other, "a", &[]).is_none());
        stop(qa);
        stop(qb);
        stop(qs);
    }
}
