//! What the host says about one process, read from the kernel without spawning anything: the
//! stock emulator's qemu is found by the PID in its lock, so these ask about that PID alone.
//!
//! A host-wide `ps -axo pid=,args=` costs ~60 ms on a host with ~1300 processes and `lsof -iTCP`
//! about as much, and a one-shot CLI call paid both every time (#256). Linux reads `/proc/<pid>`;
//! macOS reads libproc through `sim-coresim` (`sysctl KERN_PROCARGS2`, `proc_pid_rusage`,
//! `proc_pidfdinfo`), the calls `processes` and `connections` use. Another unix falls back to `ps`
//! and `lsof` for the one PID, and can't say when a process started.

use crate::listing::Host;

/// The host's own process table, read per PID.
pub struct Native;

impl Host for Native {
    fn start(&self, pid: i32) -> Option<u64> {
        imp::start(pid)
    }

    fn args(&self, pid: i32) -> Option<String> {
        imp::args(pid)
    }

    fn listening(&self, pid: i32) -> Vec<u16> {
        imp::listening(pid)
    }
}

/// The argument vector of a `KERN_PROCARGS2` record (`argc` as a native-endian `i32`, the
/// executable path, NUL padding, `argc` NUL-terminated strings, then the environment) as the
/// command line `ps` prints: the words joined by spaces.
#[cfg(any(target_os = "macos", test))]
fn procargs_line(buf: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let rest = &buf[4..];
    // The executable path, then the padding NULs before argv[0].
    let rest = &rest[rest.iter().position(|&b| b == 0)?..];
    let rest = &rest[rest.iter().position(|&b| b != 0)?..];
    let words: Vec<&[u8]> = rest.split(|&b| b == 0).take(usize::try_from(argc).ok()?).collect();
    let line = words.iter().map(|w| String::from_utf8_lossy(w)).collect::<Vec<_>>().join(" ");
    let line = line.trim_end();
    (!line.is_empty()).then(|| line.to_owned())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashSet;

    /// Field 22 of `/proc/<pid>/stat`, `starttime` (clock ticks after boot): with the PID, it
    /// names one process. The command name (field 2) may hold spaces and parentheses, so the
    /// fields are counted from the last `)`.
    pub fn start(pid: i32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        starttime(&stat)
    }

    pub(super) fn starttime(stat: &str) -> Option<u64> {
        // After the `)`: state (field 3), then fields 4.. so `starttime` is the 20th.
        stat.get(stat.rfind(')')? + 1..)?.split_whitespace().nth(19)?.parse().ok()
    }

    /// `/proc/<pid>/cmdline`'s NUL-separated words, joined by spaces; `None` for a process that
    /// has gone (or a kernel thread, which has none).
    pub fn args(pid: i32) -> Option<String> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let words: Vec<String> =
            raw.split(|&b| b == 0).filter(|w| !w.is_empty()).map(|w| String::from_utf8_lossy(w).into_owned()).collect();
        (!words.is_empty()).then(|| words.join(" "))
    }

    /// The TCP ports `pid` listens on: its socket inodes (`/proc/<pid>/fd`) found in the LISTEN
    /// rows of its network namespace's `tcp` and `tcp6` tables.
    pub fn listening(pid: i32) -> Vec<u16> {
        let inodes: HashSet<u64> = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let target = std::fs::read_link(e.path()).ok()?;
                target.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
            })
            .collect();
        if inodes.is_empty() {
            return Vec::new();
        }
        ["tcp", "tcp6"]
            .iter()
            .filter_map(|t| std::fs::read_to_string(format!("/proc/{pid}/net/{t}")).ok())
            .flat_map(|table| listen_rows(&table, &inodes))
            .collect()
    }

    pub(super) fn listen_rows(table: &str, inodes: &HashSet<u64>) -> Vec<u16> {
        table
            .lines()
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                // `sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode`; 0A is LISTEN.
                let (local, state, inode) = (f.get(1)?, f.get(3)?, f.get(9)?);
                (*state == "0A" && inodes.contains(&inode.parse().ok()?))
                    .then(|| u16::from_str_radix(local.rsplit(':').next()?, 16).ok())
                    .flatten()
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn starttime_is_counted_from_the_last_parenthesis() {
            let stat = "4242 (qemu (x) sys) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 12 0 987654 1000 200 18446744073709551615 0";
            assert_eq!(starttime(stat), Some(987654));
            assert_eq!(starttime("4242 (short"), None);
        }

        #[test]
        fn listen_rows_keep_only_this_processs_listeners() {
            let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                0: 0100007F:15B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0\n\
                1: 0100007F:15B3 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0 100 0 0 10 0\n\
                2: 0100007F:15B2 0100007F:C000 01 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0\n\
                3: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 333 1 0 100 0 0 10 0\n";
            let ours: HashSet<u64> = [111, 222].into();
            assert_eq!(listen_rows(table, &ours), [5554, 5555], "an established row and another process's listener are left out");
        }

        #[test]
        fn this_process_reads_back_as_itself() {
            let me = std::process::id() as i32;
            let start = start(me).expect("start");
            assert_eq!(super::start(me), Some(start), "stable while it lives");
            assert!(
                args(me)
                    .expect("args")
                    .contains(&std::env::current_exe().expect("exe").file_name().expect("name").to_string_lossy().into_owned())
            );
            let a = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind");
            let b = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0));
            let ports = listening(me);
            assert!(ports.contains(&a.local_addr().expect("addr").port()), "{ports:?}");
            if let Ok(b) = b {
                assert!(ports.contains(&b.local_addr().expect("addr").port()), "{ports:?}");
            }
        }

        #[test]
        fn a_gone_process_reads_as_nothing() {
            let mut child = std::process::Command::new("sleep").arg("60").spawn().expect("sleep");
            let pid = child.id() as i32;
            let started = start(pid).expect("start");
            // Between fork and exec the child still has its parent's command line.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !args(pid).is_some_and(|a| a.starts_with("sleep")) {
                assert!(std::time::Instant::now() < deadline, "the child never exec'd sleep: {:?}", args(pid));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            child.kill().expect("kill");
            child.wait().expect("wait");
            assert_eq!((start(pid), args(pid), listening(pid)), (None, None, Vec::new()));
            assert!(started > 0);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use sim_coresim::procs;

    pub fn start(pid: i32) -> Option<u64> {
        procs::usage(pid).map(|u| u.start_ns)
    }

    pub fn args(pid: i32) -> Option<String> {
        super::procargs_line(&procs::procargs(pid)?)
    }

    pub fn listening(pid: i32) -> Vec<u16> {
        const IPPROTO_TCP: i32 = 6;
        const TSI_S_LISTEN: i32 = 1;
        procs::sockets(pid)
            .into_iter()
            .filter(|s| s.protocol == IPPROTO_TCP && s.tcp_state == Some(TSI_S_LISTEN))
            .map(|s| s.local.port())
            .collect()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::process::Command;

    pub fn start(_: i32) -> Option<u64> {
        None
    }

    pub fn args(pid: i32) -> Option<String> {
        let out = Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !text.is_empty()).then_some(text)
    }

    /// `lsof`'s lines end with `TCP 127.0.0.1:5750 (LISTEN)`.
    pub fn listening(pid: i32) -> Vec<u16> {
        let out = Command::new("lsof").args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-p", &pid.to_string()]).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        text.lines()
            .filter(|l| l.contains("(LISTEN)"))
            .filter_map(|l| l.split_whitespace().rev().nth(1)?.rsplit(':').next()?.parse().ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(argc: i32, exe: &str, padding: usize, argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut b = argc.to_ne_bytes().to_vec();
        b.extend(exe.as_bytes());
        b.extend(std::iter::repeat_n(0, padding));
        for w in argv.iter().chain(env) {
            b.extend(w.as_bytes());
            b.push(0);
        }
        b
    }

    #[test]
    fn a_procargs_record_reads_as_the_command_line_without_the_environment() {
        let b = record(
            5,
            "/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless",
            3,
            &["qemu-system-aarch64-headless", "-avd", "Pixel_9", "-port", "5554"],
            &["HOME=/Users/me", "PATH=/usr/bin"],
        );
        assert_eq!(procargs_line(&b).as_deref(), Some("qemu-system-aarch64-headless -avd Pixel_9 -port 5554"));
        // Fewer words than argc says (a process still exec'ing) is what there is; none is nothing.
        assert_eq!(procargs_line(&record(0, "/x", 1, &[], &[])), None);
        assert_eq!(procargs_line(&[1, 0]), None);
        assert_eq!(procargs_line(&record(2, "/bin/sh", 1, &["sh"], &[])).as_deref(), Some("sh"));
    }
}
