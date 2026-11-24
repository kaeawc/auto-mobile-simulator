//! adb bridge: host TCP port (console port + 1, like the stock emulator) ⇄ guest adbd.
//!
//! Current adbd listens on vsock port 5555 in the guest, so each host client becomes a vsock
//! connection to it, retried while adbd isn't up yet (`adb devices` shows the device `offline`
//! meanwhile, as with stock). Images whose adbd still uses the `qemud:adb` pipe are served too:
//! that adbd writes `accept`, we answer `ok` when a client arrives, it replies `start`, and the
//! pipe then carries raw adb protocol.
use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::Thread;
use std::time::Duration;

use rustix::buffer::spare_capacity;
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;

use super::{HostMsg, PipeCtx, PipeHandler, PipeService, WorkerEvent};

pub const ADBD_VSOCK_PORT: u32 = 5555;
const RETRY: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Matchmaker {
    /// Legacy guest pipes that sent `accept` and wait for a host client.
    guests: VecDeque<PipeCtx>,
}

/// Where the listener sends host clients: the running machine's vsock worker and legacy pipes.
#[derive(Default)]
struct Route {
    vsock: Option<Sender<WorkerEvent>>,
    mm: Arc<Mutex<Matchmaker>>,
}

/// The adb port, bound once per emulator and kept across guest resets (as stock keeps it), so a
/// rebooting guest comes back on the same port and clients aren't refused meanwhile. Each machine
/// [`attach`](Self::attach)es to it. Dropping it closes the port.
///
/// The accept thread sleeps in `poll` with no timeout, on the listener and on one end of a socket
/// pair; [`Drop`] closes the other end, which wakes it to exit. An idle adb port costs no
/// wakeups (it polled every 100 ms before).
pub struct AdbListener {
    pub port: u16,
    route: Arc<Mutex<Route>>,
    /// Closing this wakes the accept thread to exit.
    wake: Option<UnixStream>,
    /// Accept-loop iterations, for the idle-wakeup test.
    #[cfg_attr(not(test), allow(dead_code))]
    turns: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AdbListener {
    /// Listens on 127.0.0.1:`port` (0 picks a free one; `port` says which). Clients arriving
    /// before a machine attaches are hung up on.
    pub fn bind(port: u16) -> std::io::Result<AdbListener> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let (wake, woken) = UnixStream::pair()?;
        let route = Arc::new(Mutex::new(Route::default()));
        let turns = Arc::new(AtomicUsize::new(0));
        let (r, t) = (route.clone(), turns.clone());
        let thread = std::thread::Builder::new().name("adb-listen".into()).spawn(move || {
            loop {
                t.fetch_add(1, Ordering::Relaxed);
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        // Nothing to accept: sleep until a client or Drop. Any other error
                        // (EMFILE with a client in the backlog, where the listener stays readable)
                        // backs off instead of spinning, and still sees Drop.
                        let backoff = rustix::event::Timespec { tv_sec: 0, tv_nsec: 100_000_000 };
                        let timeout = (e.kind() != ErrorKind::WouldBlock).then_some(&backoff);
                        let mut fds = [PollFd::new(&listener, PollFlags::IN), PollFd::new(&woken, PollFlags::IN)];
                        if e.kind() != ErrorKind::WouldBlock {
                            // Only the wake end: the listener is readable and would return at once.
                            fds[0] = PollFd::new(&woken, PollFlags::IN);
                        }
                        match rustix::event::poll(&mut fds, timeout) {
                            // Drop closed the other end (HUP, or IN at EOF): stop.
                            Ok(_) if !fds[1].revents().is_empty() => return,
                            Ok(_) | Err(Errno::INTR) => continue,
                            Err(_) => return,
                        }
                    }
                };
                // macOS hands out accepted sockets with the listener's O_NONBLOCK.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                let (waiting, vsock) = {
                    let route = r.lock().unwrap_or_else(|e| e.into_inner());
                    (route.mm.lock().unwrap_or_else(|e| e.into_inner()).guests.pop_front(), route.vsock.clone())
                };
                match (waiting, vsock) {
                    (Some(ctx), _) => {
                        ctx.send(HostMsg::Attach(stream));
                    }
                    (None, Some(vsock)) => connect_vsock(Arc::new(stream), vsock, port),
                    (None, None) => {}
                }
            }
        })?;
        Ok(AdbListener { port, route, wake: Some(wake), turns, thread: Some(thread) })
    }

    /// Sends host clients to a new machine from now on; `vsock` is its vsock worker's event
    /// channel. The previous machine's waiting legacy pipes are forgotten.
    pub fn attach(self: &Arc<Self>, vsock: Sender<WorkerEvent>) -> AdbService {
        let mm = Arc::new(Mutex::new(Matchmaker::default()));
        *self.route.lock().unwrap_or_else(|e| e.into_inner()) = Route { vsock: Some(vsock), mm: mm.clone() };
        AdbService { mm, port: self.port, _listener: self.clone() }
    }
}

impl Drop for AdbListener {
    fn drop(&mut self) {
        drop(self.wake.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One machine's end of the adb bridge: its legacy `qemud:adb` pipes.
pub struct AdbService {
    mm: Arc<Mutex<Matchmaker>>,
    pub port: u16,
    _listener: Arc<AdbListener>,
}

impl AdbService {
    /// A listener of its own on 127.0.0.1:`port` (0 picks a free one; `port` says which),
    /// attached to `vsock`, the vsock worker's event channel.
    pub fn bind(port: u16, vsock: Sender<WorkerEvent>) -> std::io::Result<AdbService> {
        Ok(Arc::new(AdbListener::bind(port)?).attach(vsock))
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
        let _ = std::thread::Builder::new().name("adb-retry".into()).spawn(move || {
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
///
/// Neither direction ever blocks the pipe's transport thread (the vsock worker, which serves
/// every pipe). The socket is non-blocking (macOS ignores `MSG_DONTWAIT` on a blocking socket's
/// send), and its reader and writer threads poll for it. Guest bytes go straight to the client's
/// socket without waiting; what it can't
/// take now is queued for a writer thread and reported as backlog, so vsock withholds that much
/// of the guest's credit: a client that stops reading stops only its own guest sender, with at
/// most vsock's receive buffer held here. Client bytes are read only while less than
/// [`TO_GUEST_WINDOW`] waits for the guest, so a guest that reads slowly pushes back on the
/// client through TCP instead of queueing without bound.
#[derive(Default)]
struct TcpForward {
    stream: Option<Arc<TcpStream>>,
    to_host: Arc<ToHost>,
    window: Arc<Window>,
}

/// Client bytes one connection may have read but the guest not yet taken.
const TO_GUEST_WINDOW: usize = 1 << 20;
const READ_CHUNK: usize = 64 * 1024;
/// Reads at least this large hand their buffer over; smaller ones are copied out of it.
const HAND_OVER: usize = READ_CHUNK / 4;
/// Guest bytes held for a client that isn't reading. vsock's credit keeps a guest well under
/// this; only the goldfish pipe, which has no credit, can reach it, and then `guest_data` waits
/// for the client as the blocking write it replaces did.
const MAX_BACKLOG: usize = 1 << 20;

/// Guest bytes waiting for the client.
#[derive(Default)]
struct ToHost {
    queue: Mutex<Queued>,
    /// The writer waits for bytes; a full `guest_data` waits for room.
    more: Condvar,
    room: Condvar,
    /// Bytes accepted from the guest and not yet written to the client.
    backlog: AtomicUsize,
    /// A `Drained` event is on its way to the worker, which clears this when it reads the backlog.
    reported: AtomicBool,
}

#[derive(Default)]
struct Queued {
    buf: Vec<u8>,
    /// The writer holds bytes taken from `buf`.
    writing: bool,
    closed: bool,
    failed: bool,
}

impl ToHost {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queued> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Client bytes in flight to the guest, and the reader thread that waits for room.
#[derive(Default)]
struct Window {
    queued: AtomicUsize,
    closed: AtomicBool,
    reader: OnceLock<Thread>,
}

impl Window {
    /// Waits until another read fits; false once the connection is closed.
    fn wait_for_room(&self) -> bool {
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            if self.queued.load(Ordering::SeqCst) + READ_CHUNK <= TO_GUEST_WINDOW {
                return true;
            }
            std::thread::park();
        }
    }

    fn took(&self, n: usize) {
        // Saturating: bytes the pipe wrote itself (the legacy handshake's `ok`) were never read.
        let prev = self.queued.update(Ordering::SeqCst, Ordering::SeqCst, |q| q.saturating_sub(n));
        let low = TO_GUEST_WINDOW - READ_CHUNK;
        if prev > low && prev.saturating_sub(n) <= low {
            self.wake();
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake();
    }

    fn wake(&self) {
        if let Some(t) = self.reader.get() {
            t.unpark();
        }
    }
}

/// Waits until the non-blocking socket is ready for `flags` or has hung up (the next read or
/// write then says which); false if it can't be polled.
fn ready(s: &TcpStream, flags: PollFlags) -> bool {
    let mut fds = [PollFd::new(s, flags)];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return true,
            Err(Errno::INTR) => {}
            Err(_) => return false,
        }
    }
}

impl TcpForward {
    fn start(stream: &TcpStream, ctx: PipeCtx) -> TcpForward {
        let (Ok(writer), Ok(reader), Ok(())) = (stream.try_clone(), stream.try_clone(), stream.set_nonblocking(true)) else {
            ctx.send(HostMsg::Close);
            return TcpForward::default();
        };
        let f = TcpForward { stream: Some(Arc::new(writer)), ..TcpForward::default() };
        spawn_reader(reader, ctx.clone(), f.window.clone());
        if let Some(s) = &f.stream {
            spawn_writer(s.clone(), ctx, f.to_host.clone());
        }
        f
    }
}

fn spawn_reader(reader: TcpStream, ctx: PipeCtx, window: Arc<Window>) {
    let name = format!("adb-in-{}", ctx.conn);
    let _ = std::thread::Builder::new().name(name).spawn(move || {
        let _ = window.reader.set(std::thread::current());
        // Reads land in the spare capacity of a buffer that is never zeroed. A large read hands
        // that buffer over as it is (and the next gets a fresh one): no copy. A small one is
        // copied out, so queued data never pins a mostly empty 64 KiB buffer per read.
        let mut buf = Vec::with_capacity(READ_CHUNK);
        while window.wait_for_room() {
            match rustix::io::read(&reader, spare_capacity(&mut buf)) {
                Ok(0) => break,
                Ok(n) => {
                    window.queued.fetch_add(n, Ordering::SeqCst);
                    let data = if n >= HAND_OVER {
                        std::mem::replace(&mut buf, Vec::with_capacity(READ_CHUNK))
                    } else {
                        let small = buf.clone();
                        buf.clear();
                        small
                    };
                    if !ctx.send(HostMsg::Data(data)) {
                        return;
                    }
                }
                Err(Errno::AGAIN) => {
                    if !ready(&reader, PollFlags::IN) {
                        break;
                    }
                }
                Err(Errno::INTR) => {}
                Err(_) => break,
            }
        }
        ctx.send(HostMsg::Close);
    });
}

/// Writes what the client couldn't take at once, as it can, reporting progress to the worker.
fn spawn_writer(stream: Arc<TcpStream>, ctx: PipeCtx, q: Arc<ToHost>) {
    let name = format!("adb-out-{}", ctx.conn);
    let _ = std::thread::Builder::new().name(name).spawn(move || {
        // Two buffers trade places: the writer drains one while the worker fills the other.
        let mut chunk = Vec::new();
        'run: loop {
            {
                let mut st = q.lock();
                st.writing = false;
                while st.buf.is_empty() && !st.closed && !st.failed {
                    st = q.more.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                if st.buf.is_empty() || st.failed {
                    break;
                }
                std::mem::swap(&mut st.buf, &mut chunk);
                st.writing = true;
            }
            let mut off = 0;
            while off < chunk.len() {
                match (&*stream).write(&chunk[off..]) {
                    Ok(n) if n > 0 => {
                        off += n;
                        q.backlog.fetch_sub(n, Ordering::SeqCst);
                        q.room.notify_all();
                        if !q.reported.swap(true, Ordering::SeqCst) {
                            ctx.drained();
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock && ready(&stream, PollFlags::OUT) => {}
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    _ => {
                        let mut st = q.lock();
                        (st.failed, st.writing) = (true, false);
                        q.room.notify_all();
                        drop(st);
                        ctx.send(HostMsg::Close);
                        break 'run;
                    }
                }
            }
            chunk.clear();
        }
        if q.lock().closed {
            // `closed` already shut the read side; XNU refuses `Both` then, without sending the FIN.
            let _ = stream.shutdown(Shutdown::Write);
        }
    });
}

impl PipeHandler for TcpForward {
    fn guest_data(&mut self, mut data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        let Some(stream) = &self.stream else { return false };
        let q = &self.to_host;
        let mut st = q.lock();
        if st.failed {
            return false;
        }
        if st.buf.is_empty() && !st.writing {
            // The client keeps up (the usual case): straight into its socket, no thread hop.
            match (&**stream).write(data) {
                Ok(n) => data = &data[n..],
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(_) => {
                    st.failed = true;
                    return false;
                }
            }
            if data.is_empty() {
                return true;
            }
        }
        while !st.failed && q.backlog.load(Ordering::SeqCst) > 0 && q.backlog.load(Ordering::SeqCst) + data.len() > MAX_BACKLOG {
            st = q.room.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        if st.failed {
            return false;
        }
        st.buf.extend_from_slice(data);
        q.backlog.fetch_add(data.len(), Ordering::SeqCst);
        q.more.notify_one();
        true
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) if to_guest.is_empty() => {
                *to_guest = d; // the common case: hand the buffer over
                true
            }
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn backlog(&self) -> usize {
        // Read after clearing, so a write that lands meanwhile reports again.
        self.to_host.reported.store(false, Ordering::SeqCst);
        self.to_host.backlog.load(Ordering::SeqCst)
    }

    fn guest_took(&mut self, n: usize) {
        self.window.took(n);
    }

    fn closed(&mut self) {
        self.window.close();
        let Some(s) = &self.stream else { return };
        let mut st = self.to_host.lock();
        st.closed = true;
        if st.buf.is_empty() && !st.writing {
            let _ = s.shutdown(Shutdown::Both);
        } else {
            // The writer delivers what the guest sent before it closed, then hangs up.
            let _ = s.shutdown(Shutdown::Read);
        }
        self.to_host.more.notify_all();
        self.to_host.room.notify_all();
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

    fn backlog(&self) -> usize {
        match &self.state {
            State::Forwarding(f) => f.backlog(),
            _ => 0,
        }
    }

    fn guest_took(&mut self, n: usize) {
        if let State::Forwarding(f) = &mut self.state {
            f.guest_took(n);
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
    use std::io::Read;
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

    /// #23: the bridge is rebuilt on the port the previous one just used.
    #[test]
    fn a_dropped_listener_frees_its_port_for_the_next() {
        let port = AdbListener::bind(0).expect("bind").port;
        let first = AdbListener::bind(port).expect("bind a free port");
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        assert_eq!(AdbListener::bind(port).err().map(|e| e.kind()), Some(ErrorKind::AddrInUse), "in use while it lives");
        drop(first);
        let again = AdbListener::bind(port).expect("rebind the port just in use");
        let (vsock, events) = channel();
        let _svc = Arc::new(again).attach(vsock);
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        assert!(matches!(next(&events), WorkerEvent::Connect { .. }), "the rebuilt bridge serves clients");
    }

    /// A guest reset attaches a new machine to the same port: clients go to the new one.
    #[test]
    fn a_reset_machine_takes_over_the_port() {
        let listener = Arc::new(AdbListener::bind(0).expect("bind"));
        let (vsock, before) = channel();
        let svc = listener.attach(vsock);
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener.port)).expect("connect");
        assert!(matches!(next(&before), WorkerEvent::Connect { .. }));
        // The first machine stops: its vsock worker and pipe registry go.
        drop((svc, before));

        let (vsock, after) = channel();
        let svc = listener.attach(vsock);
        assert_eq!(svc.port, listener.port);
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener.port)).expect("connect");
        assert!(matches!(next(&after), WorkerEvent::Connect { .. }), "the rebooted guest gets the client");
    }

    /// A legacy pipe left waiting by the machine before a reset never gets a client.
    #[test]
    fn a_reset_forgets_the_old_guests_waiting_pipes() {
        let listener = Arc::new(AdbListener::bind(0).expect("bind"));
        let (vsock, _old_events) = channel();
        let old = listener.attach(vsock);
        let (pipe_events, old_pipe) = channel();
        let mut pipe = old.open("qemud:adb", PipeCtx { conn: 5, events: pipe_events }).expect("adb pipe");
        assert!(pipe.guest_data(b"accept", &mut Vec::new()));

        let (vsock, events) = channel();
        let _new = listener.attach(vsock);
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener.port)).expect("connect");
        assert!(matches!(next(&events), WorkerEvent::Connect { .. }), "the client goes to the new guest");
        assert!(old_pipe.try_recv().is_err(), "not to the old guest's pipe");
    }

    /// Before any machine attaches (or after one's worker is gone) a client is hung up on.
    #[test]
    fn a_client_without_a_machine_is_hung_up_on() {
        let listener = AdbListener::bind(0).expect("bind");
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener.port)).expect("connect");
        client.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        assert_eq!(client.read(&mut [0u8; 1]).expect("eof"), 0);
    }

    /// The accept thread sleeps until a client or Drop: no timer wakes an idle port.
    #[test]
    fn an_idle_listener_does_not_wake() {
        let listener = AdbListener::bind(0).expect("bind");
        std::thread::sleep(Duration::from_millis(100));
        let settled = listener.turns.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(1));
        let idle = listener.turns.load(Ordering::Relaxed) - settled;
        assert!(idle <= 2, "{idle} accept-loop turns in 1 s idle");
        // A client still wakes it (one accept, then back to sleep).
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener.port)).expect("connect");
        client.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        assert_eq!(client.read(&mut [0u8; 1]).expect("eof"), 0, "served");
    }

    /// Dropping the listener wakes its blocked accept thread at once and frees the port.
    #[test]
    fn dropping_an_idle_listener_returns_promptly() {
        let listener = AdbListener::bind(0).expect("bind");
        let port = listener.port;
        std::thread::sleep(Duration::from_millis(50));
        let t = std::time::Instant::now();
        drop(listener);
        assert!(t.elapsed() < Duration::from_millis(100), "drop took {:?}", t.elapsed());
        AdbListener::bind(port).expect("port freed");
    }

    /// A forwarder on one end of a loopback connection, the client on the other.
    fn forwarder() -> (TcpForward, TcpStream, Receiver<WorkerEvent>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let (events, from_host) = channel();
        (TcpForward::start(&server, PipeCtx { conn: 1, events }), client, from_host)
    }

    /// Bytes numbered from `at`, so reordering or loss shows.
    fn numbered(at: usize, n: usize) -> Vec<u8> {
        (at..at + n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn guest_bytes_for_a_stalled_client_queue_without_blocking_and_drain_in_order() {
        let (mut f, mut client, from_host) = forwarder();
        // Until the socket is full, bytes go straight in; then they queue, each call returning at once.
        let chunk = 64 * 1024;
        let mut sent = 0;
        while f.backlog() == 0 {
            assert!(sent < 256 << 20, "the socket never filled");
            let start = std::time::Instant::now();
            assert!(f.guest_data(&numbered(sent, chunk), &mut Vec::new()));
            assert!(start.elapsed() < Duration::from_millis(100), "guest_data blocked");
            sent += chunk;
        }
        for _ in 0..4 {
            let start = std::time::Instant::now();
            assert!(f.guest_data(&numbered(sent, chunk), &mut Vec::new()));
            assert!(start.elapsed() < Duration::from_millis(100), "guest_data blocked");
            sent += chunk;
        }
        let held = f.backlog();
        assert!(held > 0 && held <= MAX_BACKLOG);
        assert!(from_host.try_recv().is_err(), "nothing delivered, nothing to report");

        // The client reads again: every byte, in order, and the worker hears the backlog shrink.
        assert_eq!(read_exactly(&mut client, sent), numbered(0, sent));
        assert!(matches!(next(&from_host), WorkerEvent::Drained { conn: 1 }));
        let start = std::time::Instant::now();
        while f.backlog() != 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "the backlog never drained");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Back to direct writes.
        assert!(f.guest_data(b"tail", &mut Vec::new()));
        assert_eq!(f.backlog(), 0);
        assert_eq!(read_exactly(&mut client, 4), b"tail");
        f.closed();
    }

    #[test]
    fn closing_delivers_what_the_guest_sent_then_hangs_up() {
        let (mut f, mut client, _from_host) = forwarder();
        let mut sent = 0;
        while f.backlog() == 0 {
            assert!(f.guest_data(&numbered(sent, 64 * 1024), &mut Vec::new()));
            sent += 64 * 1024;
        }
        f.closed();
        assert_eq!(read_exactly(&mut client, sent), numbered(0, sent));
        assert_eq!(client.read(&mut [0u8; 1]).expect("eof"), 0, "then the client sees the end");
    }

    #[test]
    fn a_client_that_hung_up_ends_the_pipe() {
        let (mut f, client, from_host) = forwarder();
        drop(client);
        let start = std::time::Instant::now();
        while f.guest_data(&[0u8; 64 * 1024], &mut Vec::new()) {
            assert!(start.elapsed() < Duration::from_secs(5), "writes to a closed client kept succeeding");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(data(&from_host), HostMsg::Close), "the reader sees the hang-up too");
        f.closed();
    }

    #[test]
    fn the_client_is_read_only_while_the_guest_keeps_up() {
        const TOTAL: usize = 8 << 20;
        let (mut f, mut client, from_host) = forwarder();
        let all = numbered(0, TOTAL);
        let writer = std::thread::spawn(move || {
            client.write_all(&all).expect("write");
            client
        });
        // The guest takes nothing: reading stops at the window, and TCP holds the rest.
        let mut got = Vec::new();
        while got.len() < TO_GUEST_WINDOW - READ_CHUNK {
            let HostMsg::Data(d) = data(&from_host) else { panic!("expected data") };
            got.extend(d);
        }
        while let Ok(WorkerEvent::Host { msg: HostMsg::Data(d), .. }) = from_host.recv_timeout(Duration::from_millis(200)) {
            got.extend(d);
        }
        assert!(got.len() <= TO_GUEST_WINDOW, "read {} bytes ahead of the guest", got.len());
        // As the guest takes bytes, reading resumes, until everything has come through in order.
        let mut taken = 0;
        while got.len() < TOTAL {
            f.guest_took(got.len() - taken);
            taken = got.len();
            let HostMsg::Data(d) = data(&from_host) else { panic!("expected data") };
            got.extend(d);
        }
        assert_eq!(got, numbered(0, TOTAL));
        f.guest_took(got.len() - taken);
        let _client = writer.join().expect("writer");
        f.closed();
        assert!(matches!(data(&from_host), HostMsg::Close), "closing stops the reader");
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
