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
            0x060 => self.irq.status(),
            0x070 => s.status,
            0x0fc => s.config_generation,
            _ => 0,
        };
        set_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
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
            0x050 => {
                if s.activated
                    && let Some(d) = s.dev.as_mut()
                {
                    d.notify(v as u16);
                }
            }
            0x064 => self.irq.ack(v),
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
