//! adb bridge: host TCP port (console port + 1, like the stock emulator) ⇄ guest adbd.
//!
//! Current adbd listens on vsock port 5555 in the guest, so each host client becomes a vsock
//! connection to it, retried while adbd isn't up yet (`adb devices` shows the device `offline`
//! meanwhile, as with stock). Images whose adbd still uses the `qemud:adb` pipe are served too:
//! that adbd writes `accept`, we answer `ok` when a client arrives, it replies `start`, and the
//! pipe then carries raw adb protocol.
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{HostMsg, PipeCtx, PipeHandler, PipeService, WorkerEvent};

pub const ADBD_VSOCK_PORT: u32 = 5555;
const RETRY: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Matchmaker {
    /// Legacy guest pipes that sent `accept` and wait for a host client.
    guests: VecDeque<PipeCtx>,
}

pub struct AdbService {
    mm: Arc<Mutex<Matchmaker>>,
    pub port: u16,
}

impl AdbService {
    /// Listens on 127.0.0.1:`port` (0 picks a free one; `port` says which); `vsock` is the vsock
    /// worker's event channel.
    pub fn bind(port: u16, vsock: Sender<WorkerEvent>) -> std::io::Result<AdbService> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let port = listener.local_addr()?.port();
        let mm = Arc::new(Mutex::new(Matchmaker::default()));
        let m = mm.clone();
        std::thread::Builder::new().name("adb-listen".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_nodelay(true);
                let waiting = m.lock().unwrap_or_else(|e| e.into_inner()).guests.pop_front();
                match waiting {
                    Some(ctx) => {
                        ctx.send(HostMsg::Attach(stream));
                    }
                    None => connect_vsock(Arc::new(stream), vsock.clone(), port),
                }
            }
        })?;
        Ok(AdbService { mm, port })
    }

    /// Tells a running adb server about us (`host:emulator:<port>`), so `adb devices` lists
    /// `emulator-<port-1>` right away instead of on its next scan.
    pub fn announce_port(port: u16) {
        let server_port: u16 = std::env::var("ANDROID_ADB_SERVER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5037);
        if let Ok(mut s) = TcpStream::connect((Ipv4Addr::LOCALHOST, server_port)) {
            let req = format!("host:emulator:{port}");
            let _ = s.write_all(format!("{:04x}{req}", req.len()).as_bytes());
        }
    }
}

/// Whether the host client is still connected (without consuming its bytes).
fn still_open(s: &TcpStream) -> bool {
    if s.set_nonblocking(true).is_err() {
        return false;
    }
    let mut b = [0u8; 1];
    let open = match s.peek(&mut b) {
        Ok(0) => false,
        Ok(_) => true,
        Err(e) => e.kind() == ErrorKind::WouldBlock,
    };
    let _ = s.set_nonblocking(false);
    open
}

fn connect_vsock(stream: Arc<TcpStream>, vsock: Sender<WorkerEvent>, port: u16) {
    let (s1, s2, v) = (stream.clone(), stream, vsock.clone());
    let make: super::MakeHandler = Box::new(move |ctx| Box::new(TcpForward::start(&s1, ctx)) as Box<dyn PipeHandler>);
    let refused = Box::new(move || {
        std::thread::spawn(move || {
            std::thread::sleep(RETRY);
            if still_open(&s2) {
                connect_vsock(s2, v, port);
            } else {
                // The adb server gave up on this attempt before adbd came up; ask it to
                // reconnect so the device shows up as soon as adbd listens.
                AdbService::announce_port(port);
            }
        });
    });
    let _ = vsock.send(WorkerEvent::Connect { guest_port: ADBD_VSOCK_PORT, make, refused });
}

/// Host end of a forwarded stream: TCP bytes → guest, guest bytes → TCP.
struct TcpForward {
    stream: Option<TcpStream>,
}

impl TcpForward {
    fn start(stream: &TcpStream, ctx: PipeCtx) -> TcpForward {
        let (Ok(writer), Ok(reader)) = (stream.try_clone(), stream.try_clone()) else {
            ctx.send(HostMsg::Close);
            return TcpForward { stream: None };
        };
        spawn_reader(reader, ctx);
        TcpForward { stream: Some(writer) }
    }
}

fn spawn_reader(mut reader: TcpStream, ctx: PipeCtx) {
    let name = format!("adb-conn-{}", ctx.conn);
    let _ = std::thread::Builder::new().name(name).spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !ctx.send(HostMsg::Data(buf[..n].to_vec())) {
                        return;
                    }
                }
            }
        }
        ctx.send(HostMsg::Close);
    });
}

impl PipeHandler for TcpForward {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        self.stream.as_mut().is_some_and(|s| s.write_all(data).is_ok())
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn closed(&mut self) {
        if let Some(s) = &self.stream {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

impl PipeService for AdbService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        name.starts_with("qemud:adb").then(|| Box::new(AdbPipe { ctx, mm: self.mm.clone(), state: State::Idle, buf: Vec::new() }) as _)
    }
}

enum State {
    /// Waiting for adbd's `accept`.
    Idle,
    /// Queued for a host client.
    Waiting,
    /// Sent `ok`; waiting for `start`.
    Starting(TcpStream),
    Forwarding(TcpForward),
}

/// Legacy `qemud:adb` pipe.
struct AdbPipe {
    ctx: PipeCtx,
    mm: Arc<Mutex<Matchmaker>>,
    state: State,
    buf: Vec<u8>,
}

impl PipeHandler for AdbPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        if let State::Forwarding(f) = &mut self.state {
            return f.guest_data(data, to_guest);
        }
        self.buf.extend_from_slice(data);
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Idle if self.buf.starts_with(b"accept") => {
                self.buf.drain(..6);
                self.mm.lock().unwrap_or_else(|e| e.into_inner()).guests.push_back(self.ctx.clone());
                self.state = State::Waiting;
                true
            }
            State::Starting(stream) if self.buf.starts_with(b"start") => {
                self.buf.drain(..5);
                let mut f = TcpForward::start(&stream, self.ctx.clone());
                let rest = std::mem::take(&mut self.buf);
                let ok = rest.is_empty() || f.guest_data(&rest, to_guest);
                self.state = State::Forwarding(f);
                ok
            }
            other => {
                self.state = other;
                true
            }
        }
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match (&mut self.state, msg) {
            (State::Waiting, HostMsg::Attach(stream)) => {
                to_guest.extend_from_slice(b"ok");
                self.state = State::Starting(stream);
                true
            }
            (State::Forwarding(f), msg) => f.host_msg(msg, to_guest),
            (_, HostMsg::Close) => false,
            _ => true,
        }
    }

    fn closed(&mut self) {
        self.mm.lock().unwrap_or_else(|e| e.into_inner()).guests.retain(|c| c.conn != self.ctx.conn);
        match &mut self.state {
            State::Forwarding(f) => f.closed(),
            State::Starting(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, channel};

    use super::*;

    fn next(events: &Receiver<WorkerEvent>) -> WorkerEvent {
        events.recv_timeout(Duration::from_secs(5)).expect("an event")
    }

    fn data(events: &Receiver<WorkerEvent>) -> HostMsg {
        match next(events) {
            WorkerEvent::Host { msg, .. } => msg,
            _ => panic!("expected a host message"),
        }
    }

    fn read_exactly(s: &mut TcpStream, n: usize) -> Vec<u8> {
        s.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut b = vec![0u8; n];
        s.read_exact(&mut b).expect("read");
        b
    }

    #[test]
    fn host_clients_become_vsock_connections_to_adbd() {
        let (vsock, events) = channel();
        let svc = AdbService::bind(0, vsock).expect("bind");
        assert_ne!(svc.port, 0, "the port actually bound");
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, svc.port)).expect("connect");
        let WorkerEvent::Connect { guest_port, make, refused } = next(&events) else { panic!("expected a connect") };
        assert_eq!(guest_port, ADBD_VSOCK_PORT);

        // adbd isn't listening yet: the attempt is retried while the client stays connected.
        refused();
        let WorkerEvent::Connect { make: retry, .. } = next(&events) else { panic!("expected a retry") };
        drop(make);

        let (pipe_events, from_host) = channel();
        let mut h = retry(PipeCtx { conn: 3, events: pipe_events });
        client.write_all(b"CNXN").expect("write");
        let HostMsg::Data(d) = data(&from_host) else { panic!("expected data") };
        assert_eq!(d, b"CNXN");
        let mut to_guest = Vec::new();
        assert!(h.host_msg(HostMsg::Data(d), &mut to_guest));
        assert_eq!(to_guest, b"CNXN", "host bytes go to the guest");
        assert!(h.guest_data(b"AUTH", &mut Vec::new()));
        assert_eq!(read_exactly(&mut client, 4), b"AUTH");
        drop(client);
        assert!(matches!(data(&from_host), HostMsg::Close), "the client leaving closes the pipe");
        assert!(!h.host_msg(HostMsg::Close, &mut Vec::new()));
        h.closed();
    }

    #[test]
    fn legacy_qemud_adb_pipes_handshake_then_forward() {
        let (vsock, _events) = channel();
        let svc = AdbService::bind(0, vsock).expect("bind");
        let (pipe_events, from_host) = channel();
        assert!(svc.open("qemud:sensors", PipeCtx { conn: 1, events: pipe_events.clone() }).is_none());
        let mut pipe = svc.open("qemud:adb", PipeCtx { conn: 7, events: pipe_events }).expect("adb pipe");
        let mut to_guest = Vec::new();
        assert!(pipe.guest_data(b"acc", &mut to_guest) && pipe.guest_data(b"ept", &mut to_guest), "accept may arrive split");
        assert!(pipe.host_msg(HostMsg::Data(b"early".to_vec()), &mut to_guest) && to_guest.is_empty(), "nothing before a client");

        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, svc.port)).expect("connect");
        let attach = data(&from_host);
        assert!(matches!(attach, HostMsg::Attach(_)), "the waiting pipe gets the client");
        assert!(pipe.host_msg(attach, &mut to_guest));
        assert_eq!(to_guest, b"ok");
        assert!(pipe.guest_data(b"startCNXN", &mut Vec::new()), "start, then adb protocol in the same write");
        assert_eq!(read_exactly(&mut client, 4), b"CNXN");
        client.write_all(b"OKAY").expect("write");
        let HostMsg::Data(d) = data(&from_host) else { panic!("expected data") };
        let mut to_guest = Vec::new();
        assert!(pipe.host_msg(HostMsg::Data(d), &mut to_guest));
        assert_eq!(to_guest, b"OKAY");
        pipe.closed();
        assert_eq!(client.read(&mut [0u8; 1]).expect("eof"), 0, "closing the pipe hangs up on the client");
    }

    #[test]
    fn a_closed_pipe_leaves_the_queue() {
        let (vsock, events) = channel();
        let svc = AdbService::bind(0, vsock).expect("bind");
        let (pipe_events, _from_host) = channel();
        let mut pipe = svc.open("qemud:adb", PipeCtx { conn: 9, events: pipe_events }).expect("adb pipe");
        assert!(pipe.guest_data(b"accept", &mut Vec::new()));
        pipe.closed();
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, svc.port)).expect("connect");
        assert!(matches!(next(&events), WorkerEvent::Connect { .. }), "no waiting pipe: the client goes to vsock");
    }

    #[test]
    fn still_open_tells_a_live_client_from_a_gone_one() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        assert!(still_open(&server));
        drop(client);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!still_open(&server));
    }
}
