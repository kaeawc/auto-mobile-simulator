//! virtio-input (virtio 1.2 §5.8): evdev-style events from the host into the guest.
//!
//! Four device kinds: a multi-touch screen (INPUT_PROP_DIRECT, so Android treats it as a
//! touchscreen), a keyboard (hardware keys incl. power/volume/back/home), a rotary encoder
//! (REL_WHEEL, used by Wear OS) and the stock emulator's virtio dual-mode mouse (absolute and
//! relative axes, mouse buttons; Android XR images expect it). Host code injects through an
//! [`InputHandle`].
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
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const BTN_TOOL_PEN: u16 = 0x140;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_GEAR_DOWN: u16 = 0x150;
pub const BTN_GEAR_UP: u16 = 0x151;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
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
/// The dual-mode mouse's absolute axes span 0..=32767 (stock `INPUT_EVENT_ABS_MAX`).
pub const MOUSE_ABS_MAX: u32 = 0x7fff;
/// The stock dual-mode mouse's name, which the guest matches on.
pub const DUAL_MODE_MOUSE_NAME: &str = "QEMU Virtio Dual Mode Mouse";
/// Buttons the stock dual-mode mouse declares (its `keymap_button`).
const MOUSE_BUTTONS: [u16; 9] = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_GEAR_UP, BTN_GEAR_DOWN, BTN_SIDE, BTN_EXTRA, BTN_TOUCH, BTN_TOOL_PEN];
const BUS_VIRTUAL: u16 = 0x06;

const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

/// The keyboard's name: goldfish's vendor image ships `qwerty2.idc` and `qwerty2.kl` for it.
pub const KEYBOARD_NAME: &str = "qwerty2";
/// The touchpad's name, which the guest's `virtio_input_multi_touch_touchpad_0.idc` configures
/// as `touchNavigation` (the display glasses' images ship it).
pub const TOUCHPAD_NAME: &str = "virtio_input_multi_touch_touchpad_0";
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
    Touch {
        width: u32,
        height: u32,
    },
    Keyboard,
    Rotary,
    /// `virtio-dual-mode-mouse` (stock `hw/input/virtio-input-hid.c`): absolute X/Y, relative
    /// X/Y and both wheels, the mouse buttons. Named [`DUAL_MODE_MOUSE_NAME`].
    DualModeMouse,
}

enum Msg {
    Events(Vec<InputEvent>),
    /// One finger on slot 0 at device pixels, down or lifted; the worker turns it into events
    /// (see [`TouchState`]).
    Touch {
        x: i32,
        y: i32,
        down: bool,
    },
    Kick,
    Stop,
}

/// A batch of events and how far into it the guest has taken.
struct Batch {
    events: Vec<InputEvent>,
    sent: usize,
    /// A move of the finger from this position (the one before it), which a newer move may
    /// replace while none of it has gone out.
    move_from: Option<(i32, i32)>,
}

/// The finger as the queued events leave it, so a move carries only what changed. Multi-touch
/// protocol B is stateful (the kernel keeps each slot's values and drops repeats, and the stock
/// emulator's `_mts_pointer_move` sends only the changed coordinates), so a move needs only the
/// axes that moved: the guest's evdev readers see exactly what the full set produced.
#[derive(Default)]
struct TouchState {
    /// The kernel's slot 0 holds what this state says: false until a down sets it all, and
    /// again after raw events (`event send`) may have changed it.
    known: bool,
    down: bool,
    x: i32,
    y: i32,
}

impl TouchState {
    /// The batch for a touch: the full set on a down (or when the state isn't known), the axes
    /// that changed on a move (none when nothing did), the lift on an up.
    fn batch(&mut self, x: i32, y: i32, down: bool) -> Option<Batch> {
        let batch = |events: Vec<InputEvent>, move_from| Some(Batch { events, sent: 0, move_from });
        if !down {
            let mut ev = Vec::with_capacity(4);
            if !self.known {
                ev.push(InputEvent::new(EV_ABS, ABS_MT_SLOT, 0));
            }
            ev.extend([InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1), InputEvent::new(EV_KEY, BTN_TOUCH, 0), InputEvent::syn()]);
            self.down = false;
            return batch(ev, None);
        }
        if self.known && self.down {
            let from = (self.x, self.y);
            (self.x, self.y) = (x, y);
            let ev = move_events(from, (x, y));
            return if ev.is_empty() { None } else { batch(ev, Some(from)) };
        }
        (self.known, self.down, self.x, self.y) = (true, true, x, y);
        batch(
            vec![
                InputEvent::new(EV_ABS, ABS_MT_SLOT, 0),
                InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, 1),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, x),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, y),
                InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0x81),
                InputEvent::new(EV_KEY, BTN_TOUCH, 1),
                InputEvent::new(EV_ABS, ABS_X, x),
                InputEvent::new(EV_ABS, ABS_Y, y),
                InputEvent::syn(),
            ],
            None,
        )
    }

    /// Raw events for this device: if they set any axis or `BTN_TOUCH`, the kernel's slot state
    /// may differ from this one, so the next touch sends the full set.
    fn raw(&mut self, events: &[InputEvent]) {
        if events.iter().any(|e| e.typ == EV_ABS || (e.typ == EV_KEY && e.code == BTN_TOUCH)) {
            self.known = false;
        }
    }
}

/// What a move from `from` to `to` sends: the multi-touch position and the single-touch axes
/// that changed, then `SYN_REPORT`; nothing when neither did.
fn move_events(from: (i32, i32), to: (i32, i32)) -> Vec<InputEvent> {
    let (dx, dy) = (from.0 != to.0, from.1 != to.1);
    if !dx && !dy {
        return Vec::new();
    }
    let mut ev = Vec::with_capacity(5);
    if dx {
        ev.push(InputEvent::new(EV_ABS, ABS_MT_POSITION_X, to.0));
    }
    if dy {
        ev.push(InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, to.1));
    }
    if dx {
        ev.push(InputEvent::new(EV_ABS, ABS_X, to.0));
    }
    if dy {
        ev.push(InputEvent::new(EV_ABS, ABS_Y, to.1));
    }
    ev.push(InputEvent::syn());
    ev
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

    /// Single-finger touch at device pixels; `down=false` lifts the finger. With the finger
    /// already down this is a move: it sends only the axes that changed, and replaces an older
    /// move the guest hasn't started taking yet (downs, ups and keys are never dropped).
    pub fn touch(&self, x: i32, y: i32, down: bool) {
        let _ = self.tx.send(Msg::Touch { x, y, down });
    }

    /// Press and release a key (Linux keycode).
    pub fn tap_key(&self, code: u16) {
        self.send(vec![InputEvent::new(EV_KEY, code, 1), InputEvent::syn(), InputEvent::new(EV_KEY, code, 0), InputEvent::syn()]);
    }

    /// Dual-mode mouse: moves to absolute axis values (0..=[`MOUSE_ABS_MAX`]) and, with `left`,
    /// presses (`true`) or releases (`false`) the left button there.
    pub fn pointer(&self, x: i32, y: i32, left: Option<bool>) {
        let mut ev = vec![InputEvent::new(EV_ABS, ABS_X, x), InputEvent::new(EV_ABS, ABS_Y, y)];
        if let Some(down) = left {
            ev.push(InputEvent::new(EV_KEY, BTN_LEFT, i32::from(down)));
        }
        ev.push(InputEvent::syn());
        self.send(ev);
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

    /// Takes the event batches queued while no guest driver has the device (before activation),
    /// as the host sent them: what an input would deliver, for tests and diagnostics.
    pub fn take_queued(&self) -> Vec<Vec<InputEvent>> {
        let Some(rx) = &self.rx else { return Vec::new() };
        let mut touch = TouchState::default();
        rx.try_iter()
            .filter_map(|m| match m {
                Msg::Events(e) => {
                    touch.raw(&e);
                    Some(e)
                }
                Msg::Touch { x, y, down } => touch.batch(x, y, down).map(|b| b.events),
                Msg::Kick | Msg::Stop => None,
            })
            .collect()
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
            (Kind::DualModeMouse, EV_KEY) => MOUSE_BUTTONS.into_iter().for_each(&mut set),
            (Kind::DualModeMouse, EV_REL) => [REL_X, REL_Y, REL_HWHEEL, REL_WHEEL].into_iter().for_each(&mut set),
            (Kind::DualModeMouse, EV_ABS) => [ABS_X, ABS_Y].into_iter().for_each(&mut set),
            (_, EV_SYN) => set(SYN_REPORT),
            _ => return Vec::new(),
        }
        let len = bits.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        bits.truncate(len);
        bits
    }

    fn abs_info(&self, axis: u8) -> Vec<u8> {
        let (min, max) = match (self.kind, u16::from(axis)) {
            (Kind::Touch { width, .. }, ABS_X | ABS_MT_POSITION_X) => (0, width.saturating_sub(1)),
            (Kind::Touch { height, .. }, ABS_Y | ABS_MT_POSITION_Y) => (0, height.saturating_sub(1)),
            (Kind::Touch { .. }, ABS_MT_SLOT) => (0, MAX_SLOTS - 1),
            (Kind::Touch { .. }, ABS_MT_TRACKING_ID) => (0, 0xffff),
            (Kind::Touch { .. }, ABS_MT_PRESSURE) => (0, 0xff),
            (Kind::DualModeMouse, ABS_X | ABS_Y) => (0, MOUSE_ABS_MAX),
            _ => return Vec::new(),
        };
        [min, max, 0, 0, 0].iter().flat_map(|v: &u32| v.to_le_bytes()).collect()
    }

    fn config_payload(&self) -> Vec<u8> {
        let (sel, sub) = self.select;
        match sel {
            CFG_ID_NAME => self.name.as_bytes().to_vec(),
            CFG_ID_SERIAL => b"emu-rs".to_vec(),
            // bustype, vendor, product, version. The keyboard and the dual-mode mouse have stock's
            // ids: QEMU's USB HID vendor and the product numbers their guest drivers bind to.
            CFG_ID_DEVIDS => match self.kind {
                Kind::Keyboard => KEYBOARD_IDS,
                Kind::DualModeMouse => [BUS_VIRTUAL, 0x0627, 0x0004, 0x0001],
                _ => OTHER_IDS,
            }
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
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
        let w = Worker::new(act.mem, act.irq, Queue::new(eventq, false), statusq.map(|q| Queue::new(q, false)));
        let h = std::thread::Builder::new().name(format!("input-{}", self.name)).spawn(move || w.run(rx)).map_err(|e| e.to_string())?;
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
    /// Batches waiting for guest buffers, in order (bounded: the oldest dropped beyond
    /// [`Worker::MAX_PENDING`] events).
    pending: VecDeque<Batch>,
    /// Events in `pending` not yet sent.
    queued: usize,
    touch: TouchState,
}

impl Worker {
    const MAX_PENDING: usize = 4096;

    fn new(mem: Arc<GuestRam>, irq: Arc<Interrupt>, eventq: Queue, statusq: Option<Queue>) -> Worker {
        Worker { mem, irq, eventq, statusq, pending: VecDeque::new(), queued: 0, touch: TouchState::default() }
    }

    /// Handles messages until `Stop` (or every sender is gone), taking everything already
    /// waiting before each flush: moves that arrived together go out as one, with one interrupt.
    fn run(mut self, rx: Receiver<Msg>) -> Receiver<Msg> {
        while let Ok(msg) = rx.recv() {
            if !self.handle(msg) {
                return rx;
            }
            while let Ok(msg) = rx.try_recv() {
                if !self.handle(msg) {
                    return rx;
                }
            }
            self.flush();
        }
        rx
    }

    /// False on `Stop`.
    fn handle(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Stop => return false,
            Msg::Events(events) => {
                self.touch.raw(&events);
                self.push(Batch { events, sent: 0, move_from: None });
            }
            Msg::Touch { x, y, down } => {
                if let Some(b) = self.touch.batch(x, y, down) {
                    self.push(b);
                }
            }
            Msg::Kick => self.drain_status(),
        }
        true
    }

    /// Queues a batch. A move replaces the move queued just before it when none of that has
    /// gone to the guest: one batch from where the older one started to where this one ends.
    fn push(&mut self, mut b: Batch) {
        if b.move_from.is_some()
            && let Some(last) = self.pending.back()
            && last.sent == 0
            && let Some(from) = last.move_from
        {
            self.queued -= last.events.len();
            self.pending.pop_back();
            // The touch state is where this move ends.
            b.events = move_events(from, (self.touch.x, self.touch.y));
            if b.events.is_empty() {
                return; // back where the older move started: nothing to send
            }
            b.move_from = Some(from);
        }
        self.queued += b.events.len();
        self.pending.push_back(b);
        // Bounded: whole batches go, oldest first, never the one the guest is part way into.
        while self.queued > Self::MAX_PENDING && self.pending.len() > 1 {
            let at = usize::from(self.pending.front().is_some_and(|f| f.sent > 0));
            if let Some(gone) = self.pending.remove(at) {
                self.queued -= gone.events.len() - gone.sent;
                // The guest never sees this batch, so what it knows of the contact no longer
                // matches ours: the next touch sends the full set again.
                self.touch.known = false;
            }
        }
    }

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

    /// Moves queued events into guest buffers, one event each, then raises one interrupt.
    fn flush(&mut self) {
        let mut used = false;
        while let Some(b) = self.pending.front_mut() {
            let Some(ev) = b.events.get(b.sent).copied() else {
                self.pending.pop_front();
                continue;
            };
            let chain = match self.eventq.pop(&self.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let n = chain.write_all(&self.mem, &ev.bytes()).unwrap_or(0);
            let _ = self.eventq.add_used(&self.mem, chain.head, n);
            b.sent += 1;
            self.queued -= 1;
            used = true;
        }
        if used {
            self.irq.raise(INT_USED_RING);
        }
    }
}

/// Ways for the host to inject input into the running guest.
#[derive(Clone)]
pub struct Inputs {
    /// The screen's size in pixels.
    pub screen: (u32, u32),
    /// The multi-touch screen; none on a `hw.screen=no-touch` device (TV), as on stock.
    pub touch: Option<InputHandle>,
    pub keyboard: InputHandle,
    pub rotary: InputHandle,
    /// The dual-mode mouse (Android XR images only).
    pub mouse: Option<InputHandle>,
    /// A touchpad beside the screen (`hw.touchpad0`: Android XR display glasses).
    pub touchpad: Option<InputHandle>,
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
    fn dual_mode_mouse_config_matches_stock() {
        let (mut dev, _h) = Input::new(Kind::DualModeMouse, DUAL_MODE_MOUSE_NAME);
        assert_eq!(cfg(&mut dev, CFG_ID_NAME, 0), b"QEMU Virtio Dual Mode Mouse");
        // BUS_VIRTUAL, vendor 0x0627, product 0x0004, version 1, little-endian.
        assert_eq!(cfg(&mut dev, CFG_ID_DEVIDS, 0), [0x06, 0, 0x27, 0x06, 0x04, 0, 0x01, 0]);
        assert!(cfg(&mut dev, CFG_PROP_BITS, 0).is_empty(), "a pointer, not a direct-touch screen");
        // EV_ABS: X and Y; EV_REL: X, Y, HWHEEL, WHEEL (stock's two bitmap bytes).
        assert_eq!(cfg(&mut dev, CFG_EV_BITS, EV_ABS as u8), [0b11]);
        assert_eq!(cfg(&mut dev, CFG_EV_BITS, EV_REL as u8), [0b0100_0011, 0b1]);
        let keys = cfg(&mut dev, CFG_EV_BITS, EV_KEY as u8);
        let has = |code: u16| keys.get(code as usize / 8).is_some_and(|b| b & (1 << (code % 8)) != 0);
        for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA, BTN_GEAR_UP, BTN_GEAR_DOWN, BTN_TOUCH, BTN_TOOL_PEN] {
            assert!(has(b), "{b:#x}");
        }
        assert_eq!(keys.iter().map(|b| b.count_ones()).sum::<u32>(), 9);
        assert_eq!(keys.len(), BTN_GEAR_UP as usize / 8 + 1);
        for axis in [ABS_X, ABS_Y] {
            // min 0, max 32767, fuzz, flat, resolution 0.
            let info = cfg(&mut dev, CFG_ABS_INFO, axis as u8);
            assert_eq!(info, [0u32, 32767, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        }
        assert!(cfg(&mut dev, CFG_ABS_INFO, ABS_MT_POSITION_X as u8).is_empty());
        // The keyboard has stock's `qwerty2` ids; the rotary keeps simee's.
        let (mut kbd, _k) = Input::new(Kind::Keyboard, "qwerty2");
        assert_eq!(cfg(&mut kbd, CFG_ID_DEVIDS, 0), [0x06, 0, 0x27, 0x06, 0x01, 0, 0x01, 0]);
        let (mut rotary, _r) = Input::new(Kind::Rotary, "virtio_input_rotary");
        assert_eq!(cfg(&mut rotary, CFG_ID_DEVIDS, 0), [0x06, 0, 0xd1, 0x18, 0x01, 0, 0x01, 0]);
    }

    #[test]
    fn pointer_events_move_then_press() {
        let (mut dev, h) = Input::new(Kind::DualModeMouse, DUAL_MODE_MOUSE_NAME);
        let rx = dev.rx.take().expect("receiver");
        h.pointer(100, 200, Some(true));
        let Ok(Msg::Events(ev)) = rx.try_recv() else { panic!("no events") };
        assert_eq!(
            ev,
            [
                InputEvent::new(EV_ABS, ABS_X, 100),
                InputEvent::new(EV_ABS, ABS_Y, 200),
                InputEvent::new(EV_KEY, BTN_LEFT, 1),
                InputEvent::syn()
            ]
        );
        h.pointer(5, 6, None);
        let Ok(Msg::Events(ev)) = rx.try_recv() else { panic!("no events") };
        assert_eq!(ev.len(), 3);
    }

    #[test]
    fn events_flow_into_guest_buffers() {
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let mut drv = Driver::new(8);
        for i in 0..4 {
            drv.push(&mem, &[(RAM_BASE + 0x20000 + i * 8, 8, true)]);
        }
        let mut w = Worker::new(mem.clone(), Arc::new(Interrupt::new(line.clone())), Queue::new(drv.cfg, false), None);
        w.handle(Msg::Events(vec![InputEvent::new(EV_KEY, 116, 1), InputEvent::syn()]));
        w.flush();
        assert!(line.level());
        assert_eq!(drv.used_idx(&mem), 2);
        assert_eq!(mem.read_u16(RAM_BASE + 0x20000 + 2).expect("code"), 116);
    }

    /// Counts the times the interrupt line goes up.
    #[derive(Default)]
    struct Raises(std::sync::atomic::AtomicUsize);

    impl crate::irq::IrqLine for Raises {
        fn set(&self, level: bool) {
            if level {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    const BUFS: u64 = RAM_BASE + 0x20000;

    /// A touch worker over a 64-entry event queue (the device's size) with `bufs` buffers posted.
    struct Rig {
        mem: Arc<GuestRam>,
        drv: Driver,
        irq: Arc<Interrupt>,
        raises: Arc<Raises>,
        w: Worker,
        posted: u64,
        read: u16,
    }

    impl Rig {
        fn new(bufs: u64) -> Rig {
            let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
            let raises = Arc::new(Raises::default());
            let irq = Arc::new(Interrupt::new(raises.clone()));
            let drv = Driver::new(64);
            let w = Worker::new(mem.clone(), irq.clone(), Queue::new(drv.cfg, false), None);
            let mut rig = Rig { mem, drv, irq, raises, w, posted: 0, read: 0 };
            rig.post(bufs);
            rig
        }

        /// The guest posts `n` more one-event buffers (descriptors recycled in order).
        fn post(&mut self, n: u64) {
            for _ in 0..n {
                self.drv.push(&self.mem, &[(BUFS + (self.posted % 64) * 8, 8, true)]);
                self.posted += 1;
            }
        }

        /// The events the guest got since the last call; acks the interrupt like the driver.
        fn take(&mut self) -> Vec<InputEvent> {
            let mut out = Vec::new();
            while self.read != self.drv.used_idx(&self.mem) {
                let (id, len) = self.drv.used(&self.mem, self.read % 64);
                assert_eq!(len, 8);
                let at = BUFS + u64::from(id) * 8;
                let (typ, code) = (self.mem.read_u16(at).expect("type"), self.mem.read_u16(at + 2).expect("code"));
                out.push(InputEvent::new(typ, code, self.mem.read_u32(at + 4).expect("value") as i32));
                self.read = self.read.wrapping_add(1);
            }
            self.irq.ack(INT_USED_RING);
            out
        }

        fn send(&mut self, msgs: impl IntoIterator<Item = Msg>) {
            for m in msgs {
                self.w.handle(m);
            }
            self.w.flush();
        }

        fn raised(&self) -> usize {
            self.raises.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn touch(x: i32, y: i32, down: bool) -> Msg {
        Msg::Touch { x, y, down }
    }

    #[test]
    fn a_down_sends_the_full_set_and_a_move_only_what_changed() {
        let mut rig = Rig::new(64);
        rig.send([touch(10, 20, true)]);
        assert_eq!(
            rig.take(),
            [
                InputEvent::new(EV_ABS, ABS_MT_SLOT, 0),
                InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, 1),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 10),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 20),
                InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0x81),
                InputEvent::new(EV_KEY, BTN_TOUCH, 1),
                InputEvent::new(EV_ABS, ABS_X, 10),
                InputEvent::new(EV_ABS, ABS_Y, 20),
                InputEvent::syn(),
            ]
        );
        rig.send([touch(11, 21, true)]);
        assert_eq!(
            rig.take(),
            [
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 11),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 21),
                InputEvent::new(EV_ABS, ABS_X, 11),
                InputEvent::new(EV_ABS, ABS_Y, 21),
                InputEvent::syn(),
            ]
        );
        // One axis moved: one axis goes. Nothing moved: nothing goes (as stock's move bails).
        rig.send([touch(11, 25, true)]);
        assert_eq!(rig.take(), [InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 25), InputEvent::new(EV_ABS, ABS_Y, 25), InputEvent::syn()]);
        let raised = rig.raised();
        rig.send([touch(11, 25, true)]);
        assert!(rig.take().is_empty());
        assert_eq!(rig.raised(), raised, "no interrupt for nothing");
        rig.send([touch(11, 25, false)]);
        assert_eq!(rig.take(), [InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1), InputEvent::new(EV_KEY, BTN_TOUCH, 0), InputEvent::syn()]);
        // The next touch is a down again: the full set.
        rig.send([touch(5, 5, true)]);
        assert_eq!(rig.take().len(), 9);
    }

    #[test]
    fn raw_events_make_the_next_touch_send_everything() {
        let mut rig = Rig::new(64);
        rig.send([touch(10, 20, true)]);
        rig.take();
        // `event send EV_ABS:ABS_MT_SLOT:3`: the kernel's slot is no longer ours.
        rig.send([Msg::Events(vec![InputEvent::new(EV_ABS, ABS_MT_SLOT, 3), InputEvent::syn()])]);
        rig.take();
        rig.send([touch(11, 20, true)]);
        let ev = rig.take();
        assert_eq!(ev[0], InputEvent::new(EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(ev.len(), 9);
        // Keys on another device are none of this: the rotary's REL events don't count either.
        rig.send([Msg::Events(vec![InputEvent::new(EV_REL, REL_WHEEL, 1), InputEvent::syn()])]);
        rig.take();
        rig.send([touch(12, 20, true)]);
        assert_eq!(rig.take().len(), 3, "x and its single-touch axis, then SYN");
    }

    /// N moves while the guest has no buffers: before, 9 events each (the 64-entry queue took
    /// the oldest seven and stale positions kept coming); now one move to the newest position.
    #[test]
    fn moves_the_guest_has_not_taken_collapse_into_the_newest() {
        const N: i32 = 100;
        let mut rig = Rig::new(0);
        rig.send([touch(0, 0, true)]);
        for i in 1..=N {
            rig.send([touch(i, 2 * i, true)]);
        }
        assert_eq!(rig.w.pending.len(), 2, "the down and one move");
        assert_eq!(rig.w.queued, 9 + 5);
        rig.post(64);
        rig.send([Msg::Kick]);
        let ev = rig.take();
        assert_eq!(ev.len(), 14);
        assert_eq!(rig.raised(), 1, "one interrupt");
        assert_eq!(
            ev[9..],
            [
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, N),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 2 * N),
                InputEvent::new(EV_ABS, ABS_X, N),
                InputEvent::new(EV_ABS, ABS_Y, 2 * N),
                InputEvent::syn()
            ]
        );
        // A move back to where the queued one started cancels it.
        let mut stalled = Rig::new(0);
        stalled.send([touch(0, 0, true), touch(5, 5, true), touch(0, 0, true)]);
        assert_eq!(stalled.w.pending.len(), 1, "just the down");
    }

    #[test]
    fn downs_ups_and_keys_are_never_dropped_or_reordered() {
        let mut rig = Rig::new(0);
        let key = |v| Msg::Events(vec![InputEvent::new(EV_KEY, 116, v), InputEvent::syn()]);
        rig.send([touch(1, 1, true), touch(2, 2, true), touch(3, 3, true), key(1), touch(4, 4, true), touch(5, 5, true)]);
        rig.send([touch(5, 5, false), key(0), touch(9, 9, true), touch(8, 8, true), touch(7, 7, false)]);
        rig.post(64);
        rig.send([Msg::Kick]);
        let ev = rig.take();
        // Each batch's first event, in order: down, move (to 3), key, move (to 5), up, key,
        // down, move (to 8), up.
        let mut firsts = Vec::new();
        let mut start = true;
        for e in &ev {
            if start {
                firsts.push(*e);
            }
            start = *e == InputEvent::syn();
        }
        assert_eq!(
            firsts,
            [
                InputEvent::new(EV_ABS, ABS_MT_SLOT, 0),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 3),
                InputEvent::new(EV_KEY, 116, 1),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 5),
                InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1),
                InputEvent::new(EV_KEY, 116, 0),
                InputEvent::new(EV_ABS, ABS_MT_SLOT, 0),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 8),
                InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1),
            ]
        );
    }

    #[test]
    fn a_move_the_guest_is_part_way_into_is_left_alone() {
        let mut rig = Rig::new(2);
        rig.send([touch(0, 0, true)]);
        assert_eq!(rig.take().len(), 2);
        rig.post(7);
        rig.send([Msg::Kick]);
        assert_eq!(rig.take().len(), 7, "the down is in");
        rig.send([touch(1, 1, true)]);
        rig.take();
        rig.post(2);
        rig.send([touch(2, 2, true)]);
        assert_eq!(rig.take().len(), 2, "half of the move to 2,2");
        rig.send([touch(3, 3, true), touch(4, 4, true)]);
        assert_eq!(rig.w.pending.len(), 2, "the started move stays; the new ones collapse behind it");
        rig.post(64);
        rig.send([Msg::Kick]);
        let ev = rig.take();
        assert_eq!(ev.len(), 3 + 5);
        assert_eq!(ev[3], InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 4));
    }

    #[test]
    fn the_backlog_is_bounded_by_whole_batches() {
        let mut rig = Rig::new(0);
        let key = |v| Msg::Events(vec![InputEvent::new(EV_KEY, 30, v), InputEvent::syn()]);
        for i in 0..3000 {
            rig.send([key(i % 2)]);
        }
        assert!(rig.w.queued <= Worker::MAX_PENDING);
        assert!(rig.w.pending.iter().all(|b| b.events.len() == 2));
        assert_eq!(rig.w.queued, rig.w.pending.len() * 2);
    }

    /// Threads touching and pressing keys through handles while a guest thread takes events and
    /// kicks: every key arrives once, in each thread's order, and the finger ends where it was
    /// last put.
    #[test]
    fn handles_and_the_guest_hammering_together() {
        const THREADS: u16 = 4;
        // Under the backlog bound even if the guest took nothing until the end (1200 key
        // batches of 2, 150 moves of at most 5), so nothing may be dropped.
        const KEYS: i32 = 150;
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let raises = Arc::new(Raises::default());
        let irq = Arc::new(Interrupt::new(raises.clone()));
        let (mut dev, h) = Input::new(Kind::Touch { width: 100, height: 100 }, "touch");
        let mut drv = Driver::new(64);
        let cfg = drv.cfg;
        let mut posted = 0u64;
        for _ in 0..64 {
            drv.push(&mem, &[(BUFS + (posted % 64) * 8, 8, true)]);
            posted += 1;
        }
        dev.activate(Activation { mem: mem.clone(), irq: irq.clone(), queues: vec![Some(cfg), None], features: 0 }).expect("activate");
        let dev = Arc::new(std::sync::Mutex::new(dev));
        let senders: Vec<_> = (0..THREADS)
            .map(|t| {
                let h = h.clone();
                std::thread::spawn(move || {
                    for i in 0..KEYS {
                        h.send(vec![InputEvent::new(EV_KEY, 100 + t, i), InputEvent::syn()]);
                        if t == 0 {
                            h.touch(i % 50, i % 70, true);
                        }
                    }
                    if t == 0 {
                        h.touch(7, 9, true);
                    }
                })
            })
            .collect();
        let (mut read, mut got) = (0u16, Vec::new());
        let start = std::time::Instant::now();
        let mut finished = false;
        loop {
            let used = drv.used_idx(&mem);
            while read != used {
                let (id, _) = drv.used(&mem, read % 64);
                let at = BUFS + u64::from(id) * 8;
                got.push(InputEvent::new(
                    mem.read_u16(at).expect("type"),
                    mem.read_u16(at + 2).expect("code"),
                    mem.read_u32(at + 4).expect("value") as i32,
                ));
                read = read.wrapping_add(1);
                drv.push(&mem, &[(BUFS + (posted % 64) * 8, 8, true)]);
                posted += 1;
            }
            irq.ack(INT_USED_RING);
            dev.lock().expect("dev").notify(0);
            let keys = got.iter().filter(|e| e.typ == EV_KEY && (100..100 + THREADS).contains(&e.code)).count();
            // The last touch is in once its x is.
            if finished
                && keys == (THREADS as usize) * KEYS as usize
                && got.iter().rev().find(|e| e.code == ABS_MT_POSITION_X).is_some_and(|e| e.value == 7)
            {
                break;
            }
            finished = finished || senders.iter().all(|s| s.is_finished());
            assert!(start.elapsed() < std::time::Duration::from_secs(20), "stalled with {} events", got.len());
            std::thread::yield_now();
        }
        for s in senders {
            s.join().expect("sender");
        }
        for t in 0..THREADS {
            let mine: Vec<i32> = got.iter().filter(|e| e.typ == EV_KEY && e.code == 100 + t).map(|e| e.value).collect();
            assert_eq!(mine, (0..KEYS).collect::<Vec<_>>(), "thread {t}'s keys, once each, in order");
        }
        let y = got.iter().rev().find(|e| e.code == ABS_MT_POSITION_Y).map(|e| e.value);
        assert_eq!(y, Some(9));
        // Every batch ends with SYN: nothing was cut.
        assert_eq!(got.last(), Some(&InputEvent::syn()));
        drop(dev);
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
