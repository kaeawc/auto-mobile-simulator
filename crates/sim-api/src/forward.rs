//! When a client may send a method to a running server instead of running it itself: the
//! handshake (`identity`) and the methods that always run in the caller.
//!
//! The CLI forwards a command only to a server that is the same build, from the same executable
//! file (unchanged since the server started), with the same environment, and that answers the
//! handshake within [`HANDSHAKE_TIMEOUT`]. Anything else (an older or newer build, a server that
//! doesn't know `identity`, a suspended or wedged one) and the command runs in the caller.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Client;
use crate::methods::METHODS;

/// How long a client waits for a server to accept and answer the handshake before running the
/// command itself. A forwarded command afterwards has no timeout: a boot can take minutes.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Methods a client never forwards: they always run in the process that calls them.
///
/// - `boot` starts the Android emulator as a child of the calling process. Run on a server, every
///   emulator booted from the CLI would be the server's child, in its process group (Ctrl-C on
///   `simee serve` would kill them all), never waited for (a crashed one stays a zombie that
///   reads as running), and started from the server's build of `emu`.
/// - `network_shape`, `network_state`, `network_flows`, `network_clear`: the shaping proxy is a
///   thread of the process that started it, and the others read or stop that process's proxy.
/// - `audio_capture_start`, `audio_capture_stop`, `audio_mute`, `audio_state`: a capture or a mute
///   is a helper process the caller starts and owns; the others read or stop the caller's.
///
/// Everything else either has no host-side effect beyond the call or acts through the device
/// (its launchd spawns what `launch` and `exec` run), so it gives the same result on a matching
/// server. When unsure about a new method, add it here: running in the caller is always correct.
pub const IN_CALLER: &[&str] = &[
    "boot",
    "network_shape",
    "network_state",
    "network_flows",
    "network_clear",
    "audio_capture_start",
    "audio_capture_stop",
    "audio_mute",
    "audio_state",
];

/// Whether a client may send `method` to a server whose identity matches its own.
pub fn forwards(method: &str) -> bool {
    !IN_CALLER.contains(&method)
}

/// The environment variables a method's result can depend on (SDK and AVD paths, the developer
/// directory, the `emu` and `adb` a backend runs). Their values are hashed, not sent.
const ENVIRONMENT: &[&str] = &[
    "ANDROID_SDK_ROOT",
    "ANDROID_HOME",
    "ANDROID_AVD_HOME",
    "ANDROID_EMULATOR_HOME",
    "ANDROID_USER_HOME",
    "ANDROID_ADB_SERVER_PORT",
    "ANDROID_EMULATOR_LAUNCHER_DIR",
    "DEVELOPER_DIR",
    "EMU_BIN",
    "HOME",
    "PATH",
    "TMPDIR",
];

/// FNV-1a, 64-bit: a stable hash with no dependency.
fn fnv(parts: impl IntoIterator<Item = impl AsRef<[u8]>>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in part.as_ref().iter().chain(&[0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The compiled-in build: the crate version and a hash of the method table (names, parameters,
/// descriptions).
pub fn build() -> String {
    let table = METHODS.iter().flat_map(|(m, p, d)| [*m, *p, *d]);
    format!("{}+{:016x}", env!("CARGO_PKG_VERSION"), fnv(table))
}

/// The executable this process runs, as it was when first asked: its path, inode, size and
/// modification time. A rebuild changes it, so a CLI rebuilt while a server runs doesn't match.
fn executable() -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let path = std::env::current_exe().ok()?.canonicalize().ok()?;
    let m = std::fs::metadata(&path).ok()?;
    Some(format!("{}:{}:{}:{}.{}", path.display(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

fn environment() -> String {
    format!("{:016x}", fnv(ENVIRONMENT.iter().map(|k| format!("{k}={}", std::env::var(k).unwrap_or_default()))))
}

/// This process's identity, fixed the first time it is asked (a server asks when it binds, so a
/// rebuild of its executable afterwards is seen as a different build).
pub fn identity() -> &'static Value {
    static IDENTITY: OnceLock<Value> = OnceLock::new();
    IDENTITY.get_or_init(|| json!({"build": build(), "exe": executable(), "env": environment()}))
}

impl Client {
    /// Connects to the server on `path` and checks that it is this build, from this executable,
    /// with this environment. Fails when it isn't, when it doesn't answer `identity` (a build from
    /// before the handshake), or when connecting and answering take longer than `timeout` (a
    /// suspended or wedged server: the kernel accepts the connection, nobody reads it).
    pub fn handshake(path: &Path, timeout: Duration) -> Result<Client, String> {
        let start = Instant::now();
        let ours = identity();
        if ours["exe"].is_null() {
            return Err("this process's executable is unknown".into());
        }
        // Connecting to a unix socket can block (a full backlog on Linux): connect on a thread and
        // stop waiting at the deadline. A thread still blocked then ends with the process.
        let (tx, rx) = std::sync::mpsc::channel();
        let target = path.to_owned();
        std::thread::Builder::new()
            .name("simee-connect".into())
            .spawn(move || drop(tx.send(UnixStream::connect(target))))
            .map_err(|e| e.to_string())?;
        let stream = rx.recv_timeout(timeout).map_err(|_| "connecting timed out".to_owned())?.map_err(|e| e.to_string())?;
        let left = timeout.saturating_sub(start.elapsed()).max(Duration::from_millis(1));
        stream.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(left)).map_err(|e| e.to_string())?;
        let mut client = Client::from_stream(stream);
        let theirs = client.call("identity", &Value::Null)?;
        if theirs != *ours {
            return Err(format!("the server is another build or environment: {theirs}"));
        }
        let stream = client.stream.get_ref();
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        stream.set_write_timeout(None).map_err(|e| e.to_string())?;
        Ok(client)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_methods_run_in_the_caller_are_pinned() {
        // Changing this list changes which process owns a device's emulator, proxy or audio
        // helper: see `IN_CALLER`.
        assert_eq!(
            IN_CALLER,
            [
                "boot",
                "network_shape",
                "network_state",
                "network_flows",
                "network_clear",
                "audio_capture_start",
                "audio_capture_stop",
                "audio_mute",
                "audio_state"
            ]
        );
        for m in IN_CALLER {
            assert!(METHODS.iter().any(|(name, ..)| name == m), "`{m}` is a method");
            assert!(!forwards(m));
        }
        for m in ["identity", "devices", "tap", "screenshot", "launch", "exec", "create", "shutdown"] {
            assert!(forwards(m), "{m}");
        }
    }

    #[test]
    fn identity_names_this_build_executable_and_environment() {
        let id = identity();
        assert!(id["build"].as_str().is_some_and(|b| b.starts_with(env!("CARGO_PKG_VERSION"))));
        assert!(id["exe"].as_str().is_some_and(|e| e.contains(':')));
        assert_eq!(id["env"].as_str().map(str::len), Some(16));
        assert_ne!(fnv(["a", "b"]), fnv(["ab"]), "parts are delimited");
        let mut s = crate::Session::default();
        assert_eq!(crate::call_method(&mut s, "identity", &Value::Null).as_ref(), Ok(id));
    }

    fn socket(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("simee-hs-{tag}-{}.sock", std::process::id()));
        std::fs::remove_file(&path).ok();
        path
    }

    /// A server that answers every request with `answer(method)`.
    fn answering(tag: &str, answer: fn(&str) -> String) -> PathBuf {
        let path = socket(tag);
        let listener = UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let req: Value = serde_json::from_str(&line).expect("request");
                        let reply = answer(req["method"].as_str().unwrap_or(""));
                        writeln!(writer, "{reply}").expect("write");
                    }
                });
            }
        });
        path
    }

    #[test]
    fn a_matching_server_is_used() {
        let path = socket("match");
        crate::serve_in_background(path.clone()).expect("serve");
        let mut client = Client::handshake(&path, HANDSHAKE_TIMEOUT).expect("same build");
        assert_eq!(client.call("identity", &Value::Null).as_ref(), Ok(identity()));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn another_build_or_an_old_server_is_not_used() {
        let other = answering("other", |_| {
            let mut id = identity().clone();
            id["build"] = json!("0.0.0+0000000000000000");
            json!({"id": 1, "ok": true, "result": id}).to_string()
        });
        let err = Client::handshake(&other, HANDSHAKE_TIMEOUT).err().expect("another build");
        assert!(err.starts_with("the server is another build"), "{err}");
        let old = answering("old", |m| json!({"id": 1, "ok": false, "error": format!("unknown method `{m}`; call `help`")}).to_string());
        let err = Client::handshake(&old, HANDSHAKE_TIMEOUT).err().expect("no handshake");
        assert_eq!(err, "unknown method `identity`; call `help`");
        std::fs::remove_file(other).ok();
        std::fs::remove_file(old).ok();
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        // Accepts and never replies (wedged), and never accepts (suspended: the kernel's backlog
        // completes the connection).
        let wedged = socket("wedged");
        let listener = UnixListener::bind(&wedged).expect("bind");
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().flatten().collect();
            drop(held);
        });
        let suspended = socket("suspended");
        let _backlog = UnixListener::bind(&suspended).expect("bind");
        for path in [&wedged, &suspended] {
            let start = Instant::now();
            assert!(Client::handshake(path, Duration::from_millis(300)).is_err());
            let took = start.elapsed();
            assert!(took >= Duration::from_millis(250) && took < Duration::from_secs(1), "{took:?}");
        }
        std::fs::remove_file(wedged).ok();
        std::fs::remove_file(suspended).ok();
    }
}
