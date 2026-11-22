//! A shaping HTTP proxy for one device. The device is pointed at it through its own `configd_sim`
//! (see [`crate::device_proxy`]), so every proxy-aware client in the device uses it: URLSession,
//! WebKit (whose networking extension can't be injected) and system daemons. HTTPS goes through
//! `CONNECT` tunnels and is shaped without being decrypted. Each tunnel is logged by host.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Profile;

/// One proxied connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flow {
    /// Unix seconds when it started.
    pub started: u64,
    pub method: String,
    pub host: String,
    pub port: u16,
    /// `ok`, `refused` (offline or loss), or `failed: <error>`.
    pub outcome: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub millis: u64,
}

struct Shared {
    profile_path: std::path::PathBuf,
    profile: Mutex<(Profile, Option<SystemTime>, Instant)>,
    up_free_at: Mutex<Instant>,
    down_free_at: Mutex<Instant>,
    log: Mutex<VecDeque<Flow>>,
    stop: AtomicBool,
    connections: AtomicU64,
}

impl Shared {
    /// The current profile, re-read at most every 250 ms when the file changed.
    fn profile(&self) -> Profile {
        let Ok(mut guard) = self.profile.lock() else { return Profile::default() };
        let (profile, mtime, checked) = &mut *guard;
        if checked.elapsed() >= Duration::from_millis(250) {
            *checked = Instant::now();
            let modified = std::fs::metadata(&self.profile_path).and_then(|m| m.modified()).ok();
            if modified != *mtime {
                *mtime = modified;
                *profile = std::fs::read_to_string(&self.profile_path).map(|t| Profile::from_conf(&t)).unwrap_or_default();
            }
        }
        profile.clone()
    }

    /// Sleeps so this direction's traffic stays under the profile's rate (shared by all flows).
    fn pace(&self, bytes: usize, up: bool, host: &str) {
        let kbps = {
            let p = self.profile().for_host(host);
            if up { p.up_kbps } else { p.down_kbps }
        };
        if kbps == 0 || bytes == 0 {
            return;
        }
        let cost = Duration::from_secs_f64(bytes as f64 * 8.0 / (f64::from(kbps) * 1000.0));
        let wait = {
            let lock = if up { &self.up_free_at } else { &self.down_free_at };
            let Ok(mut free_at) = lock.lock() else { return };
            let now = Instant::now();
            let start = (*free_at).max(now);
            *free_at = start + cost;
            *free_at - now
        };
        std::thread::sleep(wait);
    }

    fn record(&self, flow: Flow) {
        if let Ok(mut log) = self.log.lock() {
            if log.len() >= 500 {
                log.pop_front();
            }
            log.push_back(flow);
        }
    }
}

/// A running proxy. Dropping it stops accepting new connections.
pub struct Proxy {
    pub port: u16,
    shared: Arc<Shared>,
}

impl Proxy {
    /// Starts a proxy on 127.0.0.1 that shapes with the profile at `profile_path`.
    pub fn start(profile_path: std::path::PathBuf) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let now = Instant::now();
        let shared = Arc::new(Shared {
            profile_path,
            profile: Mutex::new((Profile::default(), None, now.checked_sub(Duration::from_secs(1)).unwrap_or(now))),
            up_free_at: Mutex::new(now),
            down_free_at: Mutex::new(now),
            log: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            connections: AtomicU64::new(0),
        });
        let accept = shared.clone();
        std::thread::Builder::new().name("sim-rs-proxy".into()).spawn(move || {
            for stream in listener.incoming() {
                if accept.stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let s = accept.clone();
                std::thread::spawn(move || handle(&s, stream));
            }
        })?;
        Ok(Self { port, shared })
    }

    /// The most recent flows, oldest first.
    pub fn flows(&self) -> Vec<Flow> {
        self.shared.log.lock().map(|l| l.iter().cloned().collect()).unwrap_or_default()
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> u64 {
        self.shared.connections.load(Ordering::Relaxed)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so it sees `stop`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Reads the request head (through the blank line). Returns the head and any bytes read past it.
fn read_head(stream: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed before request"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
    }
}

/// Splits `host:port` (IPv6 in brackets), with a default port.
pub fn split_host(authority: &str, default_port: u16) -> (String, u16) {
    if let Some(rest) = authority.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(default_port);
        return (host.to_owned(), port);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() && !h.contains(':') => (h.to_owned(), p.parse().unwrap_or(default_port)),
        _ => (authority.to_owned(), default_port),
    }
}

/// Parses an absolute-form URI (`http://host[:port]/path`) into host, port and origin-form path.
pub fn split_uri(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri.strip_prefix("http://")?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = split_host(authority, 80);
    Some((host, port, path.to_owned()))
}

/// SOCKS5 (no auth, CONNECT only): returns the target host and port, having sent no reply yet.
fn socks5_request(client: &mut TcpStream) -> io::Result<(String, u16)> {
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr)?; // version 5 already peeked; this is [5, nmethods]
    let mut methods = vec![0u8; usize::from(hdr[1])];
    client.read_exact(&mut methods)?;
    client.write_all(&[5, 0])?;
    let mut req = [0u8; 4];
    client.read_exact(&mut req)?;
    if req[1] != 1 {
        client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0])?; // command not supported (UDP ASSOCIATE, BIND)
        return Err(io::Error::new(io::ErrorKind::Unsupported, "only SOCKS CONNECT"));
    }
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            client.read_exact(&mut a)?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len)?;
            let mut name = vec![0u8; usize::from(len[0])];
            client.read_exact(&mut name)?;
            String::from_utf8_lossy(&name).into_owned()
        }
        4 => {
            let mut a = [0u8; 16];
            client.read_exact(&mut a)?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "bad SOCKS address")),
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port)?;
    Ok((host, u16::from_be_bytes(port)))
}

fn handle(s: &Shared, mut client: TcpStream) {
    s.connections.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let mut first = [0u8; 1];
    if client.peek(&mut first).map_or(true, |n| n == 0) {
        return;
    }
    if first[0] == 5 {
        let Ok((host, port)) = socks5_request(&mut client) else { return };
        return connect_and_tunnel(s, client, started, "SOCKS".into(), host, port, None, Vec::new());
    }
    let Ok((head, rest)) = read_head(&mut client) else { return };
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut parts = request_line.split_whitespace();
    let (method, target, version) = (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or("HTTP/1.1").to_owned(),
    );
    let (host, port, forward) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host(&target, 443);
        (h, p, None)
    } else if let Some((h, p, path)) = split_uri(&target) {
        // Re-issue the request to the origin in origin form, dropping proxy-only headers.
        let mut out = format!("{method} {path} {version}\r\n");
        for line in lines.filter(|l| !l.is_empty()) {
            let lower = line.to_ascii_lowercase();
            if !lower.starts_with("proxy-connection:") && !lower.starts_with("proxy-authorization:") {
                out.push_str(line);
                out.push_str("\r\n");
            }
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&rest);
        (h, p, Some(bytes))
    } else {
        let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    connect_and_tunnel(s, client, started, method, host, port, forward, rest);
}

/// Applies the host's profile, connects upstream, answers the client and tunnels.
/// `forward` is a rewritten plain-HTTP request; `None` means a tunnel (CONNECT or SOCKS).
#[allow(clippy::too_many_arguments)]
fn connect_and_tunnel(
    s: &Shared,
    mut client: TcpStream,
    started: Instant,
    method: String,
    host: String,
    port: u16,
    forward: Option<Vec<u8>>,
    rest: Vec<u8>,
) {
    let socks = method == "SOCKS";
    let mut flow = Flow {
        started: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        method: method.clone(),
        host: host.clone(),
        port,
        outcome: "ok".into(),
        bytes_up: 0,
        bytes_down: 0,
        millis: 0,
    };
    let profile = s.profile().for_host(&host);
    let refused = profile.offline || (profile.loss_pct > 0 && (fastrand_percent() < profile.loss_pct));
    if refused {
        // A lost connection stalls, then closes: clients see a timeout. (Offline is normally
        // applied by pointing the device at a closed port; this covers profile edits made directly.)
        flow.outcome = "refused".into();
        if !profile.offline {
            std::thread::sleep(Duration::from_secs(10));
        }
        let _ = client.shutdown(Shutdown::Both);
        flow.millis = started.elapsed().as_millis() as u64;
        s.record(flow);
        return;
    }
    std::thread::sleep(Duration::from_millis(u64::from(profile.latency_ms)));
    let upstream = (host.as_str(), port).to_socket_addrs().and_then(|mut addrs| {
        let addr = addrs.next().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))?;
        TcpStream::connect_timeout(&addr, Duration::from_secs(15))
    });
    let mut upstream = match upstream {
        Ok(u) => u,
        Err(e) => {
            flow.outcome = format!("failed: {e}");
            let reply: &[u8] =
                if socks { &[5, 5, 0, 1, 0, 0, 0, 0, 0, 0] } else { b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n" };
            let _ = client.write_all(reply);
            flow.millis = started.elapsed().as_millis() as u64;
            s.record(flow);
            return;
        }
    };
    let ready = match &forward {
        None if socks => client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]),
        None => client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .and_then(|()| if rest.is_empty() { Ok(()) } else { upstream.write_all(&rest) }),
        Some(bytes) => upstream.write_all(bytes),
    };
    if ready.is_err() {
        return;
    }
    let (up, down) = tunnel(s, client, upstream, &host);
    flow.bytes_up = up + forward.map_or(rest.len(), |b| b.len()) as u64;
    flow.bytes_down = down;
    flow.millis = started.elapsed().as_millis() as u64;
    s.record(flow);
}

/// Pumps both directions until either side closes, shaping each chunk. Returns (up, down) bytes.
fn tunnel(s: &Shared, client: TcpStream, upstream: TcpStream, host: &str) -> (u64, u64) {
    // The last direction data moved, for request/response turnaround latency.
    let last_up = Arc::new(AtomicBool::new(true));
    let pump = |mut from: TcpStream, mut to: TcpStream, up: bool, last_up: Arc<AtomicBool>, s: &Shared| -> u64 {
        let mut buf = [0u8; 16 * 1024];
        let mut total = 0u64;
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !up && last_up.swap(false, Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(u64::from(s.profile().for_host(host).latency_ms)));
            } else if up {
                last_up.store(true, Ordering::Relaxed);
            }
            s.pace(n, up, host);
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
            total += n as u64;
        }
        let _ = to.shutdown(Shutdown::Write);
        total
    };
    let (Ok(c2), Ok(u2)) = (client.try_clone(), upstream.try_clone()) else { return (0, 0) };
    std::thread::scope(|scope| {
        let lu = last_up.clone();
        let up = scope.spawn(move || pump(c2, u2, true, lu, s));
        let down = pump(upstream, client, false, last_up, s);
        (up.join().unwrap_or(0), down)
    })
}

/// A cheap percent roll (0..100) without a rand dependency.
fn fastrand_percent() -> u32 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let mut x = u64::from(nanos) ^ 0x9e37_79b9_7f4a_7c15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x % 100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_authorities_and_uris() {
        assert_eq!(split_host("example.com:8443", 443), ("example.com".into(), 8443));
        assert_eq!(split_host("example.com", 443), ("example.com".into(), 443));
        assert_eq!(split_host("[::1]:8080", 443), ("::1".into(), 8080));
        assert_eq!(split_host("[2001:db8::1]", 443), ("2001:db8::1".into(), 443));
        assert_eq!(split_uri("http://a.test:81/x?y=1"), Some(("a.test".into(), 81, "/x?y=1".into())));
        assert_eq!(split_uri("http://a.test"), Some(("a.test".into(), 80, "/".into())));
        assert_eq!(split_uri("https://a.test/"), None);
    }

    /// A local origin, the proxy, and a client: CONNECT tunnels data, offline refuses, latency delays.
    #[test]
    fn tunnels_shapes_and_refuses() {
        let origin = TcpListener::bind("127.0.0.1:0").expect("origin");
        let origin_port = origin.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for s in origin.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut s = s;
                    let mut b = [0u8; 4];
                    if s.read_exact(&mut b).is_ok() {
                        let _ = s.write_all(&[7u8; 20_000]);
                    }
                });
            }
        });
        let dir = std::env::temp_dir().join(format!("sim-netshape-proxy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let conf = dir.join("p.conf");
        std::fs::write(&conf, Profile::default().to_conf()).expect("conf");
        let proxy = Proxy::start(conf.clone()).expect("proxy");
        let fetch = || -> io::Result<(usize, Duration)> {
            let t = Instant::now();
            let mut c = TcpStream::connect(("127.0.0.1", proxy.port))?;
            c.write_all(format!("CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\n\r\n").as_bytes())?;
            let mut head = [0u8; 39];
            c.read_exact(&mut head)?;
            c.write_all(b"ping")?;
            let mut body = Vec::new();
            c.read_to_end(&mut body)?;
            Ok((body.len(), t.elapsed()))
        };
        assert_eq!(fetch().expect("plain").0, 20_000);
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(&conf, Profile { latency_ms: 300, ..Profile::default() }.to_conf()).expect("conf");
        std::thread::sleep(Duration::from_millis(300));
        let (n, took) = fetch().expect("slow");
        assert_eq!(n, 20_000);
        assert!(took >= Duration::from_millis(550), "{took:?}");
        std::fs::write(&conf, Profile { offline: true, ..Profile::default() }.to_conf()).expect("conf");
        std::thread::sleep(Duration::from_millis(300));
        assert!(fetch().is_err());
        // SOCKS5 CONNECT to the same origin, by address, with a per-host offline rule for another name.
        std::fs::write(
            &conf,
            Profile {
                rules: vec![crate::Rule { pattern: "blocked.test".into(), offline: Some(true), ..crate::Rule::default() }],
                ..Profile::default()
            }
            .to_conf(),
        )
        .expect("conf");
        std::thread::sleep(Duration::from_millis(300));
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).expect("socks");
        c.write_all(&[5, 1, 0]).expect("greet");
        let mut g = [0u8; 2];
        c.read_exact(&mut g).expect("method");
        let mut req = vec![5, 1, 0, 1, 127, 0, 0, 1];
        req.extend_from_slice(&origin_port.to_be_bytes());
        c.write_all(&req).expect("req");
        let mut rep = [0u8; 10];
        c.read_exact(&mut rep).expect("reply");
        assert_eq!(rep[1], 0);
        c.write_all(b"ping").expect("ping");
        let mut body = Vec::new();
        c.read_to_end(&mut body).expect("body");
        assert_eq!(body.len(), 20_000);
        drop(c); // a flow is logged once both directions close
        std::thread::sleep(Duration::from_millis(200));
        let flows = proxy.flows();
        assert_eq!(flows.iter().filter(|f| f.outcome == "ok").count(), 3);
        assert!(flows.iter().any(|f| f.outcome == "refused"));
        assert_eq!(flows.last().map(|f| f.method.as_str()), Some("SOCKS"));
        assert!(flows[0].bytes_down >= 20_000);
        std::fs::remove_dir_all(dir).ok();
    }
}
