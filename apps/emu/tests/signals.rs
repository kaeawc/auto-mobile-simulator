//! emu's shutdown signals (`src/on_signal.rs`, #388) under real SIGINT and SIGTERM: this test
//! binary re-runs itself as a child that wires them as `emu`'s boot does (the signals blocked
//! before any thread exists, a stop watcher, a machine that stops when asked), so no guest is
//! booted. Its own `main` (`harness = false`), since the test harness's threads would take the
//! signals before they are blocked.

#[allow(dead_code)]
#[path = "../src/on_signal.rs"]
mod on_signal;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Makes this binary the child: the directory for its power-off marker.
const CHILD: &str = "EMU_SIGNALS_CHILD";
/// The child's machine never finishes stopping (a wedged device thread).
const WEDGED: &str = "EMU_SIGNALS_WEDGED";
/// A deadline for a hang, never a measure of speed.
const DEADLINE: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    if let Some(dir) = std::env::var_os(CHILD) {
        return child(Path::new(&dir));
    }
    let cases: [(&str, fn()); 4] = [
        ("sigint_stops_the_machine_and_exits_130", || stops_in_order("-INT", 130)),
        ("sigterm_stops_the_machine_and_exits_143", || stops_in_order("-TERM", 143)),
        ("a_second_signal_exits_at_once_when_the_stop_is_stuck", a_second_signal_exits_at_once),
        ("a_console_kill_still_exits_0", a_console_kill_still_exits_0),
    ];
    let filter: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect();
    for (name, case) in cases {
        if filter.is_empty() || filter.iter().any(|f| name.contains(f.as_str())) {
            case();
            println!("test {name} ... ok");
        }
    }
    ExitCode::SUCCESS
}

/// The child: `emu`'s boot in miniature. Prints `ready` once it runs and `stopping` when a signal
/// starts the stop; `kill` on stdin stops it the console's way (no signal).
fn child(dir: &Path) -> ExitCode {
    // First, before any thread exists.
    let signals = on_signal::shutdown_signals();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let stopping = || {
                println!("stopping");
                stop.store(true, Ordering::SeqCst);
            };
            let Some((sig, forced)) = on_signal::watch(&signals, Duration::from_secs(600), stopping) else { return };
            eprintln!("emu: forced: {forced:?}");
            std::process::exit(on_signal::exit_code(sig));
        });
    }
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(Result::ok) {
                if line == "kill" {
                    stop.store(true, Ordering::SeqCst);
                }
            }
        });
    }
    let wedged = std::env::var_os(WEDGED).is_some();
    let machine = {
        let (stop, marker) = (stop.clone(), dir.join("powered-off"));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) || wedged {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::fs::write(marker, b"off").expect("marker");
        })
    };
    println!("ready");
    machine.join().expect("machine");
    on_signal::stopped()
}

struct Emu {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Emu {
    fn start(dir: &Path, wedged: bool) -> Emu {
        let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
        cmd.env(CHILD, dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if wedged {
            cmd.env(WEDGED, "1");
        }
        let mut child = cmd.spawn().expect("spawn the child");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let stdin = child.stdin.take();
        let emu = Emu { child, stdin, lines };
        emu.expect_line("ready");
        emu
    }

    fn expect_line(&self, want: &str) {
        let until = Instant::now() + DEADLINE;
        loop {
            match self.lines.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(line) if line == want => return,
                Ok(_) => {}
                Err(e) => panic!("no `{want}` from the child: {e}"),
            }
        }
    }

    fn signal(&self, sig: &str) {
        let ok = Command::new("kill").args([sig, &self.child.id().to_string()]).status().expect("kill").success();
        assert!(ok, "kill {sig}");
    }

    /// The exit status and stderr, within the deadline.
    fn exit(mut self) -> (ExitStatus, String) {
        let until = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                break status;
            }
            assert!(Instant::now() < until, "the child didn't exit within {DEADLINE:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut err = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            stderr.read_to_string(&mut err).ok();
        }
        (status, err)
    }
}

impl Drop for Emu {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A short scratch directory of this run's own.
fn scratch(tag: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!("emu-sig-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn stops_in_order(sig: &str, code: i32) {
    let dir = scratch(&sig[1..]);
    let emu = Emu::start(&dir, false);
    emu.signal(sig);
    let (status, err) = emu.exit();
    assert_eq!(status.code(), Some(code), "{status}; stderr:\n{err}");
    assert!(dir.join("powered-off").exists(), "the machine stopped in order");
    assert!(!err.contains("forced"), "{err}");
    let _ = std::fs::remove_dir_all(dir);
}

fn a_second_signal_exits_at_once() {
    let dir = scratch("twice");
    let emu = Emu::start(&dir, true);
    emu.signal("-INT");
    emu.expect_line("stopping");
    // The stop never finishes and its limit is far off: only the second signal ends it.
    emu.signal("-TERM");
    let (status, err) = emu.exit();
    assert_eq!(status.code(), Some(143), "{status}; stderr:\n{err}");
    assert!(err.contains("forced: Second(15)"), "{err}");
    assert!(!dir.join("powered-off").exists());
    let _ = std::fs::remove_dir_all(dir);
}

fn a_console_kill_still_exits_0() {
    let dir = scratch("kill");
    let mut emu = Emu::start(&dir, false);
    let mut stdin = emu.stdin.take().expect("stdin");
    writeln!(stdin, "kill").expect("write");
    let (status, err) = emu.exit();
    assert!(status.success(), "{status}; stderr:\n{err}");
    assert!(dir.join("powered-off").exists());
    let _ = std::fs::remove_dir_all(dir);
}
