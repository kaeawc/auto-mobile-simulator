//! The device's network activity: open sockets per process and bytes transferred.

use std::collections::HashMap;

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

/// Parses `lsof -nP -a -i -p <pids> -F pcPnT` field output.
pub fn parse_lsof(text: &str) -> Vec<Connection> {
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

/// Open sockets of the given processes.
pub fn connections(pids: &[i32]) -> Result<Vec<Connection>, String> {
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let list = pids.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
    Ok(parse_lsof(&crate::run("/usr/sbin/lsof", &["-nP", "-a", "-i", "-p", &list, "-F", "pcPnT"])?))
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
}
