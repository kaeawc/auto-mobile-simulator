//! Frames in, frames out: a guest (raw frames, or a smoltcp stack at 10.0.2.15) talks through
//! the NAT to servers on the host's loopback.
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use super::engine::Wire;
use super::packet::{self, Frame, Link};
use super::*;
use crate::virtio::Doorbell;
use crate::virtio::net::DEFAULT_MAC;

const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GW: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
/// Frames as the guest sends them (ipv4_frames writes the guest's direction).
const FROM_GUEST: Link = Link { gateway_mac: DEFAULT_MAC, guest_mac: GATEWAY_MAC };

fn nat(cfg: Config) -> Nat {
    let mut n = Nat::new(cfg).expect("nat");
    n.attach(RxReady::new(Arc::new(Doorbell::default())));
    n
}

fn deadline(what: &str, start: Instant) {
    assert!(start.elapsed() < Duration::from_secs(10), "timed out: {what}");
    std::thread::sleep(Duration::from_millis(1));
}

/// The next frame from the NAT that `pred` accepts, within `limit`.
fn next_frame(n: &mut Nat, limit: Duration, mut pred: impl FnMut(&[u8]) -> bool) -> Option<Vec<u8>> {
    let start = Instant::now();
    while start.elapsed() < limit {
        while let Some(f) = n.receive() {
            if pred(&f) {
                return Some(f);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}

fn arp_request(target: Ipv4Addr) -> Vec<u8> {
    let mut f = vec![0xff; 6];
    f.extend_from_slice(&DEFAULT_MAC);
    f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
    f.extend_from_slice(&DEFAULT_MAC);
    f.extend_from_slice(&GUEST.octets());
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&target.octets());
    f
}

fn is_arp_reply(f: &[u8]) -> bool {
    matches!(packet::parse(f), Frame::Arp { request: false, .. })
}

fn udp_frame(src: SocketAddrV4, dst: SocketAddrV4, payload: &[u8]) -> Vec<u8> {
    let mut d = vec![0u8; packet::UDP_HDR];
    d.extend_from_slice(payload);
    packet::udp_header(&mut d, src, dst);
    let mut out = Vec::new();
    packet::ipv4_frames(&mut out, FROM_GUEST, *src.ip(), *dst.ip(), packet::PROTO_UDP, 1, &d);
    out.remove(0)
}

fn ping_frame(dst: Ipv4Addr, ident: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut m = vec![packet::ICMP_ECHO_REQUEST, 0, 0, 0];
    m.extend_from_slice(&ident.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(data);
    let c = packet::fold(packet::sum(&m, 0));
    m[2..4].copy_from_slice(&c.to_be_bytes());
    let mut out = Vec::new();
    packet::ipv4_frames(&mut out, FROM_GUEST, GUEST, dst, packet::PROTO_ICMP, 2, &m);
    out.remove(0)
}

/// An IPv4 frame's (header, payload) with both checksums checked.
fn ip_of(f: &[u8]) -> (&[u8], &[u8]) {
    let ip = &f[packet::ETH_HDR..];
    assert_eq!(packet::fold(packet::sum(&ip[..packet::IP_HDR], 0)), 0, "IP checksum");
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    (&ip[..packet::IP_HDR], &ip[packet::IP_HDR..total])
}

fn udp_checksum_ok(src: Ipv4Addr, dst: Ipv4Addr, datagram: &[u8]) -> bool {
    let acc = packet::sum(&src.octets(), 0);
    let acc = packet::sum(&dst.octets(), acc) + u32::from(packet::PROTO_UDP) + datagram.len() as u32;
    packet::fold(packet::sum(datagram, acc)) == 0
}

#[test]
fn the_gateway_answers_arp_for_its_two_addresses_only() {
    let mut n = nat(Config::default());
    n.transmit(&arp_request(GW));
    let r = next_frame(&mut n, Duration::from_secs(5), is_arp_reply).expect("ARP reply for 10.0.2.2");
    assert_eq!(&r[0..6], &DEFAULT_MAC, "to the guest");
    assert_eq!(&r[22..28], &GATEWAY_MAC, "slirp's MAC");
    assert_eq!(&r[28..32], &GW.octets());
    n.transmit(&arp_request(DNS));
    let r = next_frame(&mut n, Duration::from_secs(5), is_arp_reply).expect("ARP reply for 10.0.2.3");
    assert_eq!(&r[28..32], &DNS.octets());

    // The guest's own address (a conflict probe) and the rest of the subnet go unanswered.
    n.transmit(&arp_request(GUEST));
    n.transmit(&arp_request(Ipv4Addr::new(10, 0, 2, 99)));
    assert!(next_frame(&mut n, Duration::from_millis(200), is_arp_reply).is_none());
}

#[test]
fn udp_reaches_the_host_and_replies_come_from_the_address_the_guest_used() {
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let port = server.local_addr().expect("addr").port();
    let echo = std::thread::spawn(move || {
        let mut b = [0u8; 64];
        let (len, from) = server.recv_from(&mut b).expect("recv");
        let mut reply = b"pong:".to_vec();
        reply.extend_from_slice(&b[..len]);
        server.send_to(&reply, from).expect("send");
        b[..len].to_vec()
    });
    let mut n = nat(Config::default());
    let src = SocketAddrV4::new(GUEST, 40000);
    let dst = SocketAddrV4::new(GW, port);
    n.transmit(&udp_frame(src, dst, b"ping"));
    let f = next_frame(&mut n, Duration::from_secs(5), |f| matches!(packet::parse(f), Frame::Udp { .. })).expect("reply");
    assert_eq!(echo.join().expect("echo"), b"ping");
    let (ip, l4) = ip_of(&f);
    assert!(udp_checksum_ok(GW, GUEST, l4), "UDP checksum");
    assert_eq!(packet::parse(&f), Frame::Udp { src: dst, dst: src, payload: b"pong:ping" });
    assert_eq!(ip[8], 64, "TTL");
}

#[test]
fn dns_to_the_resolver_address_is_forwarded_and_a_long_answer_is_fragmented() {
    let resolver = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let at = resolver.local_addr().expect("addr");
    let answer: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    let expect = answer.clone();
    let fake = std::thread::spawn(move || {
        let mut b = [0u8; 512];
        let (len, from) = resolver.recv_from(&mut b).expect("query");
        resolver.send_to(&answer, from).expect("answer");
        b[..len].to_vec()
    });
    let mut n = nat(Config { dns: Dns::Fixed(at), ..Config::default() });
    let src = SocketAddrV4::new(GUEST, 51000);
    let query = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x06google\x03com\x00\x00\x01\x00\x01";
    n.transmit(&udp_frame(src, SocketAddrV4::new(DNS, 53), query));

    // Reassemble the fragments by offset.
    let mut datagram = vec![0u8; 0];
    let start = Instant::now();
    loop {
        let f = next_frame(&mut n, Duration::from_secs(5), |f| f.len() > 34 && f[23] == packet::PROTO_UDP).expect("fragment");
        let (ip, body) = ip_of(&f);
        assert_eq!(&ip[12..16], &DNS.octets(), "from the address the guest asked");
        assert!(packet::IP_HDR + body.len() <= packet::MTU, "fits the guest's MTU");
        let ff = u16::from_be_bytes([ip[6], ip[7]]);
        let off = usize::from(ff & 0x1fff) * 8;
        if datagram.len() < off + body.len() {
            datagram.resize(off + body.len(), 0);
        }
        datagram[off..off + body.len()].copy_from_slice(body);
        if ff & 0x2000 == 0 {
            break;
        }
        deadline("fragments", start);
    }
    assert_eq!(fake.join().expect("resolver"), query, "the query reached the host's resolver");
    assert_eq!(datagram.len(), packet::UDP_HDR + expect.len());
    assert!(udp_checksum_ok(DNS, GUEST, &datagram), "checksum over the reassembled datagram");
    assert_eq!(&datagram[0..2], &53u16.to_be_bytes(), "from port 53");
    assert_eq!(&datagram[packet::UDP_HDR..], &expect[..]);
}

#[test]
fn ping_to_the_gateway_is_answered_and_other_addresses_go_through_an_icmp_socket() {
    let mut n = nat(Config::default());
    let is_reply = |f: &[u8]| f.len() > 34 && f[23] == packet::PROTO_ICMP && f[34] == packet::ICMP_ECHO_REPLY;
    n.transmit(&ping_frame(GW, 0x4242, 1, b"abcdefgh"));
    let f = next_frame(&mut n, Duration::from_secs(5), is_reply).expect("echo reply from 10.0.2.2");
    let (ip, m) = ip_of(&f);
    assert_eq!(&ip[12..16], &GW.octets());
    assert_eq!(packet::fold(packet::sum(m, 0)), 0, "ICMP checksum");
    assert_eq!(&m[4..8], &[0x42, 0x42, 0, 1], "identifier and sequence kept");
    assert_eq!(&m[8..], b"abcdefgh");

    if let Err(e) = host::ping() {
        eprintln!("skipping the ICMP socket half: {e} (Linux: net.ipv4.ping_group_range)");
        return;
    }
    let lo = Ipv4Addr::LOCALHOST;
    n.transmit(&ping_frame(lo, 0x0bad, 7, b"through the host"));
    let f = next_frame(&mut n, Duration::from_secs(5), is_reply).expect("echo reply through the host");
    let (ip, m) = ip_of(&f);
    assert_eq!(&ip[12..16], &lo.octets());
    assert_eq!(&ip[16..20], &GUEST.octets());
    assert_eq!(packet::fold(packet::sum(m, 0)), 0, "ICMP checksum");
    assert_eq!(&m[4..8], &[0x0b, 0xad, 0, 7], "the guest's identifier, whatever the host's socket used");
    assert_eq!(&m[8..], b"through the host");
}

/// A smoltcp stack as the guest's: 10.0.2.15 behind the NAT's gateway.
struct Guest {
    nat: Nat,
    iface: Interface,
    sockets: SocketSet<'static>,
    wire: Wire,
    epoch: Instant,
}

impl Guest {
    fn new(cfg: Config) -> Guest {
        let mut wire = Wire::new(true, None);
        let mut c = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(DEFAULT_MAC)));
        c.random_seed = 7;
        let epoch = Instant::now();
        let mut iface = Interface::new(c, &mut wire, smoltcp::time::Instant::from_millis(0));
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::Ipv4(GUEST), 24)).expect("addr");
        });
        iface.routes_mut().add_default_ipv4_route(GW).expect("route");
        Guest { nat: nat(cfg), iface, sockets: SocketSet::new(vec![]), wire, epoch }
    }

    fn pump(&mut self) {
        let now = smoltcp::time::Instant::from_micros(self.epoch.elapsed().as_micros() as i64);
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
        for f in self.wire.tx.drain(..) {
            self.nat.transmit(&f);
        }
        while let Some(f) = self.nat.receive() {
            self.wire.rx.push_back(f);
        }
        self.iface.poll(now, &mut self.wire, &mut self.sockets);
    }

    fn connect(&mut self, port: u16, local: u16) -> SocketHandle {
        let s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 65536]), tcp::SocketBuffer::new(vec![0; 65536]));
        let h = self.sockets.add(s);
        let cx = self.iface.context();
        self.sockets.get_mut::<tcp::Socket>(h).connect(cx, (IpAddress::Ipv4(GW), port), local).expect("connect");
        h
    }

    fn sock(&mut self, h: SocketHandle) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(h)
    }

    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Guest) -> bool) {
        let start = Instant::now();
        loop {
            self.pump();
            if done(self) {
                return;
            }
            deadline(what, start);
        }
    }
}

#[test]
fn tcp_handshake_transfer_both_ways_and_teardown() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let upload: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7) as u8).collect();
    let download: Vec<u8> = (0..1_500_000u32).map(|i| (i * 13) as u8).collect();
    let reply = download.clone();
    let server = std::thread::spawn(move || {
        let (mut s, peer) = listener.accept().expect("accept");
        let mut got = Vec::new();
        s.read_to_end(&mut got).expect("read to the guest's FIN");
        s.write_all(&reply).expect("write");
        (got, peer)
    });
    let mut g = Guest::new(Config::default());
    let h = g.connect(port, 49152);
    g.until("established", |g| g.sock(h).state() == tcp::State::Established);

    let mut sent = 0;
    g.until("upload", |g| {
        let s = g.sock(h);
        if s.can_send() {
            sent += s.send_slice(&upload[sent..]).expect("send");
        }
        sent == upload.len()
    });
    g.sock(h).close(); // FIN: the server's read_to_end returns
    let mut got = Vec::new();
    g.until("download and the server's FIN", |g| {
        let s = g.sock(h);
        while s.can_recv() {
            s.recv(|b| {
                got.extend_from_slice(b);
                (b.len(), ())
            })
            .expect("recv");
        }
        matches!(s.state(), tcp::State::TimeWait | tcp::State::Closed)
    });
    let (uploaded, peer) = server.join().expect("server");
    assert!(peer.ip().is_loopback(), "10.0.2.2 is the host's loopback");
    assert_eq!(uploaded.len(), upload.len());
    assert!(uploaded == upload, "upload intact");
    assert_eq!(got.len(), download.len());
    assert!(got == download, "download intact");
    let start = Instant::now();
    while g.nat.flows() != 0 {
        g.pump();
        deadline("the flow is reaped", start);
    }
}

#[test]
fn a_refused_connect_resets_the_guest_at_once() {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("addr").port();
    drop(l);
    let mut g = Guest::new(Config::default());
    let h = g.connect(port, 49153);
    let start = Instant::now();
    g.until("RST", |g| g.sock(h).state() == tcp::State::Closed);
    assert!(start.elapsed() < Duration::from_millis(900), "a RST, not the guest's SYN retransmit timer");
    assert_eq!(g.nat.flows(), 0);
}

#[test]
fn an_idle_connection_costs_no_wakeups_and_reattaching_drops_it() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        let mut b = [0u8; 16];
        // Held open until the NAT drops it (EOF or reset).
        let _ = s.read(&mut b);
    });
    let mut g = Guest::new(Config::default());
    let h = g.connect(port, 49154);
    g.until("established", |g| g.sock(h).state() == tcp::State::Established);
    // A UDP flow too, which has an idle expiry.
    g.nat.transmit(&udp_frame(SocketAddrV4::new(GUEST, 40001), SocketAddrV4::new(GW, 9), b"x"));
    for _ in 0..50 {
        g.pump();
        std::thread::sleep(Duration::from_millis(2));
    }
    let before = g.nat.iterations();
    std::thread::sleep(Duration::from_millis(500));
    let woke = g.nat.iterations() - before;
    assert!(woke <= 1, "the net thread woke {woke} times in 500 ms with nothing to do");
    assert_eq!(g.nat.flows(), 2);

    g.nat.attach(RxReady::new(Arc::new(Doorbell::default())));
    let start = Instant::now();
    while g.nat.flows() != 0 {
        deadline("flows dropped on reattach", start);
    }
    server.join().expect("the host side was closed");
}

#[test]
fn a_frame_for_an_unknown_flow_is_reset_not_forwarded() {
    // An ACK for a connection the NAT never saw (the guest's from before a reset).
    let mut tcp = vec![0u8; 20];
    tcp[0..2].copy_from_slice(&40002u16.to_be_bytes());
    tcp[2..4].copy_from_slice(&80u16.to_be_bytes());
    tcp[4..8].copy_from_slice(&1000u32.to_be_bytes());
    tcp[8..12].copy_from_slice(&2000u32.to_be_bytes());
    tcp[12] = 5 << 4;
    tcp[13] = 0x10; // ACK
    tcp[14..16].copy_from_slice(&1024u16.to_be_bytes());
    let dst = Ipv4Addr::new(192, 0, 2, 1);
    let acc = packet::sum(&GUEST.octets(), 0);
    let acc = packet::sum(&dst.octets(), acc) + u32::from(packet::PROTO_TCP) + 20;
    let c = packet::fold(packet::sum(&tcp, acc));
    tcp[16..18].copy_from_slice(&c.to_be_bytes());
    let mut out = Vec::new();
    packet::ipv4_frames(&mut out, FROM_GUEST, GUEST, dst, packet::PROTO_TCP, 3, &tcp);
    let mut n = nat(Config::default());
    n.transmit(&arp_request(GW)); // the NAT learns the guest's MAC
    n.transmit(&out[0]);
    let rst = next_frame(&mut n, Duration::from_secs(5), |f| f.len() >= 54 && f[23] == packet::PROTO_TCP).expect("RST");
    assert_eq!(rst[packet::ETH_HDR + packet::IP_HDR + 13] & 0x04, 0x04, "RST flag");
    assert_eq!(n.flows(), 0);
}

#[test]
fn flow_caps_leave_the_rest_of_emu_its_descriptors() {
    // macOS's default soft limit: the caps and emu's own descriptors fit under it.
    let l = Limits::for_fds(256);
    assert!((l.tcp + l.udp + l.ping) as u64 + RESERVED_FDS <= 256, "{l:?}");
    assert!(l.tcp_connecting <= l.tcp);
    // What emu raises the limit to on macOS, and anything above it.
    let full = Limits { tcp: 1024, tcp_connecting: 256, udp: 512, ping: 64 };
    assert_eq!(Limits::for_fds(10240), full);
    assert_eq!(Limits::for_fds(u64::MAX), full);
    // Even a starved process lets one flow of each kind through.
    let tiny = Limits::for_fds(0);
    assert!(tiny.tcp >= 1 && tiny.tcp_connecting >= 1 && tiny.udp >= 1 && tiny.ping >= 1);
}

#[test]
fn a_syn_past_the_tcp_cap_is_reset() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let limits = Limits { tcp: 2, tcp_connecting: 2, udp: 4, ping: 4 };
    let mut g = Guest::new(Config { limits, ..Config::default() });
    let a = g.connect(port, 49160);
    let b = g.connect(port, 49161);
    g.until("two established", |g| g.sock(a).state() == tcp::State::Established && g.sock(b).state() == tcp::State::Established);
    let c = g.connect(port, 49162);
    let start = Instant::now();
    g.until("the third is reset", |g| g.sock(c).state() == tcp::State::Closed);
    assert!(start.elapsed() < Duration::from_millis(900), "a RST, not the guest's SYN retransmit timer");
    assert_eq!(g.nat.flows(), 2);
    assert_eq!(g.sock(a).state(), tcp::State::Established, "the open ones are untouched");
    drop(listener);
}

#[test]
fn a_dns_flow_is_dropped_once_every_query_is_answered() {
    let resolver = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let at = resolver.local_addr().expect("addr");
    let fake = std::thread::spawn(move || {
        // Two queries from one socket (A and AAAA, as some resolvers send them), then answers.
        let mut b = [0u8; 512];
        let (n1, from) = resolver.recv_from(&mut b).expect("query 1");
        let q1 = b[..n1].to_vec();
        let (n2, _) = resolver.recv_from(&mut b).expect("query 2");
        let q2 = b[..n2].to_vec();
        resolver.send_to(&q1, from).expect("answer 1");
        resolver.send_to(&q2, from).expect("answer 2");
    });
    let mut n = nat(Config { dns: Dns::Fixed(at), ..Config::default() });
    let src = SocketAddrV4::new(GUEST, 51001);
    n.transmit(&udp_frame(src, SocketAddrV4::new(DNS, 53), b"query-a"));
    n.transmit(&udp_frame(src, SocketAddrV4::new(DNS, 53), b"query-aaaa"));
    let is_answer = |f: &[u8]| matches!(packet::parse(f), Frame::Udp { .. });
    let a1 = next_frame(&mut n, Duration::from_secs(5), is_answer).expect("answer 1");
    let a2 = next_frame(&mut n, Duration::from_secs(5), is_answer).expect("answer 2, after the first didn't drop the flow");
    let payload = |f: &[u8]| match packet::parse(f) {
        Frame::Udp { payload, .. } => payload.to_vec(),
        _ => unreachable!(),
    };
    assert_eq!(payload(&a1), b"query-a");
    assert_eq!(payload(&a2), b"query-aaaa");
    fake.join().expect("resolver");
    let start = Instant::now();
    while n.flows() != 0 {
        assert!(start.elapsed() < Duration::from_secs(2), "the answered DNS flow still holds a host socket");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_datagram_reaches_the_guest_whole_or_not_at_all_and_none_wait_forever() {
    const COUNT: u32 = 60;
    // 8008 bytes of UDP: 6 fragments each, so the guest's queue (256 frames) fills mid-datagram.
    const SIZE: usize = 8000;
    assert_eq!(packet::fragments(packet::UDP_HDR + SIZE), 6);
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let port = server.local_addr().expect("addr").port();
    let blast = std::thread::spawn(move || {
        let mut b = [0u8; 16];
        let (_, from) = server.recv_from(&mut b).expect("go");
        for i in 0..COUNT {
            let mut d = vec![i as u8; SIZE];
            d[..4].copy_from_slice(&i.to_be_bytes());
            server.send_to(&d, from).expect("send");
        }
    });
    let mut n = nat(Config::default());
    n.transmit(&udp_frame(SocketAddrV4::new(GUEST, 40010), SocketAddrV4::new(GW, port), b"go"));
    blast.join().expect("server");
    // The guest takes nothing for a while: its queue fills.
    std::thread::sleep(Duration::from_millis(300));

    // Reassemble by IP id: (bytes seen, total once the last fragment is seen).
    let mut seen: std::collections::HashMap<u16, (usize, Option<usize>)> = std::collections::HashMap::new();
    let mut last = Instant::now();
    while last.elapsed() < Duration::from_millis(500) {
        let Some(f) = n.receive() else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        last = Instant::now();
        if f.len() <= 34 || f[23] != packet::PROTO_UDP {
            continue;
        }
        let (ip, body) = ip_of(&f);
        let id = u16::from_be_bytes([ip[4], ip[5]]);
        let ff = u16::from_be_bytes([ip[6], ip[7]]);
        let e = seen.entry(id).or_default();
        e.0 += body.len();
        if ff & 0x2000 == 0 {
            e.1 = Some(usize::from(ff & 0x1fff) * 8 + body.len());
        }
    }
    let whole = seen.values().filter(|(got, total)| *total == Some(*got)).count();
    let partial: Vec<_> = seen.values().filter(|(got, total)| *total != Some(*got)).collect();
    assert!(partial.is_empty(), "datagrams cut mid-way: {partial:?}");
    assert!(whole > 256 / 6, "reading resumed once the guest took frames: {whole} of {COUNT}");
    // Darwin's UDP receive buffer (786 KB) holds every datagram that waited.
    #[cfg(target_vendor = "apple")]
    assert_eq!(whole, COUNT as usize, "none dropped");
}

/// The upload's byte at `at`.
fn upload_byte(at: usize) -> u8 {
    (at.wrapping_mul(11) >> 3) as u8
}

#[test]
fn what_the_guest_sent_before_its_fin_reaches_a_slow_host() {
    enum Cmd {
        Read(usize),
        ReadToEnd,
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (cmd, cmds) = std::sync::mpsc::channel::<Cmd>();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        // The host closes its half first and reads only when told.
        s.shutdown(std::net::Shutdown::Write).expect("half-close");
        let mut got = Vec::new();
        for c in cmds {
            match c {
                Cmd::Read(n) => {
                    let at = got.len();
                    got.resize(at + n, 0);
                    s.read_exact(&mut got[at..]).expect("read");
                }
                Cmd::ReadToEnd => {
                    s.read_to_end(&mut got).expect("read to the guest's FIN");
                    break;
                }
            }
        }
        got
    });
    let mut g = Guest::new(Config::default());
    let h = g.connect(port, 49163);
    g.until("the host's FIN", |g| g.sock(h).state() == tcp::State::CloseWait);

    // Upload until nothing more is acknowledged: the host's socket buffers and the NAT's
    // receive buffer are full, and the rest waits in the guest's.
    let mut sent = 0;
    let (mut acked, mut moved) = (0, Instant::now());
    g.until("every buffer on the way is full", |g| {
        let s = g.sock(h);
        while s.can_send() {
            sent += s
                .send(|b| {
                    for (k, v) in b.iter_mut().enumerate() {
                        *v = upload_byte(sent + k);
                    }
                    (b.len(), b.len())
                })
                .expect("send");
        }
        let now_acked = sent - s.send_queue();
        if now_acked != acked {
            (acked, moved) = (now_acked, Instant::now());
        }
        moved.elapsed() > Duration::from_millis(300)
    });
    // The host takes a little at a time, never more than the NAT has acknowledged, until the
    // guest's queue has reached the NAT. How much each read frees on the way depends on the host's
    // socket buffers (Linux autotunes them), so read in steps rather than one guessed amount. The
    // NAT then acknowledges the FIN while still holding data for the host.
    let mut requested = 0;
    g.until("the guest's queue is sent", |g| {
        let s = g.sock(h);
        if s.send_queue() == 0 {
            return true;
        }
        let acked = sent - s.send_queue();
        let step = (acked - requested.min(acked)).min(16 * 1024);
        if step > 0 && requested + step <= acked {
            requested += step;
            cmd.send(Cmd::Read(step)).expect("server");
        }
        false
    });
    g.sock(h).close();
    g.until("closed", |g| g.sock(h).state() == tcp::State::Closed);
    cmd.send(Cmd::ReadToEnd).expect("server");
    let start = Instant::now();
    while !server.is_finished() {
        g.pump();
        deadline("the host reads to the end", start);
    }
    let got = server.join().expect("server");
    assert_eq!(got.len(), sent, "every byte before the FIN reached the host");
    assert!(got.iter().enumerate().all(|(i, &v)| v == upload_byte(i)), "in order and intact");
    let start = Instant::now();
    while g.nat.flows() != 0 {
        g.pump();
        deadline("the drained flow is reaped", start);
    }
}
