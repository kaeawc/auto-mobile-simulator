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
    /// Listens on 127.0.0.1:`port`; `vsock` is the vsock worker's event channel.
    pub fn bind(port: u16, vsock: Sender<WorkerEvent>) -> std::io::Result<AdbService> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
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
