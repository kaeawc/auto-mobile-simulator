//! The NAT's net thread: guest frames in, host sockets out, and back.
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Checksum, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint};

use super::host::{self, Resolver};
use super::packet::{self, Frame, Link};
use super::{Config, Shared, TO_GUEST, lock};

/// Per-connection buffers: guest to host, host to guest. Allocated zeroed, so the pages a
/// connection never fills are never touched.
const TCP_RX: usize = 256 * 1024;
const TCP_TX: usize = 256 * 1024;
const UDP_IDLE: Duration = Duration::from_secs(60);
const DNS_IDLE: Duration = Duration::from_secs(10);
const PING_IDLE: Duration = Duration::from_secs(10);
const MAX_UDP: usize = 512;
const MAX_PING: usize = 64;
/// Datagrams read from one socket per pass, so one busy flow can't starve the rest.
const DATAGRAMS_PER_PASS: usize = 64;

/// smoltcp's link: frames from the guest wait in `rx`; frames for it collect in `tx` until the
/// pass hands them over. `limit` is how many more the guest's queue takes this pass.
pub struct Wire {
    pub rx: VecDeque<Vec<u8>>,
    pub tx: Vec<Vec<u8>>,
    limit: Option<usize>,
    /// smoltcp wanted to send and `limit` refused it.
    blocked: bool,
    verify: bool,
}

impl Wire {
    /// `verify`: check checksums on frames received (a virtio link never corrupts one).
    pub fn new(verify: bool, limit: Option<usize>) -> Wire {
        Wire { rx: VecDeque::new(), tx: Vec::new(), limit, blocked: false, verify }
    }

    fn full(&self) -> bool {
        self.limit.is_some_and(|l| self.tx.len() >= l)
    }

    /// Queues a frame built outside smoltcp; dropped when the guest's queue is full.
    fn push(&mut self, f: Vec<u8>) {
        if !self.full() {
            self.tx.push(f);
        }
    }
}

pub struct RxTok(Vec<u8>);
pub struct TxTok<'a>(&'a mut Vec<Vec<u8>>);

impl phy::RxToken for RxTok {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxTok<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut v = vec![0; len];
        let r = f(&mut v);
        self.0.push(v);
        r
    }
}

impl phy::Device for Wire {
    type RxToken<'a> = RxTok;
    type TxToken<'a> = TxTok<'a>;

    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(RxTok, TxTok<'_>)> {
        let f = self.rx.pop_front()?;
        // A reply to what the guest sent (an ACK, an ARP reply) always goes.
        Some((RxTok(f), TxTok(&mut self.tx)))
    }

    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<TxTok<'_>> {
        if self.full() {
            self.blocked = true;
            return None;
        }
        Some(TxTok(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = packet::ETH_HDR + packet::MTU;
        if !self.verify {
            c.checksum.ipv4 = Checksum::Tx;
            c.checksum.tcp = Checksum::Tx;
        }
        c
    }
}

/// A flow is keyed by the guest's source and the destination it asked for.
type Key = (SocketAddrV4, SocketAddrV4);

enum TcpState {
    /// The host connect is in progress; the guest's latest SYN waits for its outcome.
    Connecting(Vec<u8>),
    Open(SocketHandle),
}

struct TcpFlow {
    stream: TcpStream,
    state: TcpState,
    ready: PollFlags,
    /// The host closed its half (read returned 0).
    host_eof: bool,
    /// The host's send buffer is full: wait for POLLOUT before writing again.
    write_blocked: bool,
    /// The guest's FIN was passed on.
    host_shut: bool,
}

impl TcpFlow {
    fn new(stream: TcpStream, state: TcpState) -> TcpFlow {
        TcpFlow { stream, state, ready: PollFlags::empty(), host_eof: false, write_blocked: false, host_shut: false }
    }

    /// What to wait for on the host socket. Nothing when there is no room for what it would
    /// deliver: a socket polled for an event the pass can't act on would wake it in a loop.
    fn interest(&self, sockets: &SocketSet<'_>) -> PollFlags {
        match self.state {
            TcpState::Connecting(_) => PollFlags::OUT,
            TcpState::Open(h) => {
                let s = sockets.get::<tcp::Socket>(h);
                let mut ev = PollFlags::empty();
                if !self.host_eof && s.can_send() {
                    ev |= PollFlags::IN;
                }
                if self.write_blocked {
                    ev |= PollFlags::OUT;
                }
                ev
            }
        }
    }
}

struct UdpFlow {
    sock: UdpSocket,
    ready: PollFlags,
    idle: Duration,
    deadline: Instant,
}

struct PingFlow {
    sock: UdpSocket,
    guest: Ipv4Addr,
    ready: PollFlags,
    deadline: Instant,
}

#[derive(Clone, Copy)]
enum Slot {
    Tcp(Key),
    Udp(Key),
    Ping((Ipv4Addr, u16)),
}

/// Frames the NAT builds itself (UDP, ICMP), addressed to the guest's MAC once it is known.
struct Emitter {
    gateway_mac: [u8; 6],
    guest_mac: Option<[u8; 6]>,
    ip_id: u16,
    frames: Vec<Vec<u8>>,
}

impl Emitter {
    fn emit(&mut self, wire: &mut Wire, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, l4: &[u8]) {
        let Some(guest_mac) = self.guest_mac else { return };
        self.ip_id = self.ip_id.wrapping_add(1);
        let link = Link { gateway_mac: self.gateway_mac, guest_mac };
        packet::ipv4_frames(&mut self.frames, link, src, dst, proto, self.ip_id, l4);
        for f in self.frames.drain(..) {
            wire.push(f);
        }
    }
}

pub struct Engine {
    cfg: Config,
    shared: Arc<Shared>,
    iface: Interface,
    sockets: SocketSet<'static>,
    wire: Wire,
    tcp: HashMap<Key, TcpFlow>,
    udp: HashMap<Key, UdpFlow>,
    ping: HashMap<(Ipv4Addr, u16), PingFlow>,
    out: Emitter,
    resolver: Resolver,
    /// Room for a datagram's headers ahead of the largest payload.
    scratch: Vec<u8>,
    inbox: VecDeque<Vec<u8>>,
    epoch: Instant,
    /// Run the next pass at once (the guest drained its queue while this one was blocked).
    again: bool,
}

impl Engine {
    pub fn new(cfg: Config, shared: Arc<Shared>) -> Engine {
        let mut wire = Wire::new(false, Some(TO_GUEST));
        let mut c = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(cfg.gateway_mac)));
        c.random_seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        let mut iface = Interface::new(c, &mut wire, smoltcp::time::Instant::from_millis(0));
        iface.update_ip_addrs(|a| {
            for ip in [cfg.gateway, cfg.resolver] {
                let _ = a.push(IpCidr::new(IpAddress::Ipv4(ip), cfg.prefix_len));
            }
        });
        // Accept segments to any destination: each flow's smoltcp socket stands in for it.
        iface.set_any_ip(true);
        Engine {
            resolver: Resolver::new(cfg.dns.clone()),
            out: Emitter { gateway_mac: cfg.gateway_mac, guest_mac: None, ip_id: 0, frames: Vec::new() },
            cfg,
            shared,
            iface,
            sockets: SocketSet::new(vec![]),
            wire,
            tcp: HashMap::new(),
            udp: HashMap::new(),
            ping: HashMap::new(),
            scratch: vec![0; packet::UDP_HDR + 65536],
            inbox: VecDeque::new(),
            epoch: Instant::now(),
            again: false,
        }
    }

    fn now(&self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.epoch.elapsed().as_micros() as i64)
    }

    pub fn run(mut self, wait: UnixStream) {
        let mut slots: Vec<Slot> = Vec::new();
        let mut revents: Vec<PollFlags> = Vec::new();
        loop {
            let timeout = self.timeout().map(|d| Timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ });
            let woke = {
                let mut fds = Vec::with_capacity(1 + self.tcp.len() + self.udp.len() + self.ping.len());
                slots.clear();
                fds.push(PollFd::new(&wait, PollFlags::IN));
                for (k, f) in &self.tcp {
                    let ev = f.interest(&self.sockets);
                    if !ev.is_empty() {
                        fds.push(PollFd::new(&f.stream, ev));
                        slots.push(Slot::Tcp(*k));
                    }
                }
                for (k, f) in &self.udp {
                    fds.push(PollFd::new(&f.sock, PollFlags::IN));
                    slots.push(Slot::Udp(*k));
                }
                for (k, f) in &self.ping {
                    fds.push(PollFd::new(&f.sock, PollFlags::IN));
                    slots.push(Slot::Ping(*k));
                }
                match rustix::event::poll(&mut fds, timeout.as_ref()) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => {
                        eprintln!("emu: net: poll: {e}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                revents.clear();
                revents.extend(fds.iter().map(|f| f.revents()));
                !revents[0].is_empty()
            };
            for (slot, ev) in slots.iter().zip(&revents[1..]) {
                if ev.is_empty() {
                    continue;
                }
                let ready = match slot {
                    Slot::Tcp(k) => self.tcp.get_mut(k).map(|f| &mut f.ready),
                    Slot::Udp(k) => self.udp.get_mut(k).map(|f| &mut f.ready),
                    Slot::Ping(k) => self.ping.get_mut(k).map(|f| &mut f.ready),
                };
                if let Some(r) = ready {
                    *r = *ev;
                }
            }
            if woke {
                let mut b = [0u8; 256];
                while matches!((&wait).read(&mut b), Ok(n) if n > 0) {}
            }
            if self.shared.stop.load(Ordering::Acquire) {
                return;
            }
            self.shared.iterations.fetch_add(1, Ordering::Relaxed);
            self.step();
        }
    }

    /// How long the thread may sleep: smoltcp's next timer (unless the guest's queue is full:
    /// then the device wakes it), or the next idle datagram flow's expiry. None: until woken.
    fn timeout(&mut self) -> Option<Duration> {
        if std::mem::take(&mut self.again) {
            return Some(Duration::ZERO);
        }
        let mut t = if self.wire.blocked {
            None
        } else {
            let now = self.now();
            // A timer due now was due during the pass just run; a floor keeps a socket that
            // can't make progress from spinning the thread.
            self.iface.poll_delay(now, &self.sockets).map(|d| Duration::from_micros(d.total_micros()).max(Duration::from_millis(1)))
        };
        let now = Instant::now();
        let deadlines = self.udp.values().map(|f| f.deadline).chain(self.ping.values().map(|f| f.deadline));
        for d in deadlines {
            let left = d.saturating_duration_since(now);
            t = Some(t.map_or(left, |t| t.min(left)));
        }
        t
    }

    fn step(&mut self) {
        if self.shared.reset.swap(false, Ordering::AcqRel) {
            self.clear();
        }
        std::mem::swap(&mut *lock(&self.shared.to_host), &mut self.inbox);
        self.wire.limit = Some(TO_GUEST.saturating_sub(lock(&self.shared.to_guest).len()));
        self.wire.blocked = false;
        while let Some(f) = self.inbox.pop_front() {
            self.input(f);
        }
        self.poll_iface();
        self.service_connects();
        self.service_tcp();
        self.poll_iface();
        self.reap();
        self.service_datagrams();
        self.expire();
        self.flush();
        self.shared.flows.store(self.tcp.len() + self.udp.len() + self.ping.len(), Ordering::Relaxed);
    }

    fn poll_iface(&mut self) {
        let now = self.now();
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
    }

    /// Hands this pass's frames to the guest's queue and rings the device once.
    fn flush(&mut self) {
        if self.wire.tx.is_empty() && !self.wire.blocked {
            return;
        }
        let any = !self.wire.tx.is_empty();
        let mut q = lock(&self.shared.to_guest);
        q.extend(self.wire.tx.drain(..));
        if self.wire.blocked {
            if q.len() <= TO_GUEST / 2 {
                self.again = true;
            } else {
                self.shared.guest_full.store(true, Ordering::Release);
            }
        }
        drop(q);
        if any {
            self.shared.ring_guest();
        }
    }

    /// Drops every flow: the guest's stack was reset, so none of its connections exist.
    fn clear(&mut self) {
        for f in self.tcp.values() {
            if let TcpState::Open(h) = f.state {
                self.sockets.remove(h);
            }
        }
        self.tcp.clear();
        self.udp.clear();
        self.ping.clear();
        self.wire.rx.clear();
        self.wire.tx.clear();
        self.out.guest_mac = None;
    }

    fn input(&mut self, frame: Vec<u8>) {
        if let Some(m) = packet::src_mac(&frame) {
            self.out.guest_mac = Some(m);
        }
        match packet::parse(&frame) {
            Frame::Arp { request, target } => {
                // smoltcp would answer for any address (any-IP): only the gateway's two
                // are here, and the guest's own (a conflict probe) is not.
                if !request || target == self.cfg.gateway || target == self.cfg.resolver {
                    self.wire.rx.push_back(frame);
                }
            }
            Frame::Tcp { src, dst, syn, ack, rst } => self.tcp_in(frame, (src, dst), syn && !ack && !rst),
            Frame::Udp { src, dst, payload } => self.udp_out(src, dst, payload),
            Frame::Ping { src, dst, ident, message } => self.ping_out(src, dst, ident, message),
            Frame::Other => {}
        }
    }

    /// Where a destination the guest asked for is on the host.
    fn host_addr(&mut self, dst: SocketAddrV4) -> Option<SocketAddr> {
        if *dst.ip() == self.cfg.gateway {
            Some(SocketAddr::new(self.cfg.loopback, dst.port()))
        } else if *dst.ip() == self.cfg.resolver {
            let r = self.resolver.server()?;
            Some(if dst.port() == 53 { r } else { SocketAddr::new(r.ip(), dst.port()) })
        } else {
            Some(SocketAddr::V4(dst))
        }
    }

    fn tcp_in(&mut self, frame: Vec<u8>, key: Key, syn: bool) {
        match self.tcp.get_mut(&key) {
            Some(TcpFlow { state: TcpState::Connecting(held), .. }) => {
                if syn {
                    *held = frame;
                }
            }
            Some(_) => self.wire.rx.push_back(frame),
            None if syn => self.open_tcp(key, frame),
            // No flow (one from before a reset): smoltcp answers with RST.
            None => self.wire.rx.push_back(frame),
        }
    }

    fn open_tcp(&mut self, key: Key, syn: Vec<u8>) {
        let Some(to) = self.host_addr(key.1) else {
            self.wire.rx.push_back(syn);
            return;
        };
        match host::connect(to) {
            Ok((stream, true)) => self.accept_tcp(key, stream, syn),
            Ok((stream, false)) => {
                self.tcp.insert(key, TcpFlow::new(stream, TcpState::Connecting(syn)));
            }
            Err(_) => self.wire.rx.push_back(syn),
        }
    }

    /// The host side connected: a smoltcp socket listening on the destination takes the SYN.
    /// Frames already queued are processed first, so this socket is the only one listening
    /// there when the SYN arrives.
    fn accept_tcp(&mut self, key: Key, stream: TcpStream, syn: Vec<u8>) {
        self.poll_iface();
        let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_RX]), tcp::SocketBuffer::new(vec![0; TCP_TX]));
        s.set_nagle_enabled(false);
        let at = IpListenEndpoint { addr: Some(IpAddress::Ipv4(*key.1.ip())), port: key.1.port() };
        if s.listen(at).is_err() {
            self.wire.rx.push_back(syn);
            return;
        }
        let h = self.sockets.add(s);
        self.wire.rx.push_back(syn);
        self.poll_iface();
        if self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen {
            self.sockets.remove(h);
            return;
        }
        self.tcp.insert(key, TcpFlow::new(stream, TcpState::Open(h)));
    }

    fn service_connects(&mut self) {
        let done: Vec<Key> =
            self.tcp.iter().filter(|(_, f)| matches!(f.state, TcpState::Connecting(_)) && !f.ready.is_empty()).map(|(k, _)| *k).collect();
        for k in done {
            let Some(f) = self.tcp.remove(&k) else { continue };
            let TcpState::Connecting(syn) = f.state else { continue };
            match host::connect_result(&f.stream) {
                Ok(()) => self.accept_tcp(k, f.stream, syn),
                // Refused or unreachable: the SYN finds no socket and smoltcp resets it.
                Err(_) => self.wire.rx.push_back(syn),
            }
        }
    }

    /// Moves data between each connection's smoltcp buffers and its host socket.
    fn service_tcp(&mut self) {
        for f in self.tcp.values_mut() {
            let TcpState::Open(h) = f.state else { continue };
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            let ready = std::mem::replace(&mut f.ready, PollFlags::empty());
            let stream = &f.stream;

            // Guest to host, straight from smoltcp's buffer.
            if !f.write_blocked || ready.intersects(PollFlags::OUT | PollFlags::ERR | PollFlags::HUP) {
                f.write_blocked = false;
                while s.can_recv() {
                    match s.recv(|b| io_result((&mut &*stream).write(b))) {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                            f.write_blocked = true;
                            break;
                        }
                        Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                        Ok(Err(_)) => {
                            s.abort();
                            break;
                        }
                    }
                }
            }
            // The guest closed its half and the host has everything it sent before.
            if !f.host_shut
                && !s.can_recv()
                && matches!(s.state(), tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait)
            {
                let _ = stream.shutdown(Shutdown::Write);
                f.host_shut = true;
            }

            // Host to guest, straight into smoltcp's buffer.
            if !f.host_eof && ready.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                while s.can_send() {
                    match s.send(|b| io_result((&mut &*stream).read(b))) {
                        Ok(Ok(0)) => {
                            f.host_eof = true;
                            s.close();
                            break;
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                        Ok(Err(_)) => {
                            f.host_eof = true;
                            s.abort();
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }

    /// Drops connections smoltcp is done with: closed (after any RST went out) or in
    /// TIME-WAIT (the last ACK went out).
    fn reap(&mut self) {
        let sockets = &mut self.sockets;
        self.tcp.retain(|_, f| {
            let TcpState::Open(h) = f.state else { return true };
            let s = sockets.get::<tcp::Socket>(h);
            let done = match s.state() {
                tcp::State::TimeWait => true,
                tcp::State::Closed => s.remote_endpoint().is_none(),
                _ => false,
            };
            if done {
                sockets.remove(h);
            }
            !done
        });
    }

    fn udp_out(&mut self, src: SocketAddrV4, dst: SocketAddrV4, payload: &[u8]) {
        let key = (src, dst);
        let now = Instant::now();
        if !self.udp.contains_key(&key) {
            let Some(to) = self.host_addr(dst) else { return };
            if self.udp.len() >= MAX_UDP {
                evict(&mut self.udp, |f| f.deadline);
            }
            let Ok(sock) = host::udp(to) else { return };
            let idle = if dst.port() == 53 { DNS_IDLE } else { UDP_IDLE };
            self.udp.insert(key, UdpFlow { sock, ready: PollFlags::empty(), idle, deadline: now });
        }
        if let Some(f) = self.udp.get_mut(&key) {
            f.deadline = now + f.idle;
            let _ = f.sock.send(payload);
        }
    }

    fn ping_out(&mut self, src: Ipv4Addr, dst: Ipv4Addr, ident: u16, message: &[u8]) {
        if dst == self.cfg.gateway || dst == self.cfg.resolver {
            let m = &mut self.scratch[..message.len()];
            m.copy_from_slice(message);
            packet::echo_reply(m, ident);
            self.out.emit(&mut self.wire, dst, src, packet::PROTO_ICMP, m);
            return;
        }
        let key = (dst, ident);
        let now = Instant::now();
        if !self.ping.contains_key(&key) {
            if self.ping.len() >= MAX_PING {
                evict(&mut self.ping, |f| f.deadline);
            }
            let Ok(sock) = host::ping() else { return };
            self.ping.insert(key, PingFlow { sock, guest: src, ready: PollFlags::empty(), deadline: now });
        }
        if let Some(f) = self.ping.get_mut(&key) {
            f.deadline = now + PING_IDLE;
            let _ = f.sock.send_to(message, SocketAddrV4::new(dst, 0));
        }
    }

    /// Replies on the datagram sockets that polled readable, as frames from the address the
    /// guest sent to.
    fn service_datagrams(&mut self) {
        let now = Instant::now();
        for (&(src, dst), f) in self.udp.iter_mut() {
            if std::mem::replace(&mut f.ready, PollFlags::empty()).is_empty() {
                continue;
            }
            for _ in 0..DATAGRAMS_PER_PASS {
                // ECONNREFUSED (an ICMP port unreachable) or no more data: either ends it.
                let Ok(n) = f.sock.recv(&mut self.scratch[packet::UDP_HDR..]) else { break };
                if n > usize::from(u16::MAX) - packet::UDP_HDR {
                    continue;
                }
                let d = &mut self.scratch[..packet::UDP_HDR + n];
                packet::udp_header(d, dst, src);
                self.out.emit(&mut self.wire, *dst.ip(), *src.ip(), packet::PROTO_UDP, d);
                f.deadline = now + f.idle;
            }
        }
        for (&(dst, ident), f) in self.ping.iter_mut() {
            if std::mem::replace(&mut f.ready, PollFlags::empty()).is_empty() {
                continue;
            }
            for _ in 0..DATAGRAMS_PER_PASS {
                let Ok((n, from)) = f.sock.recv_from(&mut self.scratch) else { break };
                let b = &self.scratch[..n];
                let Some(m) = packet::strip_ip_header(b) else { continue };
                let with_ip = m.len() != b.len();
                let off = n - m.len();
                if m.len() < packet::ICMP_HDR || m[0] != packet::ICMP_ECHO_REPLY {
                    continue;
                }
                // Darwin hands every ICMP socket every echo reply, with the identifier as
                // sent; Linux hands this socket only its own, with the identifier it chose.
                let theirs = u16::from_be_bytes([m[4], m[5]]);
                if from.ip() != std::net::IpAddr::V4(dst) || (with_ip && theirs != ident) {
                    continue;
                }
                let m = &mut self.scratch[off..n];
                packet::echo_reply(m, ident);
                self.out.emit(&mut self.wire, dst, f.guest, packet::PROTO_ICMP, m);
                f.deadline = now + PING_IDLE;
            }
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.udp.retain(|_, f| f.deadline > now);
        self.ping.retain(|_, f| f.deadline > now);
    }
}

fn io_result(r: io::Result<usize>) -> (usize, io::Result<usize>) {
    match r {
        Ok(n) => (n, Ok(n)),
        Err(e) => (0, Err(e)),
    }
}

/// Drops the flow closest to expiring, to make room.
fn evict<K: Copy + Eq + std::hash::Hash, V>(flows: &mut HashMap<K, V>, deadline: impl Fn(&V) -> Instant) {
    if let Some(k) = flows.iter().min_by_key(|(_, v)| deadline(v)).map(|(k, _)| *k) {
        flows.remove(&k);
    }
}
