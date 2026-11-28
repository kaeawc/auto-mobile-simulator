//! Shutting down on SIGINT and SIGTERM (#388): `simee serve`, `simee mcp`.
//!
//! [`watch`] puts `signal-hook`'s iterator on a thread of its own: the signal handler only writes
//! to signal-hook's self-pipe, and that thread does the rest. The first signal marks the process
//! shutting down, so [`crate::call_cancelled`] is true from then on and a call in flight stops at
//! its next safe point, and runs the server's own shutdown on another thread. A second signal
//! exits at once, removing the sockets the process serves on the way out. Either way the process
//! exits with 128 + the signal: 130 for SIGINT, 143 for SIGTERM.
//!
//! Nothing here shuts a device down: a call cancelled at a safe point undoes only what it started
//! itself (a `boot` waiting for admission boots nothing), and a device the process found running
//! stays as it was.

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGINT, SIGTERM};

/// What a call that the shutdown cancelled, or never ran, answers (its own error follows, if any).
pub const SHUTTING_DOWN: &str = "cancelled: server shutting down";

/// The signal that started the shutdown; 0 while none has.
static SIGNAL: AtomicI32 = AtomicI32::new(0);

/// The sockets this process serves on, with the inode each was bound as: removed however the
/// process exits, unless another server has replaced the file since.
static SOCKETS: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());

/// Calls running on the API server's connections; [`IDLE`] is told when one ends.
static IN_FLIGHT: Mutex<usize> = Mutex::new(0);
static IDLE: Condvar = Condvar::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The signal that started this process's shutdown, once one has.
pub fn signal() -> Option<i32> {
    match SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        sig => Some(sig),
    }
}

/// Whether this process is shutting down (a SIGINT or SIGTERM arrived).
pub fn shutting_down() -> bool {
    signal().is_some()
}

/// The exit code for a process a signal stopped: 128 + its number (130 SIGINT, 143 SIGTERM).
pub fn exit_code(sig: i32) -> i32 {
    128 + sig
}

/// The signal's name, for the lines a shutdown prints.
fn name(sig: i32) -> &'static str {
    match sig {
        SIGINT => "SIGINT",
        SIGTERM => "SIGTERM",
        _ => "signal",
    }
}

/// Watches SIGINT and SIGTERM from now on. The first marks the process shutting down and runs
/// `first` (the server's shutdown, which exits or ends the server) on a thread of its own, so
/// the watch goes on; a second exits at once, with [`exit_code`] of it, after removing the
/// sockets [`remove_on_exit`] names. Call it once per process.
pub fn watch(first: impl FnOnce(i32) + Send + 'static) -> std::io::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new().name("simee-signals".into()).spawn(move || {
        let mut first = Some(first);
        for sig in signals.forever() {
            let Some(shutdown) = first.take() else {
                eprintln!("simee: a second {}: exiting now", name(sig));
                remove_sockets();
                std::process::exit(exit_code(sig));
            };
            SIGNAL.store(sig, Ordering::SeqCst);
            eprintln!("simee: {}: shutting down (a second Ctrl-C or SIGTERM exits at once)", name(sig));
            if let Err(e) = std::thread::Builder::new().name("simee-shutdown".into()).spawn(move || shutdown(sig)) {
                eprintln!("simee: cannot start the shutdown ({e}): exiting now");
                remove_sockets();
                std::process::exit(exit_code(sig));
            }
        }
    })?;
    Ok(())
}

/// Removes `path`, the socket this process just bound, when the process exits through a signal
/// ([`remove_sockets`]).
pub(crate) fn remove_on_exit(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        lock(&SOCKETS).push((path.to_owned(), meta.ino()));
    }
}

/// Removes every socket [`remove_on_exit`] named that is still the one this process bound.
pub(crate) fn remove_sockets() {
    for (path, ino) in lock(&SOCKETS).drain(..) {
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.ino() == ino) {
            std::fs::remove_file(&path).ok();
        }
    }
}

/// A call running on a server connection: counted until it is dropped (after its reply is sent).
pub(crate) struct Call(());

impl Drop for Call {
    fn drop(&mut self) {
        let mut n = lock(&IN_FLIGHT);
        *n = n.saturating_sub(1);
        if *n == 0 {
            IDLE.notify_all();
        }
    }
}

/// Counts a call about to run; `None` once the process is shutting down: the call isn't run.
pub(crate) fn enter() -> Option<Call> {
    let mut n = lock(&IN_FLIGHT);
    if shutting_down() {
        return None;
    }
    *n += 1;
    Some(Call(()))
}

/// How many calls are running.
#[cfg(test)]
pub(crate) fn in_flight() -> usize {
    *lock(&IN_FLIGHT)
}

/// Waits up to `limit` for the calls running to end; whether they all did.
pub(crate) fn drain(limit: Duration) -> bool {
    let until = Instant::now() + limit;
    let mut n = lock(&IN_FLIGHT);
    while *n > 0 {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        n = IDLE.wait_timeout(n, left).unwrap_or_else(PoisonError::into_inner).0;
    }
    true
}

/// A call's outcome, saying so when the shutdown cancelled it: an error that ends a call once the
/// process is shutting down is the shutdown's.
pub(crate) fn outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Err(e) if shutting_down() => Err(format!("{SHUTTING_DOWN} ({e})")),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _};
    use std::path::Path;
    use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    /// The environment variable that makes [`server_child`] serve instead of passing at once.
    const CHILD: &str = "SIMEE_SHUTDOWN_CHILD";
    /// How long a test waits for the child to get anywhere: a deadline for a hang, never a
    /// measure of speed.
    const DEADLINE: Duration = Duration::from_secs(60);

    #[test]
    fn a_signal_exits_with_128_plus_its_number() {
        assert_eq!((exit_code(SIGINT), exit_code(SIGTERM)), (130, 143));
        assert_eq!((name(SIGINT), name(SIGTERM)), ("SIGINT", "SIGTERM"));
    }

    /// The child process's server (run only with [`CHILD`] set, by the tests below): `simee
    /// serve`'s shutdown on a session whose host holds every boot (a `boot` waits for admission
    /// until cancelled), with one stream running on fakes that write their WHIP DELETE to
    /// `<dir>/deletes` (or, `wedged`, never finish it). Says `ready` on stdout once it serves,
    /// and `in-flight` once a call runs.
    #[test]
    fn server_child() {
        let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else { return };
        let wedged = std::env::var_os("SIMEE_SHUTDOWN_WEDGED").is_some();
        drop(crate::methods::tests::held_android_session(&dir));
        let stream = crate::stream::tests::start_global_fake_stream(&dir.join("deletes"), wedged);
        assert_eq!(stream, "stream-1");
        let root = dir.clone();
        let make = move || crate::methods::tests::held_android_session(&root);
        std::thread::spawn(|| {
            let until = Instant::now() + DEADLINE;
            while in_flight() == 0 && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            println!("in-flight");
        });
        let socket = dir.join("api.sock");
        let code = crate::serve_until_signal(&socket, std::sync::Arc::new(make), Duration::from_secs(30), || println!("ready"));
        std::process::exit(code.expect("serve"));
    }

    /// A short directory under `/tmp` (the socket path must fit `sockaddr_un`).
    fn short_dir(tag: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp").join(format!("sd-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    struct Server {
        child: Child,
        lines: Receiver<String>,
        stderr: Option<ChildStderr>,
    }

    impl Server {
        fn start(dir: &Path, wedged: bool) -> Server {
            let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
            cmd.args(["--exact", "shutdown::tests::server_child", "--nocapture", "--test-threads=1"])
                .env(CHILD, dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if wedged {
                cmd.env("SIMEE_SHUTDOWN_WEDGED", "1");
            }
            let mut child = cmd.spawn().expect("spawn the server child");
            let stdout = child.stdout.take().expect("stdout");
            let (tx, lines) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            });
            let stderr = child.stderr.take();
            Server { child, lines, stderr }
        }

        /// Waits for the child to print `want` on stdout (at the end of a line: the test harness
        /// starts the line with the test's name).
        fn expect_line(&self, want: &str) {
            let until = Instant::now() + DEADLINE;
            loop {
                let left = until.saturating_duration_since(Instant::now());
                match self.lines.recv_timeout(left) {
                    Ok(line) if line.trim_end().ends_with(want) => return,
                    Ok(_) => {}
                    Err(e) => panic!("no `{want}` from the child: {e}"),
                }
            }
        }

        fn signal(&self, sig: &str) {
            let ok = Command::new("kill").args([sig, &self.child.id().to_string()]).status().expect("kill").success();
            assert!(ok, "kill {sig}");
        }

        /// The child's exit status and its stderr, within the deadline (killed otherwise).
        fn exit(mut self) -> (ExitStatus, String) {
            let until = Instant::now() + DEADLINE;
            let status = loop {
                if let Some(status) = self.child.try_wait().expect("wait") {
                    break status;
                }
                if Instant::now() > until {
                    self.child.kill().ok();
                    panic!("the child didn't exit within {DEADLINE:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut err = String::new();
            if let Some(mut stderr) = self.stderr.take() {
                stderr.read_to_string(&mut err).ok();
            }
            (status, err)
        }
    }

    impl Drop for Server {
        /// A test that failed early leaves no server running.
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                self.child.kill().ok();
                self.child.wait().ok();
            }
        }
    }

    /// A call that waits for admission on the child's held host, sent on a connection of its
    /// own; its reply comes back on the receiver.
    fn boot_in_flight(socket: &Path) -> Receiver<Result<serde_json::Value, String>> {
        let mut client = crate::Client::connect(socket).expect("connect");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(client.call("boot", &json!({"device": "pixel", "platform": "android", "admission": "wait"})));
        });
        rx
    }

    fn stopped_and_exited(sig: &str, code: i32) {
        let dir = short_dir(&sig[1..]);
        let socket = dir.join("api.sock");
        let server = Server::start(&dir, false);
        server.expect_line("ready");
        assert!(socket.exists(), "it serves on {}", socket.display());
        let streams = crate::call(&socket, "streams", json!({})).expect("streams");
        assert_eq!(streams.as_array().map(Vec::len), Some(1), "one stream runs: {streams}");
        let reply = boot_in_flight(&socket);
        server.expect_line("in-flight");
        server.signal(sig);
        let (status, err) = server.exit();
        assert_eq!(status.code(), Some(code), "{status}; stderr:\n{err}");
        let reply = reply.recv_timeout(DEADLINE).expect("a reply");
        let e = reply.expect_err("cancelled");
        assert!(e.starts_with("cancelled: server shutting down (admission: cancelled after waiting"), "{e}");
        assert!(e.ends_with("nothing was booted)"), "{e}");
        assert!(!socket.exists(), "the socket is removed");
        let deletes = std::fs::read_to_string(dir.join("deletes")).unwrap_or_default();
        assert_eq!(deletes, "DELETE http://127.0.0.1:8889/sim/whip/session-0\n", "the stream was stopped with its DELETE");
        assert!(err.contains("shutting down"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sigint_cancels_calls_in_flight_stops_streams_removes_the_socket_and_exits_130() {
        stopped_and_exited("-INT", 130);
    }

    #[test]
    fn sigterm_cancels_calls_in_flight_stops_streams_removes_the_socket_and_exits_143() {
        stopped_and_exited("-TERM", 143);
    }

    #[test]
    fn a_second_signal_exits_at_once_when_the_shutdown_is_stuck() {
        let dir = short_dir("twice");
        let socket = dir.join("api.sock");
        // The stream's DELETE never finishes: the first signal's shutdown never ends on its own.
        let server = Server::start(&dir, true);
        server.expect_line("ready");
        // Connected before the signal: the server stops accepting, not answering open connections.
        let mut client = crate::Client::connect(&socket).expect("connect");
        server.signal("-INT");
        // A call sent once the shutdown has begun isn't run.
        let until = Instant::now() + DEADLINE;
        let refused = loop {
            match client.call("form_factors", &json!({})) {
                Err(e) if e.starts_with(SHUTTING_DOWN) => break e,
                Err(e) => panic!("{e}"),
                Ok(_) if Instant::now() > until => panic!("the server never said it was shutting down"),
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        assert_eq!(refused, SHUTTING_DOWN);
        server.signal("-TERM");
        let (status, err) = server.exit();
        assert_eq!(status.code(), Some(143), "{status}; stderr:\n{err}");
        assert!(err.contains("a second SIGTERM: exiting now"), "{err}");
        assert!(!socket.exists(), "the socket is removed on a forced exit too");
        assert!(!dir.join("deletes").exists(), "the stuck DELETE never finished");
        std::fs::remove_dir_all(dir).ok();
    }
}
