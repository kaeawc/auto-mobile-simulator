//! virtio-console with multiport: named byte-stream ports between guest services and the host.
//!
//! The guest's `qemu-props` walks `/sys/class/virtio-ports`, reads each port's name and sets
//! `vendor.qemu.vport.<name>=/dev/vportNpM`; HALs open their port by that property (the radio
//! HAL's `modem`, the GNSS HAL's `gnss`, ...). Unnamed ports are console ports: the guest makes
//! each one the next `/dev/hvcN` in port order (port 0 is `hvc0`; the next unnamed one `hvc1`,
//! where the ranchu images' `goldfish-logcat` writes `logcat -f /dev/hvc1`, as on stock's board).
//!
//! Queues: port 0 rx/tx are 0/1, control rx/tx are 2/3, port n ≥ 1 uses 2n+2 / 2n+3. Control
//! messages are `{ id: u32, event: u16, value: u16 }` plus a payload for PORT_NAME. Handshake:
//! guest DEVICE_READY → host DEVICE_ADD per port → guest PORT_READY → host PORT_NAME (named
//! ports) or CONSOLE_PORT (port 0), then PORT_OPEN(1) since the host end is always connected.
//! The guest reports its own opens with PORT_OPEN; data for a port the guest hasn't opened is
//! held until it does (the driver would discard it).
use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use super::mmio::F_RING_EVENT_IDX;
use super::queue::{Chain, Queue};
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};
use emu_hvf::GuestRam;

pub const ID_CONSOLE: u32 = 3;
const F_MULTIPORT: u64 = 1 << 1;

mod ctl {
    pub const DEVICE_READY: u16 = 0;
    pub const DEVICE_ADD: u16 = 1;
    pub const PORT_READY: u16 = 3;
    pub const CONSOLE_PORT: u16 = 4;
    pub const PORT_OPEN: u16 = 6;
    pub const PORT_NAME: u16 = 7;
}

/// The host end of one port.
pub trait PortBackend: Send {
    /// Bytes the guest wrote; returns bytes to send back.
    fn guest_data(&mut self, data: &[u8]) -> Vec<u8>;
    /// The guest opened (`true`) or closed the port; returns bytes to send once open.
    fn guest_open(&mut self, _open: bool) -> Vec<u8> {
        Vec::new()
    }
}

/// Discards guest output (port 0 when nothing listens).
pub struct Sink;

impl PortBackend for Sink {
    fn guest_data(&mut self, _data: &[u8]) -> Vec<u8> {
        Vec::new()
    }
}

/// Writes guest output to a [`Spool`] (a log file, stdout), sending nothing back. The worker
/// only queues it: a slow or stuck destination (a full pipe nobody reads) drops output instead of
/// stalling the worker, which also serves the other ports (the radio HAL's `modem`).
pub struct Writer(pub Spool);

impl PortBackend for Writer {
    fn guest_data(&mut self, data: &[u8]) -> Vec<u8> {
        self.0.send(data);
        Vec::new()
    }
}

/// A bounded queue in front of a writer, drained by its own thread. [`Spool::send`] never
/// blocks: when `depth` chunks are already waiting it drops the chunk and counts its bytes; the
/// writer thread notes the loss in the stream (`[emu: N bytes dropped ...]`) before the next
/// chunk it writes. Clones share the queue; the thread ends when every clone is gone.
#[derive(Clone)]
pub struct Spool {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    /// Bytes dropped so far.
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// Dropped but not yet noted in the stream.
    unreported: Arc<std::sync::atomic::AtomicU64>,
}

impl Spool {
    pub fn new(name: &str, mut out: Box<dyn std::io::Write + Send>, depth: usize) -> std::io::Result<Spool> {
        use std::sync::atomic::Ordering::Relaxed;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(depth);
        let spool = Spool { tx, dropped: Default::default(), unreported: Default::default() };
        let unreported = spool.unreported.clone();
        std::thread::Builder::new().name(name.into()).spawn(move || {
            // Writes logcat to a file: nothing in the guest waits on it.
            emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
            for chunk in rx {
                let lost = unreported.swap(0, Relaxed);
                if lost > 0 {
                    let _ = write!(out, "\n[emu: {lost} bytes dropped: this output fell behind]\n");
                }
                // A failed write (a full disk, a closed pipe) loses that chunk, nothing more.
                let _ = out.write_all(&chunk).and_then(|()| out.flush());
            }
        })?;
        Ok(spool)
    }

    /// Queues `data`, or drops it when the queue is full or the writer has gone.
    pub fn send(&self, data: &[u8]) {
        use std::sync::atomic::Ordering::Relaxed;
        if data.is_empty() {
            return;
        }
        if self.tx.try_send(data.to_vec()).is_err() {
            self.dropped.fetch_add(data.len() as u64, Relaxed);
            self.unreported.fetch_add(data.len() as u64, Relaxed);
        }
    }

    /// Bytes dropped so far because the writer fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct PortSpec {
    /// Empty for a console port (`hvcN`); else the name the guest finds it by.
    pub name: String,
    pub backend: Box<dyn PortBackend>,
}

enum Msg {
    Kick,
    Host { port: u32, data: Vec<u8> },
    Stop,
}

/// Pushes host-originated bytes to a port (unsolicited modem codes, GNSS sentences, ...).
#[derive(Clone)]
pub struct PortHandle {
    tx: Sender<Msg>,
    port: u32,
}

impl PortHandle {
    pub fn send(&self, data: Vec<u8>) {
        if !data.is_empty() {
            let _ = self.tx.send(Msg::Host { port: self.port, data });
        }
    }
}

struct Port {
    name: String,
    backend: Box<dyn PortBackend>,
    guest_open: bool,
    /// Bytes waiting for the guest to open the port or post receive buffers.
    pending: VecDeque<u8>,
}

pub struct Console {
    ports: Option<Vec<Port>>,
    nr_ports: u32,
    tx: Sender<Msg>,
    rx: Option<Receiver<Msg>>,
    worker: Option<JoinHandle<(Receiver<Msg>, Vec<Port>)>>,
}

impl Console {
    /// `ports[0]` is the console port; the rest are named. Returns one handle per port.
    pub fn new(ports: Vec<PortSpec>) -> (Console, Vec<PortHandle>) {
        let (tx, rx) = channel();
        let nr_ports = ports.len() as u32;
        let handles = (0..nr_ports).map(|port| PortHandle { tx: tx.clone(), port }).collect();
        let ports =
            ports.into_iter().map(|p| Port { name: p.name, backend: p.backend, guest_open: false, pending: VecDeque::new() }).collect();
        (Console { ports: Some(ports), nr_ports, tx, rx: Some(rx), worker: None }, handles)
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.tx.send(Msg::Stop);
            if let Ok((rx, ports)) = h.join() {
                self.rx = Some(rx);
                self.ports = Some(ports);
            }
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Console {
    fn device_id(&self) -> u32 {
        ID_CONSOLE
    }
    /// Event-idx: transmit queues are drained with kicks off, and the guest kicks a receive
    /// queue only once the device has taken all its buffers.
    fn features(&self) -> u64 {
        F_MULTIPORT | F_RING_EVENT_IDX
    }
    fn num_queues(&self) -> usize {
        2 + 2 * self.nr_ports as usize
    }
    fn queue_max_size(&self) -> u16 {
        128
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // cols u16, rows u16, max_nr_ports u32, emerg_wr u32.
        let mut cfg = [0u8; 12];
        cfg[4..8].copy_from_slice(&self.nr_ports.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        self.stop_worker();
        let rx = self.rx.take().ok_or("console worker receiver missing")?;
        let mut ports = self.ports.take().ok_or("console ports missing")?;
        for p in &mut ports {
            p.guest_open = false;
            p.pending.clear();
        }
        let event_idx = act.features & F_RING_EVENT_IDX != 0;
        let queues: Vec<Option<Queue>> = act.queues.iter().map(|q| q.map(|c| Queue::new(c, event_idx))).collect();
        let used = vec![false; queues.len()];
        let mut w = Worker { mem: act.mem, irq: act.irq, queues, used, ports, ctl_out: VecDeque::new() };
        let h = std::thread::Builder::new()
            .name("virtio-console".into())
            .spawn(move || {
                crate::device_thread();
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Stop => break,
                        Msg::Kick => w.service(),
                        Msg::Host { port, data } => {
                            if let Some(p) = w.ports.get_mut(port as usize) {
                                p.pending.extend(data);
                            }
                            w.flush();
                        }
                    }
                }
                (rx, w.ports)
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some(h);
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        let _ = self.tx.send(Msg::Kick);
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

fn rx_queue(port: u32) -> usize {
    if port == 0 { 0 } else { 2 * port as usize + 2 }
}

fn tx_queue(port: u32) -> usize {
    rx_queue(port) + 1
}

const CTL_RX: usize = 2;
const CTL_TX: usize = 3;

fn ctl_msg(id: u32, event: u16, value: u16, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + payload.len());
    m.extend_from_slice(&id.to_le_bytes());
    m.extend_from_slice(&event.to_le_bytes());
    m.extend_from_slice(&value.to_le_bytes());
    m.extend_from_slice(payload);
    m
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    queues: Vec<Option<Queue>>,
    /// Queues given used entries since the last interrupt check (by queue index).
    used: Vec<bool>,
    ports: Vec<Port>,
    /// Control messages waiting for control receive buffers.
    ctl_out: VecDeque<Vec<u8>>,
}

impl Worker {
    /// Drains every transmit queue (guest → host), then delivers what's pending.
    /// Transmit queues are drained with kicks off ([`Queue::pop_draining`]), so what the guest
    /// writes meanwhile costs it no exit.
    fn service(&mut self) {
        let mut c = Chain::default();
        // Control messages from the guest.
        let mut incoming = Vec::new();
        if let Some(q) = self.queues.get_mut(CTL_TX).and_then(Option::as_mut) {
            while let Ok(true) = q.pop_draining(&self.mem, &mut c) {
                if let Ok(b) = c.read_all(&self.mem, 4096) {
                    incoming.push(b);
                }
                let _ = q.add_used(&self.mem, c.head, 0);
                self.used[CTL_TX] = true;
            }
        }
        for m in incoming {
            self.control(&m);
        }
        // Port data from the guest.
        for port in 0..self.ports.len() as u32 {
            let mut data = Vec::new();
            let qi = tx_queue(port);
            if let Some(q) = self.queues.get_mut(qi).and_then(Option::as_mut) {
                while let Ok(true) = q.pop_draining(&self.mem, &mut c) {
                    if let Ok(b) = c.read_all(&self.mem, 1 << 16) {
                        data.extend_from_slice(&b);
                    }
                    let _ = q.add_used(&self.mem, c.head, 0);
                    self.used[qi] = true;
                }
            }
            if !data.is_empty() {
                let p = &mut self.ports[port as usize];
                let reply = p.backend.guest_data(&data);
                p.pending.extend(reply);
            }
        }
        // The flush raises one interrupt for both directions.
        self.flush();
    }

    fn control(&mut self, m: &[u8]) {
        if m.len() < 8 {
            return;
        }
        let id = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
        let event = u16::from_le_bytes([m[4], m[5]]);
        let value = u16::from_le_bytes([m[6], m[7]]);
        match event {
            ctl::DEVICE_READY if value == 1 => {
                for port in 0..self.ports.len() as u32 {
                    self.ctl_out.push_back(ctl_msg(port, ctl::DEVICE_ADD, 0, &[]));
                }
            }
            ctl::PORT_READY if value == 1 => {
                let Some(p) = self.ports.get(id as usize) else { return };
                if p.name.is_empty() {
                    self.ctl_out.push_back(ctl_msg(id, ctl::CONSOLE_PORT, 1, &[]));
                } else {
                    let name = p.name.clone();
                    self.ctl_out.push_back(ctl_msg(id, ctl::PORT_NAME, 1, name.as_bytes()));
                }
                self.ctl_out.push_back(ctl_msg(id, ctl::PORT_OPEN, 1, &[]));
            }
            ctl::PORT_OPEN => {
                let Some(p) = self.ports.get_mut(id as usize) else { return };
                p.guest_open = value == 1;
                if !p.guest_open {
                    p.pending.clear();
                }
                let greet = p.backend.guest_open(value == 1);
                p.pending.extend(greet);
            }
            _ => {}
        }
    }

    /// Fills guest receive buffers: control first, then each open port's pending bytes.
    fn flush(&mut self) {
        if let Some(q) = self.queues.get_mut(CTL_RX).and_then(Option::as_mut) {
            while let Some(m) = self.ctl_out.front() {
                let Ok(Some(c)) = q.pop(&self.mem) else { break };
                let n = c.write_all(&self.mem, m).unwrap_or(0);
                let _ = q.add_used(&self.mem, c.head, n);
                self.ctl_out.pop_front();
                self.used[CTL_RX] = true;
            }
        }
        for port in 0..self.ports.len() as u32 {
            let p = &mut self.ports[port as usize];
            if !p.guest_open || p.pending.is_empty() {
                continue;
            }
            let qi = rx_queue(port);
            let Some(q) = self.queues.get_mut(qi).and_then(Option::as_mut) else { continue };
            while !p.pending.is_empty() {
                let Ok(Some(c)) = q.pop(&self.mem) else { break };
                let room: usize = c.writable().map(|d| d.len as usize).sum();
                let take = room.min(p.pending.len());
                let chunk: Vec<u8> = p.pending.drain(..take).collect();
                let n = c.write_all(&self.mem, &chunk).unwrap_or(0);
                let _ = q.add_used(&self.mem, c.head, n);
                self.used[qi] = true;
            }
        }
        // Every queue that got used entries is asked (each records the check), and the guest
        // gets one interrupt if any wants it.
        let mut raise = false;
        for (q, used) in self.queues.iter_mut().zip(self.used.iter_mut()) {
            if let (true, Some(q)) = (std::mem::take(used), q.as_mut()) {
                raise |= q.should_notify(&self.mem);
            }
        }
        if raise {
            self.irq.raise(INT_USED_RING);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::irq::TestLine;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    /// A modem-like backend: greets on open, answers each write.
    struct Modem(Arc<std::sync::Mutex<Vec<String>>>);

    impl PortBackend for Modem {
        fn guest_data(&mut self, data: &[u8]) -> Vec<u8> {
            self.0.lock().expect("log").push(String::from_utf8_lossy(data).into_owned());
            b"OK\r\n".to_vec()
        }
        fn guest_open(&mut self, open: bool) -> Vec<u8> {
            self.0.lock().expect("log").push(format!("open={open}"));
            if open { b"READY\r\n".to_vec() } else { Vec::new() }
        }
    }

    /// The guest's virtio_console driver for two ports: receive queues get buffers up front.
    struct Guest {
        mem: Arc<GuestRam>,
        dev: Console,
        q: Vec<Driver>,
        seen: Vec<u16>,
        tx_next: u64,
        host: Vec<PortHandle>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
        event_idx: bool,
        /// Kicks sent (MMIO exits on a real guest).
        kicks: u32,
    }

    const BUFS: u64 = RAM_BASE + 0x10_0000;

    fn guest() -> Guest {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x40_0000).expect("ram"));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut dev, host) = Console::new(vec![
            PortSpec { name: String::new(), backend: Box::new(Sink) },
            PortSpec { name: "modem".into(), backend: Box::new(Modem(log.clone())) },
        ]);
        let mut q: Vec<Driver> = (0..6).map(|i| Driver::at(RAM_BASE + 0x1000 + i * 0x2000, 16)).collect();
        // Receive queues (port 0, control, port 1): eight 64-byte buffers each.
        for rx in [0usize, CTL_RX, rx_queue(1)] {
            for b in 0..8u64 {
                q[rx].push(&mem, &[(BUFS + (rx as u64 * 8 + b) * 64, 64, true)]);
            }
        }
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let features = dev.features();
        dev.activate(Activation { mem: mem.clone(), irq, queues: q.iter().map(|d| Some(d.cfg)).collect(), features }).expect("activate");
        let event_idx = features & F_RING_EVENT_IDX != 0;
        Guest { mem, dev, q, seen: vec![0; 6], tx_next: RAM_BASE + 0x20_0000, host, log, event_idx, kicks: 0 }
    }

    impl Guest {
        /// Publishes `bytes` on `queue`, kicking only when the device asked for it.
        fn send(&mut self, queue: usize, bytes: &[u8]) {
            self.mem.write(self.tx_next, bytes).expect("tx");
            let old = self.q[queue].avail_idx();
            self.q[queue].push(&self.mem, &[(self.tx_next, bytes.len() as u32, false)]);
            self.tx_next += 0x100;
            self.kick_if_asked(queue, old);
        }
        fn kick_if_asked(&mut self, queue: usize, old: u16) -> bool {
            let kick = self.q[queue].needs_kick(&self.mem, old, self.event_idx);
            if kick {
                self.kicks += 1;
                self.dev.notify(queue as u16);
            }
            kick
        }
        fn control(&mut self, id: u32, event: u16, value: u16) {
            self.send(CTL_TX, &ctl_msg(id, event, value, &[]));
        }
        /// The next buffer the device filled on receive queue `rx`.
        fn recv(&mut self, rx: usize) -> Vec<u8> {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.q[rx].used_idx(&self.mem) == self.seen[rx] {
                assert!(Instant::now() < deadline, "nothing on queue {rx}");
                std::thread::sleep(Duration::from_millis(1));
            }
            let (id, len) = self.q[rx].used(&self.mem, self.seen[rx]);
            self.seen[rx] += 1;
            let mut b = vec![0u8; len as usize];
            self.mem.read(BUFS + (rx as u64 * 8 + u64::from(id) % 8) * 64, &mut b).expect("rx");
            b
        }
        fn idle(&self, rx: usize) -> bool {
            std::thread::sleep(Duration::from_millis(50));
            self.q[rx].used_idx(&self.mem) == self.seen[rx]
        }
    }

    #[test]
    fn multiport_bring_up_names_the_modem_and_carries_data_both_ways() {
        let mut g = guest();
        g.host[1].send(b"+CREG: 1\r\n".to_vec()); // before the guest opens the port: held
        g.control(0, ctl::DEVICE_READY, 1);
        assert_eq!(g.recv(CTL_RX), ctl_msg(0, ctl::DEVICE_ADD, 0, &[]));
        assert_eq!(g.recv(CTL_RX), ctl_msg(1, ctl::DEVICE_ADD, 0, &[]));
        g.control(0, ctl::PORT_READY, 1);
        assert_eq!(g.recv(CTL_RX), ctl_msg(0, ctl::CONSOLE_PORT, 1, &[]));
        assert_eq!(g.recv(CTL_RX), ctl_msg(0, ctl::PORT_OPEN, 1, &[]));
        g.control(1, ctl::PORT_READY, 1);
        assert_eq!(g.recv(CTL_RX), ctl_msg(1, ctl::PORT_NAME, 1, b"modem"));
        assert_eq!(g.recv(CTL_RX), ctl_msg(1, ctl::PORT_OPEN, 1, &[]));
        assert!(g.idle(rx_queue(1)), "nothing until the guest opens the port");

        g.control(1, ctl::PORT_OPEN, 1);
        let mut got = Vec::new();
        while got.len() < b"+CREG: 1\r\nREADY\r\n".len() {
            got.extend(g.recv(rx_queue(1)));
        }
        assert_eq!(got, b"+CREG: 1\r\nREADY\r\n", "held bytes, then the greeting");
        g.send(tx_queue(1), b"AT+CSQ\r");
        assert_eq!(g.recv(rx_queue(1)), b"OK\r\n");
        g.send(tx_queue(0), b"console output");
        assert!(g.idle(0), "port 0 is a sink");

        g.control(1, ctl::PORT_OPEN, 0);
        g.host[1].send(b"dropped".to_vec());
        assert!(g.idle(rx_queue(1)), "a closed port gets nothing");
        assert_eq!(*g.log.lock().expect("log"), ["open=true", "AT+CSQ\r", "open=false"]);
        g.control(9, ctl::PORT_READY, 1); // no such port: ignored
        g.send(CTL_TX, &[1, 2, 3]); // runt message: ignored
        assert!(g.idle(CTL_RX));
    }

    #[test]
    fn host_bytes_wait_for_receive_buffers() {
        let mut g = guest();
        g.control(1, ctl::PORT_OPEN, 1);
        let _ = g.recv(rx_queue(1)); // READY
        // 8 buffers of 64 bytes are posted; send more than fits, then add buffers.
        let big = vec![b'x'; 64 * 10];
        let posted_before = g.q[rx_queue(1)].used_idx(&g.mem);
        g.host[1].send(big.clone());
        let mut got = Vec::new();
        for _ in posted_before..8 {
            got.extend(g.recv(rx_queue(1)));
        }
        assert!(g.idle(rx_queue(1)), "out of buffers");
        let old = g.q[rx_queue(1)].avail_idx();
        for b in 0..4u64 {
            let addr = BUFS + (rx_queue(1) as u64 * 8 + b) * 64;
            g.q[rx_queue(1)].push(&g.mem, &[(addr, 64, true)]);
        }
        g.host[1].send(Vec::new()); // empty sends are dropped; the next kick flushes
        assert!(g.kick_if_asked(rx_queue(1), old), "a device out of buffers asks for the refill");
        while got.len() < big.len() {
            got.extend(g.recv(rx_queue(1)));
        }
        assert_eq!(got, big);
    }

    /// A backend whose first write blocks the worker (mid-drain) until the test lets it go.
    struct Gate(Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>);

    impl PortBackend for Gate {
        fn guest_data(&mut self, _data: &[u8]) -> Vec<u8> {
            if let Some((entered, release)) = self.0.take() {
                let _ = entered.send(());
                let _ = release.recv();
            }
            Vec::new()
        }
    }

    /// What the guest writes while the worker is busy costs it at most one kick per pass.
    #[test]
    fn writes_while_the_worker_drains_cost_no_kicks() {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x40_0000).expect("ram"));
        let (mut dev, _host) =
            Console::new(vec![PortSpec { name: String::new(), backend: Box::new(Gate(Some((entered_tx, release_rx)))) }]);
        let mut q: Vec<Driver> = (0..4).map(|i| Driver::at(RAM_BASE + 0x1000 + i * 0x2000, 16)).collect();
        let features = dev.features();
        assert!(features & F_RING_EVENT_IDX != 0, "the device offers event-idx");
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        dev.activate(Activation { mem: mem.clone(), irq, queues: q.iter().map(|d| Some(d.cfg)).collect(), features }).expect("activate");
        // Dropped before the device, so a failing assertion releases the worker.
        let release = release;
        let tx = tx_queue(0);
        let mut kicks = 0;
        let mut write = |q: &mut Driver, i: u64| {
            let at = RAM_BASE + 0x20_0000 + i * 0x100;
            mem.write(at, b"log line").expect("tx");
            let old = q.avail_idx();
            q.push(&mem, &[(at, 8, false)]);
            let kick = q.needs_kick(&mem, old, true);
            if kick {
                dev.notify(tx as u16);
            }
            u32::from(kick)
        };
        kicks += write(&mut q[tx], 0);
        entered.recv_timeout(Duration::from_secs(5)).expect("the worker took the first write");
        for i in 1..12 {
            kicks += write(&mut q[tx], i);
        }
        // The worker hands a port its data after draining the queue, so the gate holds it
        // with kicks back on: the next write kicks once, and the rest none until it catches up.
        assert_eq!(kicks, 2, "12 writes while the worker is busy: 2 kicks (12 with the flags)");
        release.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(5);
        while q[tx].used_idx(&mem) != 12 {
            assert!(Instant::now() < deadline, "the worker never took the writes");
            std::thread::yield_now();
        }
    }

    /// The lost-wakeup window on a transmit queue: the guest writes after the worker's last pop
    /// came back empty but before kicks are back on, so it doesn't kick; the worker's recheck
    /// still takes the write.
    #[test]
    fn a_write_as_kicks_come_back_on_is_still_taken() {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x40_0000).expect("ram"));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ports = vec![Port { name: "modem".into(), backend: Box::new(Modem(log.clone())), guest_open: false, pending: VecDeque::new() }];
        let tx = Arc::new(std::sync::Mutex::new(Driver::at(RAM_BASE + 0x3000, 16)));
        let txcfg = tx.lock().expect("tx").cfg;
        let mut txq = Queue::new(txcfg, true);
        let write = |mem: &GuestRam, d: &mut Driver, at: u64, b: &[u8]| {
            mem.write(at, b).expect("tx");
            let old = d.avail_idx();
            d.push(mem, &[(at, b.len() as u32, false)]);
            d.needs_kick(mem, old, true)
        };
        write(&mem, &mut tx.lock().expect("tx"), RAM_BASE + 0x20_0000, b"AT");
        let (m, t, missed) = (mem.clone(), tx.clone(), Arc::new(std::sync::Mutex::new(Vec::new())));
        let missed2 = missed.clone();
        let mut fired = false;
        txq.on_empty = Some(Box::new(move || {
            if !std::mem::replace(&mut fired, true) {
                let kicked = write(&m, &mut t.lock().expect("tx"), RAM_BASE + 0x20_0100, b"+CSQ");
                missed2.lock().expect("missed").push(!kicked);
            }
        }));
        let queues = vec![None, Some(txq)];
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        let mut w = Worker { mem: mem.clone(), irq, used: vec![false; queues.len()], queues, ports, ctl_out: VecDeque::new() };
        w.service();
        assert_eq!(*missed.lock().expect("missed"), [true], "the guest skipped its kick");
        assert_eq!(tx.lock().expect("tx").used_idx(&mem), 2, "both writes taken");
        assert_eq!(*log.lock().expect("log"), ["AT+CSQ"]);
    }

    /// A `Write` the test can read back.
    #[derive(Clone, Default)]
    struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buf").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unnamed_ports_after_the_first_are_console_ports_too() {
        // The board's layout: console (hvc0), modem, then logcat's console (hvc1).
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x40_0000).expect("ram"));
        let logcat = Shared::default();
        let (mut dev, _host) = Console::new(vec![
            PortSpec { name: String::new(), backend: Box::new(Sink) },
            PortSpec { name: "modem".into(), backend: Box::new(Sink) },
            PortSpec {
                name: String::new(),
                backend: Box::new(Writer(Spool::new("test-logcat", Box::new(logcat.clone()), 64).expect("spool"))),
            },
        ]);
        assert_eq!(dev.num_queues(), 8);
        let mut q: Vec<Driver> = (0..8).map(|i| Driver::at(RAM_BASE + 0x1000 + i * 0x2000, 16)).collect();
        for b in 0..8u64 {
            q[CTL_RX].push(&mem, &[(BUFS + b * 64, 64, true)]);
        }
        let irq = Arc::new(Interrupt::new(Arc::new(TestLine::default())));
        dev.activate(Activation { mem: mem.clone(), irq, queues: q.iter().map(|d| Some(d.cfg)).collect(), features: F_MULTIPORT })
            .expect("activate");
        let mut tx = RAM_BASE + 0x20_0000;
        let mut send = |q: &mut Driver, dev: &mut Console, queue: usize, bytes: &[u8]| {
            mem.write(tx, bytes).expect("tx");
            q.push(&mem, &[(tx, bytes.len() as u32, false)]);
            tx += 0x100;
            dev.notify(queue as u16);
        };
        send(&mut q[CTL_TX], &mut dev, CTL_TX, &ctl_msg(2, ctl::PORT_READY, 1, &[]));
        let deadline = Instant::now() + Duration::from_secs(5);
        while q[CTL_RX].used_idx(&mem) < 2 {
            assert!(Instant::now() < deadline, "no reply to PORT_READY");
            std::thread::sleep(Duration::from_millis(1));
        }
        let read = |i: u16| {
            let (id, len) = q[CTL_RX].used(&mem, i);
            let mut b = vec![0u8; len as usize];
            mem.read(BUFS + u64::from(id) % 8 * 64, &mut b).expect("rx");
            b
        };
        assert_eq!(read(0), ctl_msg(2, ctl::CONSOLE_PORT, 1, &[]), "a console port, not a named one");
        assert_eq!(read(1), ctl_msg(2, ctl::PORT_OPEN, 1, &[]));
        send(&mut q[tx_queue(2)], &mut dev, tx_queue(2), b"11-23 19:35:00.000 I/ActivityManager: Start proc\n");
        while logcat.0.lock().expect("buf").is_empty() {
            assert!(Instant::now() < deadline, "logcat output never arrived");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(*logcat.0.lock().expect("buf"), b"11-23 19:35:00.000 I/ActivityManager: Start proc\n");
    }

    /// A `Write` that blocks until the test opens its gate (a pipe nobody reads).
    struct Gated(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>, Shared);

    impl std::io::Write for Gated {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let (open, cv) = &*self.0;
            let mut g = open.lock().expect("gate");
            while !*g {
                g = cv.wait(g).expect("gate");
            }
            self.1.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_stuck_writer_drops_output_instead_of_blocking_the_port() {
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let out = Shared::default();
        let mut port = Writer(Spool::new("test-stuck", Box::new(Gated(gate.clone(), out.clone())), 4).expect("spool"));
        // The writer thread takes one chunk and blocks on it; four more fill the queue; the
        // rest are dropped and counted. None of it waits.
        let t = Instant::now();
        for _ in 0..100 {
            assert!(port.guest_data(&[b'#'; 10]).is_empty());
        }
        assert!(t.elapsed() < Duration::from_secs(1), "guest_data blocked: {:?}", t.elapsed());
        let dropped = port.0.dropped();
        assert!([950, 960].contains(&dropped), "four queued, maybe one more taken: {dropped}");
        *gate.0.lock().expect("gate") = true;
        gate.1.notify_all();
        let kept = |b: &[u8]| b.iter().filter(|&&c| c == b'#').count() as u64;
        let deadline = Instant::now() + Duration::from_secs(5);
        while kept(&out.0.lock().expect("buf")) < 1000 - dropped {
            assert!(Instant::now() < deadline, "the queued chunks never drained");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Drained: the next chunk goes through.
        port.guest_data(b"after\n");
        while !out.0.lock().expect("buf").ends_with(b"after\n") {
            assert!(Instant::now() < deadline, "the spool never drained");
            std::thread::sleep(Duration::from_millis(1));
        }
        let text = String::from_utf8(out.0.lock().expect("buf").clone()).expect("utf8");
        // Every dropped byte is noted (in one marker, or two when the drops straddled a chunk the
        // writer had already taken).
        let noted: u64 =
            text.split("[emu: ").skip(1).map(|s| s.split(' ').next().and_then(|n| n.parse::<u64>().ok()).expect("count")).sum();
        assert_eq!(noted, dropped, "{text}");
        assert_eq!(kept(text.as_bytes()), 1000 - dropped);
    }

    #[test]
    fn control_messages_and_queue_numbers() {
        assert_eq!(ctl_msg(2, ctl::PORT_NAME, 1, b"modem"), [2, 0, 0, 0, 7, 0, 1, 0, b'm', b'o', b'd', b'e', b'm']);
        assert_eq!((rx_queue(0), tx_queue(0)), (0, 1));
        assert_eq!((rx_queue(1), tx_queue(1)), (4, 5));
        assert_eq!((rx_queue(2), tx_queue(2)), (6, 7));
        let (c, handles) = Console::new(vec![
            PortSpec { name: String::new(), backend: Box::new(Sink) },
            PortSpec { name: "modem".into(), backend: Box::new(Sink) },
        ]);
        assert_eq!(c.num_queues(), 6);
        assert_eq!(handles.len(), 2);
        let mut cfg = [0u8; 4];
        c.read_config(4, &mut cfg);
        assert_eq!(u32::from_le_bytes(cfg), 2, "max_nr_ports");
    }
}
