//! virtio-console with multiport: named byte-stream ports between guest services and the host.
//!
//! The guest's `qemu-props` walks `/sys/class/virtio-ports`, reads each port's name and sets
//! `vendor.qemu.vport.<name>=/dev/vportNpM`; HALs open their port by that property (the radio
//! HAL's `modem`, the GNSS HAL's `gnss`, ...). Port 0 is a plain console port.
//!
//! Queues: port 0 rx/tx are 0/1, control rx/tx are 2/3, port n ≥ 1 uses 2n+2 / 2n+3. Control
//! messages are `{ id: u32, event: u16, value: u16 }` plus a payload for PORT_NAME. Handshake:
//! guest DEVICE_READY → host DEVICE_ADD per port → guest PORT_READY → host PORT_NAME (named
//! ports) or CONSOLE_PORT (port 0), then PORT_OPEN(1) since the host end is always connected.
//! The guest reports its own opens with PORT_OPEN; data for a port the guest hasn't opened is
//! held until it does (the driver would discard it).
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use super::queue::Queue;
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

pub struct PortSpec {
    /// Empty for port 0 (the console port).
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
    fn features(&self) -> u64 {
        F_MULTIPORT
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
        let queues = act.queues.iter().map(|q| q.map(|c| Queue::new(c, false))).collect();
        let mut w = Worker { mem: act.mem, irq: act.irq, queues, ports, ctl_out: VecDeque::new() };
        let h = std::thread::Builder::new()
            .name("virtio-console".into())
            .spawn(move || {
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
    ports: Vec<Port>,
    /// Control messages waiting for control receive buffers.
    ctl_out: VecDeque<Vec<u8>>,
}

impl Worker {
    /// Drains every transmit queue (guest → host), then delivers what's pending.
    fn service(&mut self) {
        let mut used = false;
        // Control messages from the guest.
        let mut incoming = Vec::new();
        if let Some(q) = self.queues.get_mut(CTL_TX).and_then(Option::as_mut) {
            while let Ok(Some(c)) = q.pop(&self.mem) {
                if let Ok(b) = c.read_all(&self.mem, 4096) {
                    incoming.push(b);
                }
                let _ = q.add_used(&self.mem, c.head, 0);
                used = true;
            }
        }
        for m in incoming {
            self.control(&m);
        }
        // Port data from the guest.
        for port in 0..self.ports.len() as u32 {
            let mut data = Vec::new();
            if let Some(q) = self.queues.get_mut(tx_queue(port)).and_then(Option::as_mut) {
                while let Ok(Some(c)) = q.pop(&self.mem) {
                    if let Ok(b) = c.read_all(&self.mem, 1 << 16) {
                        data.extend_from_slice(&b);
                    }
                    let _ = q.add_used(&self.mem, c.head, 0);
                    used = true;
                }
            }
            if !data.is_empty() {
                let p = &mut self.ports[port as usize];
                let reply = p.backend.guest_data(&data);
                p.pending.extend(reply);
            }
        }
        if used {
            self.irq.raise(INT_USED_RING);
        }
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
                if id == 0 {
                    self.ctl_out.push_back(ctl_msg(0, ctl::CONSOLE_PORT, 1, &[]));
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
        let mut used = false;
        if let Some(q) = self.queues.get_mut(CTL_RX).and_then(Option::as_mut) {
            while let Some(m) = self.ctl_out.front() {
                let Ok(Some(c)) = q.pop(&self.mem) else { break };
                let n = c.write_all(&self.mem, m).unwrap_or(0);
                let _ = q.add_used(&self.mem, c.head, n);
                self.ctl_out.pop_front();
                used = true;
            }
        }
        for port in 0..self.ports.len() as u32 {
            let p = &mut self.ports[port as usize];
            if !p.guest_open || p.pending.is_empty() {
                continue;
            }
            let Some(q) = self.queues.get_mut(rx_queue(port)).and_then(Option::as_mut) else { continue };
            while !p.pending.is_empty() {
                let Ok(Some(c)) = q.pop(&self.mem) else { break };
                let room: usize = c.writable().map(|d| d.len as usize).sum();
                let take = room.min(p.pending.len());
                let chunk: Vec<u8> = p.pending.drain(..take).collect();
                let n = c.write_all(&self.mem, &chunk).unwrap_or(0);
                let _ = q.add_used(&self.mem, c.head, n);
                used = true;
            }
        }
        if used {
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
        dev.activate(Activation { mem: mem.clone(), irq, queues: q.iter().map(|d| Some(d.cfg)).collect(), features: F_MULTIPORT })
            .expect("activate");
        Guest { mem, dev, q, seen: vec![0; 6], tx_next: RAM_BASE + 0x20_0000, host, log }
    }

    impl Guest {
        fn send(&mut self, queue: usize, bytes: &[u8]) {
            self.mem.write(self.tx_next, bytes).expect("tx");
            self.q[queue].push(&self.mem, &[(self.tx_next, bytes.len() as u32, false)]);
            self.tx_next += 0x100;
            self.dev.notify(queue as u16);
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
        for b in 0..4u64 {
            let addr = BUFS + (rx_queue(1) as u64 * 8 + b) * 64;
            g.q[rx_queue(1)].push(&g.mem, &[(addr, 64, true)]);
        }
        g.host[1].send(Vec::new()); // empty sends are dropped; the next kick flushes
        g.dev.notify(0);
        while got.len() < big.len() {
            got.extend(g.recv(rx_queue(1)));
        }
        assert_eq!(got, big);
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
