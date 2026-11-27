//! The device's network activity: open sockets per process and bytes transferred. Sockets come
//! from what the host's kernel reports of each process's file descriptors (`sim_coresim::procs`,
//! read by the caller and passed in) and are shown as `lsof -nP -i -F pcPnT` printed them.

use std::collections::HashMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// One open socket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub pid: i32,
    pub process: String,
    pub protocol: String,
    pub local: String,
    pub remote: String,
    pub state: String,
}

/// Bytes a process has sent and received.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub pid: i32,
    pub process: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// One internet socket of a process, as the kernel reports it, before it is shown as a
/// [`Connection`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Socket {
    pub pid: i32,
    /// The process's name (`pbi_name`, up to 32 bytes: what `lsof` prints).
    pub process: String,
    pub fd: i32,
    /// The IP protocol number.
    pub protocol: i32,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    /// A TCP socket's state (`TSI_S_*`).
    pub tcp_state: Option<i32>,
}

impl Connection {
    /// `socket` as `lsof` shows it: numeric addresses, `*` for an unspecified address or port, the
    /// remote end only when there is one, IPv6 addresses in brackets.
    pub fn from_socket(socket: &Socket) -> Connection {
        let remote = if socket.remote.ip().is_unspecified() && socket.remote.port() == 0 { String::new() } else { endpoint(socket.remote) };
        Connection {
            pid: socket.pid,
            process: socket.process.clone(),
            protocol: protocol(socket.protocol),
            local: endpoint(socket.local),
            remote,
            state: socket.tcp_state.map(tcp_state).unwrap_or_default(),
        }
    }
}

/// The connections of `sockets`, by pid then fd (`lsof`'s order).
pub fn connections(sockets: &[Socket]) -> Vec<Connection> {
    let mut sorted: Vec<&Socket> = sockets.iter().collect();
    sorted.sort_by_key(|s| (s.pid, s.fd));
    sorted.into_iter().map(Connection::from_socket).collect()
}

fn endpoint(addr: SocketAddr) -> String {
    let host = match addr {
        _ if addr.ip().is_unspecified() => "*".to_owned(),
        SocketAddr::V4(a) => a.ip().to_string(),
        SocketAddr::V6(a) => format!("[{}]", a.ip()),
    };
    if addr.port() == 0 { format!("{host}:*") } else { format!("{host}:{}", addr.port()) }
}

/// `lsof`'s protocol names.
fn protocol(n: i32) -> String {
    match n {
        6 => "TCP".into(),
        17 => "UDP".into(),
        1 => "ICMP".into(),
        58 => "ICMPV6".into(),
        n => n.to_string(),
    }
}

/// `lsof`'s names for the TCP states (`TSI_S_*`).
fn tcp_state(state: i32) -> String {
    const NAMES: [&str; 11] = [
        "CLOSED",
        "LISTEN",
        "SYN_SENT",
        "SYN_RCVD",
        "ESTABLISHED",
        "CLOSE_WAIT",
        "FIN_WAIT_1",
        "CLOSING",
        "LAST_ACK",
        "FIN_WAIT_2",
        "TIME_WAIT",
    ];
    usize::try_from(state).ok().and_then(|i| NAMES.get(i)).map_or_else(|| format!("UNKNOWN_TCP_STATE_{state}"), |n| (*n).to_owned())
}

/// Parses `lsof -nP -a -i -p <pids> -F pcPnT` field output (what [`connections`] is checked
/// against).
#[cfg(test)]
pub(crate) fn parse_lsof(text: &str) -> Vec<Connection> {
    let mut out = Vec::new();
    let (mut pid, mut process) = (0, String::new());
    let mut current: Option<Connection> = None;
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => process = value.to_owned(),
            "f" => out.extend(current.take()),
            "P" => {
                out.extend(current.take());
                current = Some(Connection {
                    pid,
                    process: process.clone(),
                    protocol: value.to_owned(),
                    local: String::new(),
                    remote: String::new(),
                    state: String::new(),
                });
            }
            "n" => {
                if let Some(c) = current.as_mut() {
                    let (local, remote) = value.split_once("->").unwrap_or((value, ""));
                    c.local = local.to_owned();
                    c.remote = remote.to_owned();
                }
            }
            "T" => {
                if let (Some(c), Some(state)) = (current.as_mut(), value.strip_prefix("ST=")) {
                    c.state = state.to_owned();
                }
            }
            _ => {}
        }
    }
    out.extend(current);
    out
}

/// Parses `nettop -P -L 1 -x -J bytes_in,bytes_out` CSV (`name.pid,in,out,`).
pub fn parse_nettop(text: &str) -> HashMap<i32, (String, u64, u64)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split(',');
            let id = cols.next()?;
            let (name, pid) = id.rsplit_once('.')?;
            let bytes_in = cols.next()?.trim().parse().ok()?;
            let bytes_out = cols.next()?.trim().parse().ok()?;
            Some((pid.parse().ok()?, (name.to_owned(), bytes_in, bytes_out)))
        })
        .collect()
}

/// Traffic of the given processes, busiest first.
pub fn traffic(pids: &[i32]) -> Result<Vec<Traffic>, String> {
    let all = parse_nettop(&crate::run("/usr/bin/nettop", &["-P", "-L", "1", "-x", "-J", "bytes_in,bytes_out"])?);
    let mut out: Vec<Traffic> = pids
        .iter()
        .filter_map(|pid| all.get(pid).map(|(name, i, o)| Traffic { pid: *pid, process: name.clone(), bytes_in: *i, bytes_out: *o }))
        .collect();
    out.sort_by_key(|t| std::cmp::Reverse(t.bytes_in + t.bytes_out));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_fields() {
        let text = "p42\ncMobileSafari\nf7\nPTCP\nn10.0.0.2:5000->17.1.2.3:443\nTST=ESTABLISHED\nf8\nPUDP\nn*:5353\np43\ncnsurlsessiond\nf3\nPTCP\nn127.0.0.1:80\nTST=LISTEN\n";
        let c = parse_lsof(text);
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0],
            Connection {
                pid: 42,
                process: "MobileSafari".into(),
                protocol: "TCP".into(),
                local: "10.0.0.2:5000".into(),
                remote: "17.1.2.3:443".into(),
                state: "ESTABLISHED".into()
            }
        );
        assert_eq!((c[1].protocol.as_str(), c[1].local.as_str(), c[1].remote.as_str()), ("UDP", "*:5353", ""));
        assert_eq!((c[2].pid, c[2].state.as_str()), (43, "LISTEN"));
    }

    #[test]
    fn parses_nettop_csv() {
        let t = parse_nettop(",bytes_in,bytes_out,\nMobileSafari.42,1000,200,\nweird.name.7,5,6,\nbad,\n");
        assert_eq!(t.get(&42), Some(&("MobileSafari".into(), 1000, 200)));
        assert_eq!(t.get(&7), Some(&("weird.name".into(), 5, 6)));
        assert_eq!(t.len(), 2);
    }

    fn sock(pid: i32, fd: i32, protocol: i32, local: &str, remote: &str, tcp_state: Option<i32>) -> Socket {
        Socket {
            pid,
            process: format!("p{pid}"),
            fd,
            protocol,
            local: local.parse().expect("local"),
            remote: remote.parse().expect("remote"),
            tcp_state,
        }
    }

    #[test]
    fn sockets_are_shown_as_lsof_shows_them_by_pid_then_fd() {
        let c = connections(&[
            sock(9, 4, 6, "[2600::bd0b]:52592", "[2620:149:a28:f000::1]:443", Some(4)),
            sock(7, 12, 17, "0.0.0.0:0", "0.0.0.0:0", None),
            sock(7, 3, 6, "127.0.0.1:80", "0.0.0.0:0", Some(1)),
            sock(9, 2, 17, "[::]:5353", "[::]:0", None),
            sock(9, 5, 6, "10.0.0.2:5000", "17.1.2.3:443", Some(3)),
            sock(9, 6, 6, "10.0.0.2:5001", "17.1.2.3:443", Some(42)),
        ]);
        let row = |c: &Connection| (c.pid, c.protocol.clone(), c.local.clone(), c.remote.clone(), c.state.clone());
        let rows: Vec<_> = c.iter().map(row).collect();
        let want = |pid, p: &str, l: &str, r: &str, s: &str| (pid, p.to_owned(), l.to_owned(), r.to_owned(), s.to_owned());
        assert_eq!(
            rows,
            vec![
                want(7, "TCP", "127.0.0.1:80", "", "LISTEN"),
                want(7, "UDP", "*:*", "", ""),
                want(9, "UDP", "*:5353", "", ""),
                want(9, "TCP", "[2600::bd0b]:52592", "[2620:149:a28:f000::1]:443", "ESTABLISHED"),
                want(9, "TCP", "10.0.0.2:5000", "17.1.2.3:443", "SYN_RCVD"),
                want(9, "TCP", "10.0.0.2:5001", "17.1.2.3:443", "UNKNOWN_TCP_STATE_42"),
            ]
        );
        assert_eq!(c[0].process, "p7");
    }

    /// Recorded on an iPhone 17 Pro (iOS 27.2) on macOS 26.6: the internet sockets of the device's
    /// processes read through libproc and, between two such reads that agreed, `lsof` over the same
    /// processes. Global addresses are rewritten into documentation ranges.
    const SOCKETS: &str = include_str!("../fixtures/ios27.2-connections.libproc.tsv");
    const LSOF_RECORDED: &str = include_str!("../fixtures/ios27.2-connections.lsof.txt");

    fn parse_sockets(text: &str) -> Vec<Socket> {
        text.lines()
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let state: i32 = f[6].parse().expect("state");
                Socket {
                    pid: f[0].parse().expect("pid"),
                    process: f[1].to_owned(),
                    fd: f[2].parse().expect("fd"),
                    protocol: f[3].parse().expect("protocol"),
                    local: f[4].parse().expect("local"),
                    remote: f[5].parse().expect("remote"),
                    tcp_state: (state >= 0).then_some(state),
                }
            })
            .collect()
    }

    #[test]
    fn the_recorded_sockets_match_what_lsof_printed() {
        let ours = connections(&parse_sockets(SOCKETS));
        let theirs = parse_lsof(LSOF_RECORDED);
        assert!(ours.len() >= 10, "{} sockets", ours.len());
        assert_eq!(ours, theirs);
    }
}
