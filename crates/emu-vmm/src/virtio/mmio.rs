//! virtio-mmio transport, version 2 (virtio 1.x, section 4.2).
use std::sync::{Arc, Mutex};

use emu_hvf::GuestRam;

use super::queue::QueueConfig;
use super::{Interrupt, VirtioDevice};
use crate::bus::{MmioDevice, le_value, set_le};

const MAGIC: u32 = 0x7472_6976; // "virt"
const VENDOR: u32 = 0x554d_4551; // "QEMU", what guests expect from emulated boards

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_FAILED: u32 = 0x80;

/// QueueNotify: the driver writes a queue index here when it adds buffers.
pub const QUEUE_NOTIFY: u64 = 0x050;

pub const F_VERSION_1: u64 = 1 << 32;
pub const F_RING_INDIRECT_DESC: u64 = 1 << 28;
pub const F_RING_EVENT_IDX: u64 = 1 << 29;

#[derive(Clone, Copy, Default)]
struct QueueRegs {
    num: u32,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
}

struct State {
    dev: Option<Box<dyn VirtioDevice>>,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    shm_sel: u32,
    queues: Vec<QueueRegs>,
    status: u32,
    config_generation: u32,
    activated: bool,
}

/// One virtio-mmio slot. An empty slot (no device) answers with device ID 0, which guests
/// skip — the stock board exposes all 32 slots this way.
pub struct MmioTransport {
    state: Mutex<State>,
    irq: Arc<Interrupt>,
    mem: Arc<GuestRam>,
}

impl MmioTransport {
    pub fn new(dev: Option<Box<dyn VirtioDevice>>, irq: Arc<Interrupt>, mem: Arc<GuestRam>) -> MmioTransport {
        let nq = dev.as_ref().map_or(0, |d| d.num_queues());
        MmioTransport {
            state: Mutex::new(State {
                dev,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                queue_sel: 0,
                shm_sel: 0,
                queues: vec![QueueRegs::default(); nq],
                status: 0,
                config_generation: 0,
                activated: false,
            }),
            irq,
            mem,
        }
    }

    /// How many queues the device has (0 for an empty slot): the QueueNotify values that mean
    /// something.
    pub fn queue_count(&self) -> usize {
        self.lock().queues.len()
    }

    /// The driver's QueueNotify of `queue`, arriving some other way than an MMIO store (an
    /// ioeventfd the hypervisor completed in the kernel): the same as the store.
    pub fn queue_notify(&self, queue: u32) {
        Self::notify_locked(&mut self.lock(), queue);
    }

    fn notify_locked(s: &mut State, queue: u32) {
        if s.activated
            && let Some(d) = s.dev.as_mut()
        {
            d.notify(queue as u16);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn reset(s: &mut State, irq: &Interrupt) {
        if let Some(d) = s.dev.as_mut() {
            d.reset();
        }
        s.status = 0;
        s.driver_features = 0;
        s.activated = false;
        for q in &mut s.queues {
            *q = QueueRegs::default();
        }
        irq.ack(u32::MAX);
    }

    fn try_activate(&self, s: &mut State) {
        if s.activated || s.status & STATUS_DRIVER_OK == 0 {
            return;
        }
        let feats = s.driver_features;
        let configs: Vec<Option<QueueConfig>> = s
            .queues
            .iter()
            .map(|q| q.ready.then_some(QueueConfig { size: q.num as u16, desc: q.desc, avail: q.driver, used: q.device }))
            .collect();
        if let Some(d) = s.dev.as_mut() {
            match d.activate(super::Activation { mem: self.mem.clone(), irq: self.irq.clone(), queues: configs, features: feats }) {
                Ok(()) => s.activated = true,
                Err(e) => {
                    eprintln!("emu: virtio device {} failed to activate: {e}", d.device_id());
                    s.status |= STATUS_FAILED;
                }
            }
        }
    }
}

impl MmioDevice for MmioTransport {
    fn read(&self, offset: u64, data: &mut [u8]) {
        // The interrupt handler's status read needs nothing of the transport's state.
        if offset == 0x060 {
            set_le(data, u64::from(self.irq.status()));
            return;
        }
        let s = self.lock();
        if offset >= 0x100 {
            match s.dev.as_ref() {
                Some(d) => d.read_config(offset - 0x100, data),
                None => data.fill(0),
            }
            return;
        }
        let q = s.queues.get(s.queue_sel as usize).copied();
        let v: u32 = match offset {
            0x000 => MAGIC,
            0x004 => 2,
            0x008 => s.dev.as_ref().map_or(0, |d| d.device_id()),
            0x00c => VENDOR,
            0x010 => {
                let f = s.dev.as_ref().map_or(0, |d| d.features() | F_VERSION_1);
                match s.device_features_sel {
                    0 => f as u32,
                    1 => (f >> 32) as u32,
                    _ => 0,
                }
            }
            0x034 => {
                if q.is_some() {
                    u32::from(s.dev.as_ref().map_or(0, |d| d.queue_max_size()))
                } else {
                    0
                }
            }
            0x044 => u32::from(q.is_some_and(|q| q.ready)),
            0x0b0..=0x0bc => {
                // Shared memory region: length then base, low/high words; absent = all ones.
                match s.dev.as_ref().and_then(|d| d.shm_region(s.shm_sel as u8)) {
                    Some((base, len)) => {
                        let v = if offset < 0x0b8 { len } else { base };
                        if offset & 4 == 0 { v as u32 } else { (v >> 32) as u32 }
                    }
                    None => u32::MAX,
                }
            }
            0x070 => s.status,
            0x0fc => s.config_generation,
            _ => 0,
        };
        set_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        // Nor does its acknowledge.
        if offset == 0x064 {
            self.irq.ack(le_value(data) as u32);
            return;
        }
        let mut s = self.lock();
        if offset >= 0x100 {
            if let Some(d) = s.dev.as_mut() {
                d.write_config(offset - 0x100, data);
            }
            return;
        }
        let v = le_value(data) as u32;
        let sel = s.queue_sel as usize;
        let max = s.dev.as_ref().map_or(0, |d| d.queue_max_size());
        match offset {
            0x014 => s.device_features_sel = v,
            0x020 => {
                let offered = s.dev.as_ref().map_or(0, |d| d.features() | F_VERSION_1);
                match s.driver_features_sel {
                    0 => s.driver_features = (s.driver_features & !0xffff_ffff) | u64::from(v),
                    1 => s.driver_features = (s.driver_features & 0xffff_ffff) | (u64::from(v) << 32),
                    _ => {}
                }
                s.driver_features &= offered;
            }
            0x024 => s.driver_features_sel = v,
            0x030 => s.queue_sel = v,
            0x0ac => s.shm_sel = v,
            0x038 => {
                if let Some(q) = s.queues.get_mut(sel) {
                    q.num = if v.is_power_of_two() && v <= u32::from(max) { v } else { 0 };
                }
            }
            0x044 => {
                if let Some(q) = s.queues.get_mut(sel) {
                    q.ready = v == 1 && q.num != 0;
                }
            }
            QUEUE_NOTIFY => Self::notify_locked(&mut s, v),
            0x070 => {
                if v == 0 {
                    let irq = self.irq.clone();
                    Self::reset(&mut s, &irq);
                } else {
                    // Drivers must not clear bits; FEATURES_OK sticks only if we accept them.
                    let mut new = v;
                    if new & STATUS_FEATURES_OK != 0 && s.driver_features & F_VERSION_1 == 0 {
                        new &= !STATUS_FEATURES_OK;
                    }
                    s.status = new | (s.status & (STATUS_ACKNOWLEDGE | STATUS_DRIVER));
                    self.try_activate(&mut s);
                }
            }
            0x080 | 0x084 | 0x090 | 0x094 | 0x0a0 | 0x0a4 => {
                if let Some(q) = s.queues.get_mut(sel) {
                    let field = match offset {
                        0x080 | 0x084 => &mut q.desc,
                        0x090 | 0x094 => &mut q.driver,
                        _ => &mut q.device,
                    };
                    if offset & 4 == 0 {
                        *field = (*field & !0xffff_ffff) | u64::from(v);
                    } else {
                        *field = (*field & 0xffff_ffff) | (u64::from(v) << 32);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::irq::TestLine;

    /// What the transport told the device, for assertions.
    #[derive(Default)]
    struct Seen {
        activations: Vec<(u64, Vec<Option<QueueConfig>>)>,
        notified: Vec<u16>,
        config_writes: Vec<(u64, Vec<u8>)>,
        resets: u32,
    }

    struct FakeDevice {
        seen: Arc<Mutex<Seen>>,
        fail: bool,
    }

    const FEATURE_A: u64 = 1 << 3;

    impl VirtioDevice for FakeDevice {
        fn device_id(&self) -> u32 {
            42
        }
        fn features(&self) -> u64 {
            FEATURE_A | F_RING_EVENT_IDX
        }
        fn num_queues(&self) -> usize {
            2
        }
        fn queue_max_size(&self) -> u16 {
            128
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            for (i, b) in data.iter_mut().enumerate() {
                *b = (offset as u8).wrapping_add(i as u8);
            }
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            self.seen.lock().expect("seen").config_writes.push((offset, data.to_vec()));
        }
        fn activate(&mut self, act: super::super::Activation) -> Result<(), String> {
            if self.fail {
                return Err("no".into());
            }
            self.seen.lock().expect("seen").activations.push((act.features, act.queues));
            Ok(())
        }
        fn notify(&mut self, queue: u16) {
            self.seen.lock().expect("seen").notified.push(queue);
        }
        fn reset(&mut self) {
            self.seen.lock().expect("seen").resets += 1;
        }
        fn shm_region(&self, id: u8) -> Option<(u64, u64)> {
            (id == 1).then_some((0x1_2345_6000, 0x8000_0000))
        }
    }

    struct Slot {
        t: MmioTransport,
        seen: Arc<Mutex<Seen>>,
        line: Arc<TestLine>,
    }

    fn slot(fail: bool) -> Slot {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let line = Arc::new(TestLine::default());
        let mem = Arc::new(GuestRam::new(0x4000_0000, 0x10000).expect("ram"));
        let dev: Box<dyn VirtioDevice> = Box::new(FakeDevice { seen: seen.clone(), fail });
        Slot { t: MmioTransport::new(Some(dev), Arc::new(Interrupt::new(line.clone())), mem), seen, line }
    }

    impl Slot {
        fn r(&self, offset: u64) -> u32 {
            let mut b = [0u8; 4];
            self.t.read(offset, &mut b);
            u32::from_le_bytes(b)
        }
        fn w(&self, offset: u64, v: u32) {
            self.t.write(offset, &v.to_le_bytes());
        }
        /// The Linux driver's bring-up up to FEATURES_OK, accepting `features`.
        fn negotiate(&self, features: u64) {
            self.w(0x070, 0);
            self.w(0x070, STATUS_ACKNOWLEDGE);
            self.w(0x070, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
            for sel in 0..2 {
                self.w(0x024, sel);
                self.w(0x020, (features >> (32 * sel)) as u32);
            }
            self.w(0x070, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        }
        fn queue(&self, sel: u32, num: u32, base: u64) {
            self.w(0x030, sel);
            self.w(0x038, num);
            for (lo, addr) in [(0x080, base), (0x090, base + 0x1000), (0x0a0, base + 0x2000)] {
                self.w(lo, addr as u32);
                self.w(lo + 4, (addr >> 32) as u32);
            }
            self.w(0x044, 1);
        }
    }

    #[test]
    fn identifies_like_a_qemu_virtio_mmio_v2_slot() {
        let s = slot(false);
        assert_eq!([s.r(0x000), s.r(0x004), s.r(0x008), s.r(0x00c)], [MAGIC, 2, 42, VENDOR]);
        s.w(0x014, 0);
        assert_eq!(s.r(0x010), (FEATURE_A | F_RING_EVENT_IDX) as u32);
        s.w(0x014, 1);
        assert_eq!(s.r(0x010), 1, "VERSION_1 in the high word");
        s.w(0x014, 2);
        assert_eq!(s.r(0x010), 0);
        s.w(0x030, 1);
        assert_eq!(s.r(0x034), 128, "queue 1 exists");
        s.w(0x030, 2);
        assert_eq!(s.r(0x034), 0, "queue 2 doesn't");
    }

    #[test]
    fn empty_slots_answer_device_id_zero() {
        let mem = Arc::new(GuestRam::new(0x4000_0000, 0x4000).expect("ram"));
        let t = MmioTransport::new(None, Arc::new(Interrupt::new(Arc::new(TestLine::default()))), mem);
        let mut b = [0xffu8; 4];
        t.read(0x008, &mut b);
        assert_eq!(b, [0; 4]);
        t.read(0x100, &mut b);
        assert_eq!(b, [0; 4], "no config space");
        t.write(0x070, &4u32.to_le_bytes()); // DRIVER_OK on nothing: ignored
    }

    #[test]
    fn bring_up_activates_with_the_negotiated_features_and_ready_queues() {
        let s = slot(false);
        // Features the device never offered are dropped.
        s.negotiate(FEATURE_A | (1 << 7) | F_VERSION_1);
        assert_ne!(s.r(0x070) & STATUS_FEATURES_OK, 0, "accepted");
        s.queue(0, 64, 0x4000_0000);
        s.w(0x030, 1);
        s.w(0x038, 100); // not a power of two
        s.w(0x044, 1);
        assert_eq!(s.r(0x044), 0, "a queue without a valid size never becomes ready");
        s.w(0x050, 0);
        assert!(s.seen.lock().expect("seen").notified.is_empty(), "no notifications before DRIVER_OK");
        s.w(0x070, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
        {
            let seen = s.seen.lock().expect("seen");
            let [(features, queues)] = seen.activations.as_slice() else { panic!("one activation: {}", seen.activations.len()) };
            assert_eq!(*features, FEATURE_A | F_VERSION_1);
            assert_eq!(queues.len(), 2);
            let q = queues[0].as_ref().expect("queue 0");
            assert_eq!((q.size, q.desc, q.avail, q.used), (64, 0x4000_0000, 0x4000_1000, 0x4000_2000));
            assert!(queues[1].is_none());
        }
        s.w(0x070, 0xf);
        assert_eq!(s.seen.lock().expect("seen").activations.len(), 1, "activates once");
        s.w(0x050, 1);
        assert_eq!(s.seen.lock().expect("seen").notified, [1]);
    }

    /// An ioeventfd delivers QueueNotify off the MMIO path: it must mean exactly what the store
    /// does (ignored before DRIVER_OK and after a reset).
    #[test]
    fn a_notify_from_an_ioeventfd_is_the_same_as_the_store() {
        let s = slot(false);
        assert_eq!(s.t.queue_count(), 2);
        s.t.queue_notify(0);
        assert!(s.seen.lock().expect("seen").notified.is_empty(), "nothing before DRIVER_OK");
        s.negotiate(F_VERSION_1);
        s.queue(0, 16, 0x4000_4000);
        s.w(0x070, 0xf);
        s.t.queue_notify(1);
        s.w(QUEUE_NOTIFY, 0);
        assert_eq!(s.seen.lock().expect("seen").notified, [1, 0], "both paths, in order");
        s.w(0x070, 0);
        s.t.queue_notify(0);
        assert_eq!(s.seen.lock().expect("seen").notified, [1, 0], "nothing after a reset");
        let mem = Arc::new(GuestRam::new(0x4000_0000, 0x4000).expect("ram"));
        let empty = MmioTransport::new(None, Arc::new(Interrupt::new(Arc::new(TestLine::default()))), mem);
        assert_eq!(empty.queue_count(), 0, "an empty slot gets no notifiers");
        empty.queue_notify(0);
    }

    #[test]
    fn features_ok_needs_version_1() {
        let s = slot(false);
        s.negotiate(FEATURE_A);
        assert_eq!(s.r(0x070) & STATUS_FEATURES_OK, 0, "a legacy driver is refused");
    }

    #[test]
    fn a_failed_activation_sets_failed() {
        let s = slot(true);
        s.negotiate(F_VERSION_1);
        s.w(0x070, 0xf);
        assert_ne!(s.r(0x070) & STATUS_FAILED, 0);
    }

    #[test]
    fn interrupts_are_level_until_acknowledged_and_reset_clears_everything() {
        let s = slot(false);
        s.negotiate(F_VERSION_1);
        s.queue(0, 16, 0x4000_4000);
        s.w(0x070, 0xf);
        s.t.irq.raise(super::super::INT_USED_RING | super::super::INT_CONFIG);
        assert!(s.line.level());
        assert_eq!(s.r(0x060), 3);
        s.w(0x064, super::super::INT_USED_RING);
        assert!(s.line.level(), "still asserted while a bit is set");
        s.w(0x064, super::super::INT_CONFIG);
        assert!(!s.line.level());

        s.t.irq.raise(super::super::INT_USED_RING);
        s.w(0x070, 0);
        assert!(!s.line.level(), "reset drops the line");
        assert_eq!([s.r(0x070), s.r(0x044)], [0, 0]);
        assert_eq!(s.seen.lock().expect("seen").resets, 2, "negotiate's reset and this one");
        s.w(0x050, 0);
        assert!(s.seen.lock().expect("seen").notified.is_empty(), "no notifications after reset");
    }

    /// A device thread raising and the driver acknowledging (as a worker and a vCPU do) while
    /// another vCPU reads the interrupt status: what each costs when they meet on the transport.
    /// `cargo test --release -p emu-vmm isr_under_interrupt_traffic -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn isr_under_interrupt_traffic() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Instant;
        const READS: u32 = 2_000_000;
        let s = slot(false);
        s.negotiate(F_VERSION_1);
        s.queue(0, 16, 0x4000_4000);
        s.w(0x070, 0xf);
        let done = AtomicBool::new(false);
        let (reads_ns, cycles) = std::thread::scope(|sc| {
            let (s, done) = (&s, &done);
            let traffic = sc.spawn(move || {
                let mut n = 0u64;
                while !done.load(Ordering::Relaxed) {
                    s.t.irq.raise(super::super::INT_USED_RING);
                    s.w(0x064, super::super::INT_USED_RING);
                    n += 1;
                }
                n
            });
            let t = Instant::now();
            for _ in 0..READS {
                std::hint::black_box(s.r(0x060));
            }
            let el = t.elapsed();
            done.store(true, Ordering::Relaxed);
            (el.as_nanos() as f64 / f64::from(READS), traffic.join().expect("thread") as f64 / el.as_secs_f64())
        });
        println!("isr: {reads_ns:.1} ns per ISR read, {:.2} M raise+ack per s alongside", cycles / 1e6);
    }

    #[test]
    fn config_space_and_shared_memory_reach_the_device() {
        let s = slot(false);
        let mut b = [0u8; 2];
        s.t.read(0x104, &mut b);
        assert_eq!(b, [4, 5]);
        s.t.write(0x108, &[9, 8, 7]);
        assert_eq!(s.seen.lock().expect("seen").config_writes, [(8, vec![9, 8, 7])]);
        s.w(0x0ac, 1);
        assert_eq!([s.r(0x0b0), s.r(0x0b4), s.r(0x0b8), s.r(0x0bc)], [0x8000_0000, 0, 0x2345_6000, 1]);
        s.w(0x0ac, 0);
        assert_eq!(s.r(0x0b0), u32::MAX, "no region 0");
    }
}
