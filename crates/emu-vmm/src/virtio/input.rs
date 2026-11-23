//! virtio-input (virtio 1.2 §5.8): evdev-style events from the host into the guest.
//!
//! Three device kinds: a multi-touch screen (INPUT_PROP_DIRECT, so Android treats it as a
//! touchscreen), a keyboard (hardware keys incl. power/volume/back/home) and a rotary encoder
//! (REL_WHEEL, used by Wear OS). Host code injects through an [`InputHandle`].
//!
//! The keyboard looks like stock's (hw/input/virtio-input-hid.c): named `qwerty2` with ids
//! 0627:0001, so the guest loads goldfish's `qwerty2.kl` (102 is HOME, 232 DPAD_CENTER, 580
//! APP_SWITCH) instead of `Generic.kl`, and advertising stock's key set, since the kernel drops
//! codes a device doesn't advertise.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;
use sim_core::goldfish::keyboard_has;

use super::queue::Queue;
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};

pub const ID_INPUT: u32 = 18;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_LED: u16 = 0x11;
pub const EV_REP: u16 = 0x14;
pub const SYN_REPORT: u16 = 0;
pub const BTN_TOUCH: u16 = 0x14a;
pub const REL_WHEEL: u16 = 0x08;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const INPUT_PROP_DIRECT: u16 = 0x01;
pub const MAX_SLOTS: u32 = 10;

const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

/// The keyboard's name: goldfish's vendor image ships `qwerty2.idc` and `qwerty2.kl` for it.
pub const KEYBOARD_NAME: &str = "qwerty2";
/// Bus, vendor, product, version of stock's keyboard (`BUS_VIRTUAL`, 0627:0001).
const KEYBOARD_IDS: [u16; 4] = [0x06, 0x0627, 0x0001, 0x0001];
/// Ids of the other devices.
const OTHER_IDS: [u16; 4] = [0x06, 0x18d1, 0x0001, 0x0001];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub typ: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub const fn new(typ: u16, code: u16, value: i32) -> InputEvent {
        InputEvent { typ, code, value }
    }

    pub const fn syn() -> InputEvent {
        InputEvent::new(EV_SYN, SYN_REPORT, 0)
    }

    fn bytes(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&self.typ.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Touch { width: u32, height: u32 },
    Keyboard,
    Rotary,
}

enum Msg {
    Events(Vec<InputEvent>),
    Kick,
    Stop,
}

/// Sends events to one virtio-input device; cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct InputHandle {
    tx: Sender<Msg>,
    pub kind: Kind,
}

impl InputHandle {
    /// Queues a batch (callers end batches with [`InputEvent::syn`]).
    pub fn send(&self, events: Vec<InputEvent>) {
        let _ = self.tx.send(Msg::Events(events));
    }

    /// Single-finger touch at device pixels; `down=false` lifts the finger.
    pub fn touch(&self, x: i32, y: i32, down: bool) {
        let mut ev = vec![InputEvent::new(EV_ABS, ABS_MT_SLOT, 0)];
        if down {
            ev.extend([
                InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, 1),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, x),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, y),
                InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0x81),
                InputEvent::new(EV_KEY, BTN_TOUCH, 1),
                InputEvent::new(EV_ABS, ABS_X, x),
                InputEvent::new(EV_ABS, ABS_Y, y),
            ]);
        } else {
            ev.extend([InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1), InputEvent::new(EV_KEY, BTN_TOUCH, 0)]);
        }
        ev.push(InputEvent::syn());
        self.send(ev);
    }

    /// Press and release a key (Linux keycode).
    pub fn tap_key(&self, code: u16) {
        self.send(vec![InputEvent::new(EV_KEY, code, 1), InputEvent::syn(), InputEvent::new(EV_KEY, code, 0), InputEvent::syn()]);
    }
}

pub struct Input {
    kind: Kind,
    name: String,
    select: (u8, u8),
    tx: Sender<Msg>,
    rx: Option<Receiver<Msg>>,
    worker: Option<JoinHandle<Receiver<Msg>>>,
}

impl Input {
    pub fn new(kind: Kind, name: &str) -> (Input, InputHandle) {
        let (tx, rx) = channel();
        let handle = InputHandle { tx: tx.clone(), kind };
        (Input { kind, name: name.to_string(), select: (0, 0), tx, rx: Some(rx), worker: None }, handle)
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.tx.send(Msg::Stop);
            if let Ok(rx) = h.join() {
                self.rx = Some(rx);
            }
        }
    }

    fn ev_bits(&self, typ: u8) -> Vec<u8> {
        match (self.kind, u16::from(typ)) {
            // Like stock: an (empty) EV_REP entry turns on the kernel's key repeat, and the
            // keyboard has num, caps and scroll lock LEDs.
            (Kind::Keyboard, EV_REP) => return vec![0],
            (Kind::Keyboard, EV_LED) => return vec![0b111],
            _ => {}
        }
        let mut bits = vec![0u8; 128];
        let mut set = |code: u16| {
            if let Some(b) = bits.get_mut(code as usize / 8) {
                *b |= 1 << (code % 8);
            }
        };
        match (self.kind, u16::from(typ)) {
            (Kind::Touch { .. }, EV_KEY) => set(BTN_TOUCH),
            (Kind::Touch { .. }, EV_ABS) => {
                for c in [ABS_X, ABS_Y, ABS_MT_SLOT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID, ABS_MT_PRESSURE] {
                    set(c);
                }
            }
            (Kind::Keyboard, EV_KEY) => {
                for c in (0..1024u16).filter(|&c| keyboard_has(c)) {
                    set(c);
                }
            }
            (Kind::Rotary, EV_REL) => set(REL_WHEEL),
            (_, EV_SYN) => set(SYN_REPORT),
            _ => return Vec::new(),
        }
        let len = bits.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        bits.truncate(len);
        bits
    }

    fn abs_info(&self, axis: u8) -> Vec<u8> {
        let Kind::Touch { width, height } = self.kind else { return Vec::new() };
        let (min, max) = match u16::from(axis) {
            ABS_X | ABS_MT_POSITION_X => (0, width.saturating_sub(1)),
            ABS_Y | ABS_MT_POSITION_Y => (0, height.saturating_sub(1)),
            ABS_MT_SLOT => (0, MAX_SLOTS - 1),
            ABS_MT_TRACKING_ID => (0, 0xffff),
            ABS_MT_PRESSURE => (0, 0xff),
            _ => return Vec::new(),
        };
        [min, max, 0, 0, 0].iter().flat_map(|v: &u32| v.to_le_bytes()).collect()
    }

    fn config_payload(&self) -> Vec<u8> {
        let (sel, sub) = self.select;
        match sel {
            CFG_ID_NAME => self.name.as_bytes().to_vec(),
            CFG_ID_SERIAL => b"emu-rs".to_vec(),
            CFG_ID_DEVIDS => {
                let ids = if self.kind == Kind::Keyboard { KEYBOARD_IDS } else { OTHER_IDS };
                ids.iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            CFG_PROP_BITS => match self.kind {
                Kind::Touch { .. } => vec![1 << INPUT_PROP_DIRECT],
                _ => Vec::new(),
            },
            CFG_EV_BITS => self.ev_bits(sub),
            CFG_ABS_INFO => self.abs_info(sub),
            _ => Vec::new(),
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Input {
    fn device_id(&self) -> u32 {
        ID_INPUT
    }
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        2
    }
    fn queue_max_size(&self) -> u16 {
        64
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let payload = self.config_payload();
        let mut cfg = [0u8; 136];
        cfg[0] = self.select.0;
        cfg[1] = self.select.1;
        cfg[2] = payload.len().min(128) as u8;
        cfg[8..8 + payload.len().min(128)].copy_from_slice(&payload[..payload.len().min(128)]);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match offset as usize + i {
                0 => self.select.0 = b,
                1 => self.select.1 = b,
                _ => {}
            }
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let eventq = act.queues.first().copied().flatten().ok_or("input event queue not enabled")?;
        let statusq = act.queues.get(1).copied().flatten();
        self.stop_worker();
        let rx = self.rx.take().ok_or("input worker receiver missing")?;
        let mut w = Worker {
            mem: act.mem,
            irq: act.irq,
            eventq: Queue::new(eventq, false),
            statusq: statusq.map(|q| Queue::new(q, false)),
            pending: VecDeque::new(),
        };
        let h = std::thread::Builder::new()
            .name(format!("input-{}", self.name))
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Stop => break,
                        Msg::Events(ev) => w.pending.extend(ev),
                        Msg::Kick => w.drain_status(),
                    }
                    w.flush();
                }
                rx
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

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    eventq: Queue,
    statusq: Option<Queue>,
    /// Events waiting for guest buffers (bounded: oldest dropped beyond 4096).
    pending: VecDeque<InputEvent>,
}

impl Worker {
    fn drain_status(&mut self) {
        let Some(q) = self.statusq.as_mut() else { return };
        let mut used = false;
        while let Ok(Some(c)) = q.pop(&self.mem) {
            let _ = q.add_used(&self.mem, c.head, 0);
            used = true;
        }
        if used {
            self.irq.raise(INT_USED_RING);
        }
    }

    fn flush(&mut self) {
        while self.pending.len() > 4096 {
            self.pending.pop_front();
        }
        let mut used = false;
        while let Some(ev) = self.pending.front().copied() {
            let chain = match self.eventq.pop(&self.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let n = chain.write_all(&self.mem, &ev.bytes()).unwrap_or(0);
            let _ = self.eventq.add_used(&self.mem, chain.head, n);
            self.pending.pop_front();
            used = true;
        }
        if used {
            self.irq.raise(INT_USED_RING);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::TestLine;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    fn cfg(dev: &mut Input, sel: u8, sub: u8) -> Vec<u8> {
        dev.write_config(0, &[sel, sub]);
        let mut b = [0u8; 136];
        dev.read_config(0, &mut b);
        b[8..8 + b[2] as usize].to_vec()
    }

    #[test]
    fn touch_config_describes_a_direct_multitouch_screen() {
        let (mut dev, _h) = Input::new(Kind::Touch { width: 1080, height: 2400 }, "virtio_input_multi_touch_1");
        assert_eq!(cfg(&mut dev, CFG_ID_NAME, 0), b"virtio_input_multi_touch_1");
        assert_eq!(cfg(&mut dev, CFG_PROP_BITS, 0), vec![1 << INPUT_PROP_DIRECT]);
        let abs = cfg(&mut dev, CFG_EV_BITS, EV_ABS as u8);
        assert!(abs[(ABS_MT_POSITION_X / 8) as usize] & (1 << (ABS_MT_POSITION_X % 8)) != 0);
        let info = cfg(&mut dev, CFG_ABS_INFO, ABS_MT_POSITION_Y as u8);
        assert_eq!(u32::from_le_bytes([info[4], info[5], info[6], info[7]]), 2399);
        assert!(cfg(&mut dev, CFG_EV_BITS, EV_REL as u8).is_empty());
    }

    #[test]
    fn events_flow_into_guest_buffers() {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let mut drv = Driver::new(8);
        for i in 0..4 {
            drv.push(&mem, &[(RAM_BASE + 0x20000 + i * 8, 8, true)]);
        }
        let mut w = Worker {
            mem: mem.clone(),
            irq: Arc::new(Interrupt::new(line.clone())),
            eventq: Queue::new(drv.cfg, false),
            statusq: None,
            pending: VecDeque::new(),
        };
        w.pending.extend([InputEvent::new(EV_KEY, 116, 1), InputEvent::syn()]);
        w.flush();
        assert!(line.level());
        assert_eq!(drv.used_idx(&mem), 2);
        assert_eq!(mem.read_u16(RAM_BASE + 0x20000 + 2).expect("code"), 116);
    }

    fn has(bits: &[u8], code: u16) -> bool {
        bits.get(code as usize / 8).is_some_and(|b| b & (1 << (code % 8)) != 0)
    }

    #[test]
    fn keyboard_looks_like_stocks_qwerty2() {
        let (mut dev, _h) = Input::new(Kind::Keyboard, KEYBOARD_NAME);
        assert_eq!(cfg(&mut dev, CFG_ID_NAME, 0), b"qwerty2");
        assert_eq!(cfg(&mut dev, CFG_ID_DEVIDS, 0), [0x06, 0, 0x27, 0x06, 0x01, 0, 0x01, 0]);
        // The bitmap is `sim_core::goldfish::keyboard_has`, up to ASSIST (585).
        let keys = cfg(&mut dev, CFG_EV_BITS, EV_KEY as u8);
        assert!((0..1024).all(|c| has(&keys, c) == keyboard_has(c)));
        assert!(has(&keys, 232) && has(&keys, 580) && !has(&keys, 0));
        assert_eq!(keys.len(), 585 / 8 + 1);
        assert_eq!(cfg(&mut dev, CFG_EV_BITS, EV_REP as u8), [0], "key repeat on");
        assert_eq!(cfg(&mut dev, CFG_EV_BITS, EV_LED as u8), [0b111]);
        assert!(cfg(&mut dev, CFG_PROP_BITS, 0).is_empty());

        let (mut rotary, _h) = Input::new(Kind::Rotary, "virtio_input_rotary");
        assert_eq!(cfg(&mut rotary, CFG_ID_DEVIDS, 0), [0x06, 0, 0xd1, 0x18, 0x01, 0, 0x01, 0]);
        assert!(cfg(&mut rotary, CFG_EV_BITS, EV_REP as u8).is_empty());
        assert!(has(&cfg(&mut rotary, CFG_EV_BITS, EV_REL as u8), REL_WHEEL));
    }
}
