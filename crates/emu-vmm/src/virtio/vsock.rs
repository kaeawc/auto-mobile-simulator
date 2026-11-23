//! virtio-vsock (stream sockets only), terminating guest connections in host pipe services.
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;

use super::queue::Queue;
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};
use crate::pipe::{PipeCtx, PipeHandler, PipeRegistry, WorkerEvent};

pub const ID_VSOCK: u32 = 19;
pub const HOST_CID: u64 = 2;
/// Where the guest's qemu-pipe library connects.
pub const PIPE_PORT: u32 = 5000;

const HDR: usize = 44;
const TYPE_STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;

const RXQ: usize = 0;
const TXQ: usize = 1;
/// Receive buffer we advertise per connection.
const BUF_ALLOC: u32 = 256 * 1024;
const MAX_PKT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hdr {
    src_cid: u64,
    dst_cid: u64,
    src_port: u32,
    dst_port: u32,
    len: u32,
    typ: u16,
    op: u16,
    flags: u32,
    buf_alloc: u32,
    fwd_cnt: u32,
}

impl Hdr {
    fn parse(b: &[u8]) -> Option<Hdr> {
        if b.len() < HDR {
            return None;
        }
        let u64_ = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
        let u32_ = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
        let u16_ = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        Some(Hdr {
            src_cid: u64_(0),
            dst_cid: u64_(8),
            src_port: u32_(16),
            dst_port: u32_(20),
            len: u32_(24),
            typ: u16_(28),
            op: u16_(30),
            flags: u32_(32),
            buf_alloc: u32_(36),
            fwd_cnt: u32_(40),
        })
    }

    fn encode(&self) -> [u8; HDR] {
        let mut b = [0u8; HDR];
        b[0..8].copy_from_slice(&self.src_cid.to_le_bytes());
        b[8..16].copy_from_slice(&self.dst_cid.to_le_bytes());
        b[16..20].copy_from_slice(&self.src_port.to_le_bytes());
        b[20..24].copy_from_slice(&self.dst_port.to_le_bytes());
        b[24..28].copy_from_slice(&self.len.to_le_bytes());
        b[28..30].copy_from_slice(&self.typ.to_le_bytes());
        b[30..32].copy_from_slice(&self.op.to_le_bytes());
        b[32..36].copy_from_slice(&self.flags.to_le_bytes());
        b[36..40].copy_from_slice(&self.buf_alloc.to_le_bytes());
        b[40..44].copy_from_slice(&self.fwd_cnt.to_le_bytes());
        b
    }
}

pub struct Vsock {
    guest_cid: u64,
    registry: Arc<PipeRegistry>,
    events: Sender<WorkerEvent>,
    rx: Option<Receiver<WorkerEvent>>,
    worker: Option<JoinHandle<Receiver<WorkerEvent>>>,
}

/// The vsock worker's event channel, created first so host services can hold a sender.
pub fn event_channel() -> (Sender<WorkerEvent>, Receiver<WorkerEvent>) {
    channel()
}

impl Vsock {
    pub fn new(guest_cid: u64, registry: Arc<PipeRegistry>, events: Sender<WorkerEvent>, rx: Receiver<WorkerEvent>) -> Vsock {
        Vsock { guest_cid, registry, events, rx: Some(rx), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.events.send(WorkerEvent::Stop);
            if let Ok(rx) = h.join() {
                // Keep the receiver: host threads hold senders for the device's lifetime.
                while rx.try_recv().is_ok() {}
                self.rx = Some(rx);
            }
        }
    }
}

impl Drop for Vsock {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Vsock {
    fn device_id(&self) -> u32 {
        ID_VSOCK
    }
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        3
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = self.guest_cid.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let rxq = act.queues.get(RXQ).copied().flatten().ok_or("vsock rx queue not enabled")?;
        let txq = act.queues.get(TXQ).copied().flatten().ok_or("vsock tx queue not enabled")?;
        self.stop_worker();
        let rx = self.rx.take().ok_or("vsock worker receiver missing")?;
        let mut w = Worker {
            mem: act.mem,
            irq: act.irq,
            rxq: Queue::new(rxq, false),
            txq: Queue::new(txq, false),
            guest_cid: self.guest_cid,
            registry: self.registry.clone(),
            events: self.events.clone(),
            conns: HashMap::new(),
            ids: HashMap::new(),
            next_id: 1,
            next_host_port: 50_000,
            control: VecDeque::new(),
        };
        let handle = std::thread::Builder::new()
            .name("vsock".into())
            .spawn(move || {
                w.run(&rx);
                w.close_all();
                rx
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some(handle);
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        let _ = self.events.send(WorkerEvent::Kick);
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

/// (guest port, host port)
type Key = (u32, u32);

struct Conn {
    id: u64,
    /// False until the guest answers a host-initiated REQUEST.
    connected: bool,
    /// For host-initiated connections still waiting on the guest: what to run on refusal, and
    /// the handler to build on acceptance (so host I/O starts only once the guest listens).
    refused: Option<Box<dyn FnOnce() + Send>>,
    pending: Option<crate::pipe::MakeHandler>,
    /// Collecting `pipe:<name>\0` until the handler exists.
    name_buf: Option<Vec<u8>>,
    handler: Option<Box<dyn PipeHandler>>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    tx_cnt: u32,
    fwd_cnt: u32,
    advertised_fwd: u32,
    out: VecDeque<u8>,
    closing: bool,
}

impl Conn {
    fn credit(&self) -> usize {
        self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt)) as usize
    }
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    rxq: Queue,
    txq: Queue,
    guest_cid: u64,
    registry: Arc<PipeRegistry>,
    events: Sender<WorkerEvent>,
    conns: HashMap<Key, Conn>,
    ids: HashMap<u64, Key>,
    next_id: u64,
    next_host_port: u32,
    /// Header-only packets (responses, resets, credit updates) waiting for rx buffers.
    control: VecDeque<Hdr>,
}

impl Worker {
    fn run(&mut self, rx: &Receiver<WorkerEvent>) {
        while let Ok(ev) = rx.recv() {
            let mut ev = Some(ev);
            // Drain everything queued before touching the rings again.
            while let Some(e) = ev.take() {
                match e {
                    WorkerEvent::Stop => return,
                    WorkerEvent::Kick => self.process_tx(),
                    WorkerEvent::Host { conn, msg } => self.host_msg(conn, msg),
                    WorkerEvent::Connect { guest_port, make, refused } => self.connect(guest_port, make, refused),
                }
                ev = rx.try_recv().ok();
            }
            self.flush();
        }
    }

    fn connect(&mut self, guest_port: u32, make: crate::pipe::MakeHandler, refused: Box<dyn FnOnce() + Send>) {
        let host_port = loop {
            let p = self.next_host_port;
            self.next_host_port = if p >= 60_000 { 50_000 } else { p + 1 };
            if !self.conns.contains_key(&(guest_port, p)) {
                break p;
            }
        };
        let key = (guest_port, host_port);
        let id = self.next_id;
        self.next_id += 1;
        let conn = Conn {
            id,
            connected: false,
            refused: Some(refused),
            pending: Some(make),
            name_buf: None,
            handler: None,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            fwd_cnt: 0,
            advertised_fwd: 0,
            out: VecDeque::new(),
            closing: false,
        };
        self.control.push_back(reply(self.guest_cid, key, OP_REQUEST, 0, 0));
        self.conns.insert(key, conn);
        self.ids.insert(id, key);
    }

    fn process_tx(&mut self) {
        let mut done = false;
        loop {
            let chain = match self.txq.pop(&self.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("emu: vsock tx: {e}");
                    break;
                }
            };
            if let Ok(pkt) = chain.read_all(&self.mem, HDR + MAX_PKT)
                && let Some(h) = Hdr::parse(&pkt)
            {
                let end = (HDR + h.len as usize).min(pkt.len());
                self.guest_packet(h, &pkt[HDR..end]);
            }
            let _ = self.txq.add_used(&self.mem, chain.head, 0);
            done = true;
        }
        if done && self.txq.should_notify(&self.mem) {
            self.irq.raise(INT_USED_RING);
        }
    }

    fn guest_packet(&mut self, h: Hdr, payload: &[u8]) {
        if h.dst_cid != HOST_CID || h.typ != TYPE_STREAM {
            if h.op != OP_RST {
                self.control.push_back(reply(self.guest_cid, (h.src_port, h.dst_port), OP_RST, 0, 0));
            }
            return;
        }
        let key = (h.src_port, h.dst_port);
        if h.op == OP_REQUEST {
            if h.dst_port != PIPE_PORT || self.conns.contains_key(&key) {
                self.control.push_back(reply(self.guest_cid, key, OP_RST, 0, 0));
                return;
            }
            let id = self.next_id;
            self.next_id += 1;
            let conn = Conn {
                id,
                connected: true,
                refused: None,
                pending: None,
                name_buf: Some(Vec::new()),
                handler: None,
                peer_buf_alloc: h.buf_alloc,
                peer_fwd_cnt: h.fwd_cnt,
                tx_cnt: 0,
                fwd_cnt: 0,
                advertised_fwd: 0,
                out: VecDeque::new(),
                closing: false,
            };
            self.control.push_back(reply(self.guest_cid, key, OP_RESPONSE, 0, conn.fwd_cnt));
            self.conns.insert(key, conn);
            self.ids.insert(id, key);
            return;
        }
        let Some(conn) = self.conns.get_mut(&key) else {
            if h.op != OP_RST {
                self.control.push_back(reply(self.guest_cid, key, OP_RST, 0, 0));
            }
            return;
        };
        conn.peer_buf_alloc = h.buf_alloc;
        conn.peer_fwd_cnt = h.fwd_cnt;
        match h.op {
            OP_RW => {
                conn.fwd_cnt = conn.fwd_cnt.wrapping_add(payload.len() as u32);
                let keep = self.deliver(key, payload);
                if !keep {
                    self.close(key, true);
                    return;
                }
                if let Some(c) = self.conns.get(&key)
                    && c.fwd_cnt.wrapping_sub(c.advertised_fwd) >= BUF_ALLOC / 2
                {
                    let hdr = reply(self.guest_cid, key, OP_CREDIT_UPDATE, 0, c.fwd_cnt);
                    self.control.push_back(hdr);
                    if let Some(c) = self.conns.get_mut(&key) {
                        c.advertised_fwd = c.fwd_cnt;
                    }
                }
            }
            OP_CREDIT_REQUEST => {
                let hdr = reply(self.guest_cid, key, OP_CREDIT_UPDATE, 0, conn.fwd_cnt);
                conn.advertised_fwd = conn.fwd_cnt;
                self.control.push_back(hdr);
            }
            OP_CREDIT_UPDATE => {}
            OP_RESPONSE => {
                conn.connected = true;
                conn.refused = None;
                if let Some(make) = conn.pending.take() {
                    conn.handler = Some(make(PipeCtx { conn: conn.id, events: self.events.clone() }));
                }
            }
            OP_SHUTDOWN => self.close(key, true),
            OP_RST => {
                if let Some(refused) = conn.refused.take() {
                    // Nothing listening: drop silently, let the requester retry.
                    if let Some(c) = self.conns.remove(&key) {
                        self.ids.remove(&c.id);
                    }
                    refused();
                } else {
                    self.close(key, false);
                }
            }
            _ => {}
        }
    }

    /// Feeds guest bytes to the connection's pipe (opening it once the name is complete).
    fn deliver(&mut self, key: Key, mut data: &[u8]) -> bool {
        let Some(conn) = self.conns.get_mut(&key) else { return false };
        if let Some(name_buf) = conn.name_buf.as_mut() {
            match data.iter().position(|&b| b == 0) {
                None => {
                    name_buf.extend_from_slice(data);
                    return name_buf.len() < 256;
                }
                Some(nul) => {
                    name_buf.extend_from_slice(&data[..nul]);
                    data = &data[nul + 1..];
                    let raw = conn.name_buf.take().unwrap_or_default();
                    let full = String::from_utf8_lossy(&raw).into_owned();
                    let name = full.strip_prefix("pipe:").unwrap_or(&full).to_string();
                    let ctx = PipeCtx { conn: conn.id, events: self.events.clone() };
                    match self.registry.open(&name, ctx) {
                        Some(h) => conn.handler = Some(h),
                        None => {
                            eprintln!("emu: guest opened unknown pipe '{name}'");
                            return false;
                        }
                    }
                }
            }
        }
        if data.is_empty() {
            return true;
        }
        let Some(h) = conn.handler.as_mut() else { return false };
        let mut to_guest = Vec::new();
        let keep = h.guest_data(data, &mut to_guest);
        conn.out.extend(to_guest);
        keep
    }

    fn host_msg(&mut self, id: u64, msg: crate::pipe::HostMsg) {
        let Some(&key) = self.ids.get(&id) else { return };
        let Some(conn) = self.conns.get_mut(&key) else { return };
        let Some(h) = conn.handler.as_mut() else { return };
        let mut to_guest = Vec::new();
        let keep = h.host_msg(msg, &mut to_guest);
        conn.out.extend(to_guest);
        if !keep {
            conn.closing = true;
        }
    }

    fn close(&mut self, key: Key, send_rst: bool) {
        if let Some(mut c) = self.conns.remove(&key) {
            self.ids.remove(&c.id);
            if let Some(h) = c.handler.as_mut() {
                h.closed();
            }
            if send_rst {
                self.control.push_back(reply(self.guest_cid, key, OP_RST, 0, 0));
            }
        }
    }

    fn close_all(&mut self) {
        let keys: Vec<Key> = self.conns.keys().copied().collect();
        for k in keys {
            self.close(k, false);
        }
    }

    /// Moves control packets and pending data into guest rx buffers.
    fn flush(&mut self) {
        let mut used = false;
        while let Some(work) = self.next_rx_work() {
            let chain = match self.rxq.pop(&self.mem) {
                Ok(Some(c)) => c,
                _ => {
                    self.unpop(work);
                    break;
                }
            };
            let cap: usize = chain.writable().map(|d| d.len as usize).sum::<usize>().saturating_sub(HDR);
            let (hdr, payload) = match work {
                RxWork::Control(h) => (h, Vec::new()),
                RxWork::Data(key) => {
                    let Some(c) = self.conns.get_mut(&key) else {
                        let _ = self.rxq.add_used(&self.mem, chain.head, 0);
                        used = true;
                        continue;
                    };
                    let n = c.out.len().min(c.credit()).min(cap).min(MAX_PKT);
                    let payload: Vec<u8> = c.out.drain(..n).collect();
                    c.tx_cnt = c.tx_cnt.wrapping_add(n as u32);
                    c.advertised_fwd = c.fwd_cnt;
                    (reply(self.guest_cid, key, OP_RW, n as u32, c.fwd_cnt), payload)
                }
            };
            let mut pkt = hdr.encode().to_vec();
            pkt.extend_from_slice(&payload);
            let written = chain.write_all(&self.mem, &pkt).unwrap_or(0);
            let _ = self.rxq.add_used(&self.mem, chain.head, written);
            used = true;
        }
        // Connections whose host side finished and whose data has drained.
        let done: Vec<Key> = self.conns.iter().filter(|(_, c)| c.closing && c.out.is_empty()).map(|(k, _)| *k).collect();
        for k in done {
            self.close(k, true);
        }
        if used && self.rxq.should_notify(&self.mem) {
            self.irq.raise(INT_USED_RING);
        }
    }

    fn next_rx_work(&mut self) -> Option<RxWork> {
        if let Some(h) = self.control.pop_front() {
            return Some(RxWork::Control(h));
        }
        self.conns.iter().find(|(_, c)| c.connected && !c.out.is_empty() && c.credit() > 0).map(|(k, _)| RxWork::Data(*k))
    }

    fn unpop(&mut self, work: RxWork) {
        if let RxWork::Control(h) = work {
            self.control.push_front(h);
        }
    }
}

/// A host→guest header for connection `key` (guest port, host port).
fn reply(guest_cid: u64, key: Key, op: u16, len: u32, fwd_cnt: u32) -> Hdr {
    Hdr {
        src_cid: HOST_CID,
        dst_cid: guest_cid,
        src_port: key.1,
        dst_port: key.0,
        len,
        typ: TYPE_STREAM,
        op,
        flags: 0,
        buf_alloc: BUF_ALLOC,
        fwd_cnt,
    }
}

enum RxWork {
    Control(Hdr),
    Data(Key),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h =
            Hdr { src_cid: 3, dst_cid: 2, src_port: 1234, dst_port: 5000, len: 7, typ: 1, op: OP_RW, flags: 0, buf_alloc: 9, fwd_cnt: 4 };
        assert_eq!(Hdr::parse(&h.encode()), Some(h));
        assert!(Hdr::parse(&[0u8; 10]).is_none());
    }
}
