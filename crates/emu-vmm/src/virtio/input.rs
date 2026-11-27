//! virtio-input (virtio 1.2 §5.8): evdev-style events from the host into the guest.
//!
//! Four device kinds: a multi-touch screen (stock's `virtio-input-multi-touch.c`: axes
//! 0..=32767, no input properties; the images' `virtio_input_multi_touch_1.idc` makes it a touch
//! screen, so its name must stay stock's), a
//! keyboard (hardware keys incl. power/volume/back/home), a rotary encoder (REL_WHEEL, used by
//! Wear OS) and the stock emulator's virtio dual-mode mouse (absolute and relative axes, mouse
//! buttons; Android XR images expect it). Host code injects through an [`InputHandle`], in
//! device pixels. A touch screen also takes a stylus, as stock's do ([`PenState`]).
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
use sim_core::stylus::{PenPhase, PenSample};

use super::mmio::F_RING_EVENT_IDX;
use super::queue::{Chain, Queue};
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
pub const BTN_TOOL_RUBBER: u16 = 0x141;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_STYLUS: u16 = 0x14b;
pub const BTN_GEAR_DOWN: u16 = 0x150;
pub const BTN_GEAR_UP: u16 = 0x151;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const EV_SW: u16 = 0x05;
pub const SW_LID: u16 = 0x00;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_TOUCH_MINOR: u16 = 0x31;
pub const ABS_MT_ORIENTATION: u16 = 0x34;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TOOL_TYPE: u16 = 0x37;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;
pub const INPUT_PROP_DIRECT: u16 = 0x01;
pub const MAX_SLOTS: u32 = 10;
/// `ABS_MT_TOOL_TYPE` values: a pen, and `MT_TOOL_MAX`, which stock sends for the pen's eraser
/// end (with `BTN_TOOL_RUBBER`).
pub const MT_TOOL_PEN: i32 = 1;
pub const MT_TOOL_MAX: i32 = 0x0f;
/// The stylus's slot and tracking id: the one after the fingers' (stock's `MTS_POINTER_PEN`), so
/// a pen never takes a finger's contact.
pub const PEN_SLOT: i32 = MAX_SLOTS as i32;
/// Stock's pressure for a pen pressed without one (raw, on the 0..=[`SCREEN_PRESSURE_MAX`] axis).
pub const PEN_PRESSURE: i32 = 0x81;
/// A finger on the screen, as stock's mouse (its window, gRPC `sendMouse`) puts one down:
/// full pressure and this touch size on both axes (stock 37.2.13, `getevent -lt`).
pub const FINGER_PRESSURE: i32 = SCREEN_PRESSURE_MAX as i32;
pub const FINGER_SIZE: i32 = 0x500;
/// A finger on the touchpad: its pressure, and its tracking id (the screen's is its slot, 0).
const PAD_PRESSURE: i32 = 0x81;
const PAD_TRACKING_ID: i32 = 1;
/// The dual-mode mouse's absolute axes and the touch screen's span 0..=32767 (stock
/// `INPUT_EVENT_ABS_MAX`), whatever the display's size.
pub const MOUSE_ABS_MAX: u32 = 0x7fff;
pub const SCREEN_ABS_MAX: u32 = MOUSE_ABS_MAX;
/// The touch screen's `ABS_MT_PRESSURE` max (stock's).
pub const SCREEN_PRESSURE_MAX: u32 = 0x400;
/// The touch screen's `ABS_MT_ORIENTATION` max (stock's, in degrees).
pub const SCREEN_ORIENTATION_MAX: u32 = 90;
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
/// Stock's touch screens: BUS_VIRTUAL and no vendor, product or version.
const SCREEN_IDS: [u16; 4] = [0x06, 0, 0, 0];

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
    /// A multi-touch surface of `width`×`height` pixels. `screen`: stock's touch screen
    /// (`virtio-input-multi-touch.c`): axes 0..=[`SCREEN_ABS_MAX`] that host pixels are scaled
    /// to, pressure 0..=[`SCREEN_PRESSURE_MAX`], touch size and orientation axes, `ABS_Z`,
    /// `SW_LID`, no `BTN_TOUCH` and no input properties, and a stylus (`ABS_MT_TOOL_TYPE`,
    /// `BTN_TOOL_RUBBER`, `BTN_STYLUS` and a slot for the pen; see [`PenState`]). Otherwise the
    /// display glasses' touchpad: axes in its own units, `BTN_TOUCH`, pressure 0..=255,
    /// `INPUT_PROP_DIRECT`, no pen.
    Touch {
        width: u32,
        height: u32,
        screen: bool,
    },
    Keyboard,
    Rotary,
    /// `virtio-dual-mode-mouse` (stock `hw/input/virtio-input-hid.c`): absolute X/Y, relative
    /// X/Y and both wheels, the mouse buttons. Named [`DUAL_MODE_MOUSE_NAME`].
    DualModeMouse,
}

impl Kind {
    /// A position in device pixels as this device's axis values. The screen's span
    /// 0..=[`SCREEN_ABS_MAX`]; Android maps a raw value `r` to `r × size / 32768` pixels, so `p`
    /// goes to the smallest `r` at or past `p`: the guest's position is `p` plus under a pixel,
    /// and lands on pixel `p` (any other device: unchanged).
    pub fn axis(self, x: i32, y: i32) -> (i32, i32) {
        match self {
            Kind::Touch { width, height, screen: true } => (to_axis(x, width), to_axis(y, height)),
            _ => (x, y),
        }
    }

    /// What this device's contacts send (see [`Contact`]).
    fn contact(self) -> Contact {
        Contact { screen: matches!(self, Kind::Touch { screen: true, .. }) }
    }
}

/// `p` pixels of `size` on a 0..=[`SCREEN_ABS_MAX`] axis (see [`Kind::axis`]).
fn to_axis(p: i32, size: u32) -> i32 {
    if size == 0 {
        return 0;
    }
    let span = u64::from(SCREEN_ABS_MAX) + 1;
    let r = (u64::from(p.max(0).unsigned_abs()) * span).div_ceil(u64::from(size));
    r.min(u64::from(SCREEN_ABS_MAX)) as i32
}

/// How a touch device's contacts look on the wire: stock's screen's, or the touchpad's.
///
/// Stock's screen (`getevent -lt` on 37.2.13): a finger's down is its tracking id (its slot),
/// position, pressure and touch size; a move only the position that changed; a lift zero
/// pressure, then tracking id -1. No `BTN_TOUCH`, so Android reads a slot in use with zero
/// pressure as hovering, any other as touching, and no single-touch `ABS_X`/`ABS_Y` (Android's
/// multi-touch mapper reads only the slots). The touchpad's are simee's own: `BTN_TOUCH`, the
/// single-touch axes beside the slot's, pressure 0..=255.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Contact {
    screen: bool,
}

impl Contact {
    /// A host's pressure (1..=255, as the console's `pen` takes) on this device's pressure axis,
    /// rounded.
    fn pressure(self, p: i32) -> i32 {
        if self.screen { (p * SCREEN_PRESSURE_MAX as i32 + 127) / 255 } else { p }
    }
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
    /// One stylus sample (see [`PenState`]).
    Pen(PenSample),
    Kick,
    Stop,
}

/// The stylus as the queued events leave it. Stock's pen protocol (`multitouch-screen.c`): its
/// own slot ([`PEN_SLOT`]), `ABS_MT_TOOL_TYPE` pen, or `MT_TOOL_MAX` with `BTN_TOOL_RUBBER` for
/// the eraser, the barrel button as `BTN_STYLUS`, and the changed axes on a move. Contact is
/// pressure alone (the screen has no `BTN_TOUCH`): a pen in range with none is hovering, which
/// Android reports as hover events from a stylus, and a finger's contact is its own slot's.
#[derive(Default)]
struct PenState {
    in_range: bool,
    touching: bool,
    x: i32,
    y: i32,
    /// On the device's axis ([`Contact::pressure`]).
    pressure: i32,
    button: bool,
    eraser: bool,
}

impl PenState {
    /// The batch for a sample (its position already on the device's axes); `None` when nothing
    /// changes (a leave while out of range, a repeated sample).
    fn batch(&mut self, s: &PenSample, c: Contact) -> Option<Batch> {
        let mut ev = vec![InputEvent::new(EV_ABS, ABS_MT_SLOT, PEN_SLOT)];
        // A leave, or the other end of the pen: the contact ends.
        if self.in_range && (s.phase == PenPhase::Leave || s.eraser != self.eraser) {
            ev.push(InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0));
            if self.eraser {
                ev.push(InputEvent::new(EV_KEY, BTN_TOOL_RUBBER, 0));
            }
            if self.button {
                ev.push(InputEvent::new(EV_KEY, BTN_STYLUS, 0));
            }
            ev.push(InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1));
            ev.push(InputEvent::syn());
            *self = PenState::default();
        }
        if s.phase == PenPhase::Leave {
            return (ev.len() > 1).then_some(Batch { events: ev, sent: 0, move_from: None });
        }
        let touching = match s.phase {
            PenPhase::Down => true,
            PenPhase::Move => self.touching,
            _ => false,
        };
        // Touching without a pressure keeps the one it has (a press's default to begin with).
        let kept = if self.touching { self.pressure } else { PEN_PRESSURE };
        let pressure = if touching { s.pressure.map_or(kept, |p| c.pressure(i32::from(p)).max(1)) } else { 0 };
        let start = ev.len();
        if !self.in_range {
            if start > 1 {
                ev.push(InputEvent::new(EV_ABS, ABS_MT_SLOT, PEN_SLOT));
            }
            ev.extend([
                InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, PEN_SLOT),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_X, s.x),
                InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, s.y),
                InputEvent::new(EV_ABS, ABS_MT_TOOL_TYPE, if s.eraser { MT_TOOL_MAX } else { MT_TOOL_PEN }),
            ]);
            if s.eraser {
                ev.push(InputEvent::new(EV_KEY, BTN_TOOL_RUBBER, 1));
            }
            ev.push(InputEvent::new(EV_ABS, ABS_MT_PRESSURE, pressure));
            if s.button {
                ev.push(InputEvent::new(EV_KEY, BTN_STYLUS, 1));
            }
        } else {
            if s.x != self.x {
                ev.push(InputEvent::new(EV_ABS, ABS_MT_POSITION_X, s.x));
            }
            if s.y != self.y {
                ev.push(InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, s.y));
            }
            if pressure != self.pressure {
                ev.push(InputEvent::new(EV_ABS, ABS_MT_PRESSURE, pressure));
            }
            if s.button != self.button {
                ev.push(InputEvent::new(EV_KEY, BTN_STYLUS, i32::from(s.button)));
            }
        }
        *self = PenState { in_range: true, touching, x: s.x, y: s.y, pressure, button: s.button, eraser: s.eraser };
        if ev.len() == start {
            return None; // in range already, and nothing changed
        }
        ev.push(InputEvent::syn());
        Some(Batch { events: ev, sent: 0, move_from: None })
    }
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
    /// The batch for a touch (at the device's axis values): the full set on a down (or when the
    /// state isn't known), the axes that changed on a move (none when nothing did), the lift on
    /// an up; `BTN_TOUCH` with the down and the up where the device has it.
    fn batch(&mut self, x: i32, y: i32, down: bool, c: Contact) -> Option<Batch> {
        let batch = |events: Vec<InputEvent>, move_from| Some(Batch { events, sent: 0, move_from });
        let abs = |code, value| InputEvent::new(EV_ABS, code, value);
        if !down {
            let mut ev = Vec::with_capacity(5);
            if !self.known {
                ev.push(abs(ABS_MT_SLOT, 0));
            }
            if c.screen {
                ev.push(abs(ABS_MT_PRESSURE, 0));
            }
            ev.push(abs(ABS_MT_TRACKING_ID, -1));
            if !c.screen {
                ev.push(InputEvent::new(EV_KEY, BTN_TOUCH, 0));
            }
            ev.push(InputEvent::syn());
            self.down = false;
            return batch(ev, None);
        }
        if self.known && self.down {
            let from = (self.x, self.y);
            (self.x, self.y) = (x, y);
            let ev = move_events(from, (x, y), c);
            return if ev.is_empty() { None } else { batch(ev, Some(from)) };
        }
        (self.known, self.down, self.x, self.y) = (true, true, x, y);
        let ev = if c.screen {
            vec![
                abs(ABS_MT_SLOT, 0),
                abs(ABS_MT_TRACKING_ID, 0),
                abs(ABS_MT_POSITION_X, x),
                abs(ABS_MT_POSITION_Y, y),
                abs(ABS_MT_PRESSURE, FINGER_PRESSURE),
                abs(ABS_MT_TOUCH_MAJOR, FINGER_SIZE),
                abs(ABS_MT_TOUCH_MINOR, FINGER_SIZE),
                InputEvent::syn(),
            ]
        } else {
            vec![
                abs(ABS_MT_SLOT, 0),
                abs(ABS_MT_TRACKING_ID, PAD_TRACKING_ID),
                abs(ABS_MT_POSITION_X, x),
                abs(ABS_MT_POSITION_Y, y),
                abs(ABS_MT_PRESSURE, PAD_PRESSURE),
                InputEvent::new(EV_KEY, BTN_TOUCH, 1),
                abs(ABS_X, x),
                abs(ABS_Y, y),
                InputEvent::syn(),
            ]
        };
        batch(ev, None)
    }

    /// Raw events for this device: if they set any axis or `BTN_TOUCH`, the kernel's slot state
    /// may differ from this one, so the next touch sends the full set.
    fn raw(&mut self, events: &[InputEvent]) {
        if events.iter().any(|e| e.typ == EV_ABS || (e.typ == EV_KEY && e.code == BTN_TOUCH)) {
            self.known = false;
        }
    }
}

/// What a move from `from` to `to` sends: the multi-touch position that changed (and on the
/// touchpad its single-touch axis), then `SYN_REPORT`; nothing when neither did.
fn move_events(from: (i32, i32), to: (i32, i32), c: Contact) -> Vec<InputEvent> {
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
    if !c.screen && dx {
        ev.push(InputEvent::new(EV_ABS, ABS_X, to.0));
    }
    if !c.screen && dy {
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
        let (x, y) = self.kind.axis(x, y);
        let _ = self.tx.send(Msg::Touch { x, y, down });
    }

    /// One stylus sample at device pixels (a touch screen; see [`PenState`]).
    pub fn pen(&self, sample: PenSample) {
        let (x, y) = self.kind.axis(sample.x, sample.y);
        let _ = self.tx.send(Msg::Pen(PenSample { x, y, ..sample }));
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
        let (mut touch, mut pen, c) = (TouchState::default(), PenState::default(), self.kind.contact());
        rx.try_iter()
            .filter_map(|m| match m {
                Msg::Events(e) => {
                    touch.raw(&e);
                    Some(e)
                }
                Msg::Touch { x, y, down } => touch.batch(x, y, down, c).map(|b| b.events),
                Msg::Pen(s) => pen.batch(&s, c).map(|b| {
                    touch.raw(&b.events);
                    b.events
                }),
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
            // Stock's touch screens: the pen's eraser end and barrel button, no BTN_TOUCH.
            (Kind::Touch { screen: true, .. }, EV_KEY) => [BTN_TOOL_RUBBER, BTN_STYLUS].into_iter().for_each(&mut set),
            (Kind::Touch { screen: false, .. }, EV_KEY) => set(BTN_TOUCH),
            (Kind::Touch { screen, .. }, EV_ABS) => {
                for c in [ABS_X, ABS_Y, ABS_MT_SLOT, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_TRACKING_ID, ABS_MT_PRESSURE] {
                    set(c);
                }
                if screen {
                    for c in [ABS_Z, ABS_MT_TOUCH_MAJOR, ABS_MT_TOUCH_MINOR, ABS_MT_ORIENTATION, ABS_MT_TOOL_TYPE] {
                        set(c);
                    }
                }
            }
            // Stock's screens have a lid switch, open: Android's lid state is open, not absent.
            (Kind::Touch { screen: true, .. }, EV_SW) => set(SW_LID),
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
            (Kind::Touch { screen: true, .. }, ABS_X | ABS_Y | ABS_MT_POSITION_X | ABS_MT_POSITION_Y) => (0, SCREEN_ABS_MAX),
            (Kind::Touch { width, .. }, ABS_X | ABS_MT_POSITION_X) => (0, width.saturating_sub(1)),
            (Kind::Touch { height, .. }, ABS_Y | ABS_MT_POSITION_Y) => (0, height.saturating_sub(1)),
            (Kind::Touch { screen: true, .. }, ABS_Z) => (0, 1),
            (Kind::Touch { screen: true, .. }, ABS_MT_TOUCH_MAJOR | ABS_MT_TOUCH_MINOR) => (0, i32::MAX as u32),
            (Kind::Touch { screen: true, .. }, ABS_MT_ORIENTATION) => (0, SCREEN_ORIENTATION_MAX),
            // The pen's slot comes after the fingers' (stock's slot range is 0..=10 too).
            (Kind::Touch { screen: true, .. }, ABS_MT_SLOT) => (0, PEN_SLOT as u32),
            (Kind::Touch { .. }, ABS_MT_SLOT) => (0, MAX_SLOTS - 1),
            (Kind::Touch { screen: true, .. }, ABS_MT_TOOL_TYPE) => (0, MT_TOOL_MAX as u32),
            // The screen's slot count, as stock's: Android 11's and 12's kernels (5.4, 5.10)
            // size the slots from this max, not ABS_MT_SLOT's, so 0xffff there is 65535 slots
            // and an event buffer too big to allocate (no touch screen, #93). Every kernel then
            // sets the axis to 0..=0xffff itself, so the guest sees that either way.
            (Kind::Touch { screen: true, .. }, ABS_MT_TRACKING_ID) => (0, PEN_SLOT as u32 + 1),
            (Kind::Touch { .. }, ABS_MT_TRACKING_ID) => (0, 0xffff),
            (Kind::Touch { screen: true, .. }, ABS_MT_PRESSURE) => (0, SCREEN_PRESSURE_MAX),
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
                Kind::Touch { screen: true, .. } => SCREEN_IDS,
                Kind::DualModeMouse => [BUS_VIRTUAL, 0x0627, 0x0004, 0x0001],
                _ => OTHER_IDS,
            }
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect(),
            CFG_PROP_BITS => match self.kind {
                // Stock's screen has none. Without INPUT_PROP_DIRECT Android would make it a
                // pointer; the image's `virtio_input_multi_touch_1.idc` (`touch.deviceType =
                // touchScreen`, matched by the device name) is what makes it a touch screen.
                Kind::Touch { screen: false, .. } => vec![1 << INPUT_PROP_DIRECT],
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
    /// Event-idx: Linux's driver kicks after refilling the event queue on every interrupt;
    /// under event-idx it kicks only when the device had taken every buffer, which a 64-entry
    /// queue fed one event per buffer rarely is. The status queue is drained with kicks off.
    fn features(&self) -> u64 {
        F_RING_EVENT_IDX
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
        let event_idx = act.features & F_RING_EVENT_IDX != 0;
        let w = Worker::new(act.mem, act.irq, Queue::new(eventq, event_idx), statusq.map(|q| Queue::new(q, event_idx)), self.kind);
        let h = std::thread::Builder::new()
            .name(format!("input-{}", self.name))
            .spawn(move || {
                crate::device_thread();
                w.run(rx)
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
    /// Batches waiting for guest buffers, in order (bounded: the oldest dropped beyond
    /// [`Worker::MAX_PENDING`] events).
    pending: VecDeque<Batch>,
    /// Events in `pending` not yet sent.
    queued: usize,
    touch: TouchState,
    pen: PenState,
    contact: Contact,
}

impl Worker {
    const MAX_PENDING: usize = 4096;

    fn new(mem: Arc<GuestRam>, irq: Arc<Interrupt>, eventq: Queue, statusq: Option<Queue>, kind: Kind) -> Worker {
        let (touch, pen, contact) = (TouchState::default(), PenState::default(), kind.contact());
        Worker { mem, irq, eventq, statusq, pending: VecDeque::new(), queued: 0, touch, pen, contact }
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
                if let Some(b) = self.touch.batch(x, y, down, self.contact) {
                    self.push(b);
                }
            }
            Msg::Pen(s) => {
                if let Some(b) = self.pen.batch(&s, self.contact) {
                    // The pen selects its own slot: the finger's next batch selects slot 0 again.
                    self.touch.raw(&b.events);
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
            b.events = move_events(from, (self.touch.x, self.touch.y), self.contact);
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

    /// Takes the guest's status writes (LEDs), with kicks off while it does.
    fn drain_status(&mut self) {
        let Some(q) = self.statusq.as_mut() else { return };
        let mut used = false;
        let mut c = Chain::default();
        while let Ok(true) = q.pop_draining(&self.mem, &mut c) {
            let _ = q.add_used(&self.mem, c.head, 0);
            used = true;
        }
        if used && q.should_notify(&self.mem) {
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
        // A driver that turned callbacks off (NO_INTERRUPT, or a `used_event` not yet
        // reached) gets no interrupt.
        if used && self.eventq.should_notify(&self.mem) {
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
    fn touchpads_keep_their_units_btn_touch_and_direct() {
        let (mut dev, h) = Input::new(Kind::Touch { width: 1542, height: 297, screen: false }, TOUCHPAD_NAME);
        assert_eq!(cfg(&mut dev, CFG_ID_NAME, 0), TOUCHPAD_NAME.as_bytes());
        assert_eq!(cfg(&mut dev, CFG_PROP_BITS, 0), vec![1 << INPUT_PROP_DIRECT]);
        let abs = cfg(&mut dev, CFG_EV_BITS, EV_ABS as u8);
        assert!(has(&abs, ABS_MT_POSITION_X));
        let info = cfg(&mut dev, CFG_ABS_INFO, ABS_MT_POSITION_Y as u8);
        assert_eq!(u32::from_le_bytes([info[4], info[5], info[6], info[7]]), 296);
        assert!(cfg(&mut dev, CFG_EV_BITS, EV_REL as u8).is_empty());
        assert!(cfg(&mut dev, CFG_EV_BITS, EV_SW as u8).is_empty());
        let keys = cfg(&mut dev, CFG_EV_BITS, EV_KEY as u8);
        assert_eq!((0..1024u16).filter(|&c| has(&keys, c)).collect::<Vec<_>>(), [BTN_TOUCH]);
        // No pen: no tool type, the fingers' ten slots.
        assert!(!has(&abs, ABS_MT_TOOL_TYPE) && !has(&abs, ABS_MT_TOUCH_MAJOR));
        let slot = cfg(&mut dev, CFG_ABS_INFO, ABS_MT_SLOT as u8);
        assert_eq!(u32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]), MAX_SLOTS - 1);
        // Touches go as they come, with BTN_TOUCH.
        h.touch(900, 150, true);
        h.touch(900, 150, false);
        let b = dev.take_queued();
        assert!(b[0].contains(&InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 900)) && b[0].contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 1)));
        assert!(b[1].contains(&InputEvent::new(EV_KEY, BTN_TOUCH, 0)));
    }

    /// Stock 37.2.13's `virtio_input_multi_touch_1` (`getevent -p` on the phone, tablet and
    /// Wear AVDs): the pen's eraser and barrel keys and no `BTN_TOUCH`; axes 0..=32767 whatever
    /// the display, `ABS_Z` 0..=1, touch size 0..=i32::MAX, orientation 0..=90, slots 0..=10,
    /// tool type 0..=15, pressure 0..=1024; `SW_LID`; no input properties; no ids. The tracking
    /// id reads 0..=65535 in the guest on every kernel, which sets it so itself; its config max
    /// is the slot count (11: API 30's kernel takes the slots from it, see
    /// `every_guest_kernel_gets_the_screens_eleven_slots`).
    #[test]
    fn screens_look_like_stocks_virtio_multi_touch() {
        for (w, h) in [(720, 1280), (2560, 1600), (454, 454)] {
            let (mut dev, _h) = Input::new(Kind::Touch { width: w, height: h, screen: true }, "virtio_input_multi_touch_1");
            let set = |bits: Vec<u8>| (0..1024u16).filter(|&c| has(&bits, c)).collect::<Vec<_>>();
            assert_eq!(set(cfg(&mut dev, CFG_EV_BITS, EV_KEY as u8)), [BTN_TOOL_RUBBER, BTN_STYLUS]);
            let max = |info: Vec<u8>| {
                (u32::from_le_bytes([info[0], info[1], info[2], info[3]]), u32::from_le_bytes([info[4], info[5], info[6], info[7]]))
            };
            let stock: [(u16, u32); 12] = [
                (ABS_X, 32767),
                (ABS_Y, 32767),
                (ABS_Z, 1),
                (ABS_MT_SLOT, 10),
                (ABS_MT_TOUCH_MAJOR, i32::MAX as u32),
                (ABS_MT_TOUCH_MINOR, i32::MAX as u32),
                (ABS_MT_ORIENTATION, 90),
                (ABS_MT_POSITION_X, 32767),
                (ABS_MT_POSITION_Y, 32767),
                (ABS_MT_TOOL_TYPE, 15),
                (ABS_MT_TRACKING_ID, 11),
                (ABS_MT_PRESSURE, 1024),
            ];
            assert_eq!(set(cfg(&mut dev, CFG_EV_BITS, EV_ABS as u8)), stock.map(|(c, _)| c));
            for (axis, m) in stock {
                assert_eq!(max(cfg(&mut dev, CFG_ABS_INFO, axis as u8)), (0, m), "{axis:#x}");
            }
            assert_eq!(set(cfg(&mut dev, CFG_EV_BITS, EV_SW as u8)), [SW_LID]);
            assert!(cfg(&mut dev, CFG_EV_BITS, EV_REL as u8).is_empty());
            assert!(cfg(&mut dev, CFG_PROP_BITS, 0).is_empty(), "no INPUT_PROP_DIRECT");
            assert_eq!(cfg(&mut dev, CFG_ID_DEVIDS, 0), [0x06, 0, 0, 0, 0, 0, 0, 0]);
        }
    }

    /// The guest kernels' two ways of sizing a virtio multi-touch device. Android 11's and 12's
    /// (android11-5.4, android12-5.10 `virtinput_cfg_abs`) take the slot count from
    /// `ABS_MT_TRACKING_ID`'s max; android13-5.15 onwards from `ABS_MT_SLOT`'s max + 1. Both
    /// then set the slot axis to 0..=slots-1 and the tracking id to 0..=0xffff (`TRKID_MAX`),
    /// and `input_register_device` sizes its event buffer from the slot count: on the screen,
    /// (slots + 1 + 9 MT axes × slots + 3 other axes + 7 + 2) 8-byte values in one `kcalloc`,
    /// which fails (probe -ENOMEM, no touch screen) beyond 4 MiB (order 10).
    #[derive(Clone, Copy, Debug)]
    enum GuestKernel {
        SlotsFromTrackingId,
        SlotsFromSlotAxis,
    }

    impl GuestKernel {
        /// (slots, the event buffer's bytes) for a device's config.
        fn probe(self, dev: &mut Input) -> (u32, usize) {
            let max = |info: Vec<u8>| u32::from_le_bytes([info[4], info[5], info[6], info[7]]);
            let slots = match self {
                GuestKernel::SlotsFromTrackingId => max(cfg(dev, CFG_ABS_INFO, ABS_MT_TRACKING_ID as u8)),
                GuestKernel::SlotsFromSlotAxis => max(cfg(dev, CFG_ABS_INFO, ABS_MT_SLOT as u8)) + 1,
            };
            let abs = cfg(dev, CFG_EV_BITS, EV_ABS as u8);
            let mt = |c: u16| c == ABS_MT_SLOT || (0x30..=0x3d).contains(&c);
            let axes = (0..64u16).filter(|&c| has(&abs, c));
            let events: usize = slots as usize + 1 + axes.map(|c| if mt(c) { slots as usize } else { 1 }).sum::<usize>() + 7;
            (slots, (events + 2) * 8)
        }
    }

    /// Issue #93: on API 30 the screen's probe failed (`virtio_input: probe of virtio1 failed
    /// with error -12`: 65535 slots from the tracking id's max, a 5 MiB event buffer), so the
    /// guest had no touch screen, its configuration said `notouch` and every accessibility
    /// gesture was cancelled. Stock 37.2.13 on the same API 30 image: slots 0..=10, so its
    /// tracking id max is 11; every kernel gets stock's 11 slots and a small buffer.
    #[test]
    fn every_guest_kernel_gets_the_screens_eleven_slots() {
        for (w, h) in [(720, 1280), (2560, 1600), (454, 454)] {
            let (mut dev, _h) = Input::new(Kind::Touch { width: w, height: h, screen: true }, "virtio_input_multi_touch_1");
            for k in [GuestKernel::SlotsFromTrackingId, GuestKernel::SlotsFromSlotAxis] {
                let (slots, buffer) = k.probe(&mut dev);
                assert_eq!(slots, PEN_SLOT as u32 + 1, "{k:?}: fingers' slots and the pen's");
                assert!(buffer <= 4 << 20, "{k:?}: a {buffer}-byte event buffer fails the probe");
            }
        }
        // The touchpad (display glasses' images, android14 kernels and later): its ten slots.
        let (mut pad, _h) = Input::new(Kind::Touch { width: 1542, height: 297, screen: false }, TOUCHPAD_NAME);
        assert_eq!(GuestKernel::SlotsFromSlotAxis.probe(&mut pad).0, MAX_SLOTS);
    }

    /// Two contacts at once (a finger and the pen, or two fingers sent raw): the sequence is the
    /// same for every API level, and every slot it selects exists on every guest kernel.
    #[test]
    fn two_contacts_select_only_slots_every_guest_kernel_has() {
        let (mut dev, h) = Input::new(Kind::Touch { width: 720, height: 1280, screen: true }, "virtio_input_multi_touch_1");
        let abs = |c: u16, v: i32| InputEvent::new(EV_ABS, c, v);
        h.touch(100, 200, true);
        h.pen(PenSample { x: 300, y: 400, pressure: None, phase: PenPhase::Down, button: false, eraser: false });
        h.send(vec![
            abs(ABS_MT_SLOT, 1),
            abs(ABS_MT_TRACKING_ID, 1),
            abs(ABS_MT_POSITION_X, 9000),
            abs(ABS_MT_PRESSURE, 1024),
            InputEvent::syn(),
        ]);
        h.touch(110, 200, true);
        h.pen(PenSample { x: 300, y: 400, pressure: None, phase: PenPhase::Leave, button: false, eraser: false });
        h.touch(110, 200, false);
        let (x, y) = Kind::Touch { width: 720, height: 1280, screen: true }.axis(100, 200);
        let (px, py) = Kind::Touch { width: 720, height: 1280, screen: true }.axis(300, 400);
        let x2 = Kind::Touch { width: 720, height: 1280, screen: true }.axis(110, 200).0;
        let got = dev.take_queued();
        assert_eq!(
            got,
            [
                vec![
                    abs(ABS_MT_SLOT, 0),
                    abs(ABS_MT_TRACKING_ID, 0),
                    abs(ABS_MT_POSITION_X, x),
                    abs(ABS_MT_POSITION_Y, y),
                    abs(ABS_MT_PRESSURE, FINGER_PRESSURE),
                    abs(ABS_MT_TOUCH_MAJOR, FINGER_SIZE),
                    abs(ABS_MT_TOUCH_MINOR, FINGER_SIZE),
                    InputEvent::syn(),
                ],
                vec![
                    abs(ABS_MT_SLOT, PEN_SLOT),
                    abs(ABS_MT_TRACKING_ID, PEN_SLOT),
                    abs(ABS_MT_POSITION_X, px),
                    abs(ABS_MT_POSITION_Y, py),
                    abs(ABS_MT_TOOL_TYPE, MT_TOOL_PEN),
                    abs(ABS_MT_PRESSURE, PEN_PRESSURE),
                    InputEvent::syn(),
                ],
                vec![
                    abs(ABS_MT_SLOT, 1),
                    abs(ABS_MT_TRACKING_ID, 1),
                    abs(ABS_MT_POSITION_X, 9000),
                    abs(ABS_MT_PRESSURE, 1024),
                    InputEvent::syn()
                ],
                // Raw events and the pen moved the kernel's slot: the finger selects slot 0 again.
                vec![
                    abs(ABS_MT_SLOT, 0),
                    abs(ABS_MT_TRACKING_ID, 0),
                    abs(ABS_MT_POSITION_X, x2),
                    abs(ABS_MT_POSITION_Y, y),
                    abs(ABS_MT_PRESSURE, FINGER_PRESSURE),
                    abs(ABS_MT_TOUCH_MAJOR, FINGER_SIZE),
                    abs(ABS_MT_TOUCH_MINOR, FINGER_SIZE),
                    InputEvent::syn(),
                ],
                vec![abs(ABS_MT_SLOT, PEN_SLOT), abs(ABS_MT_PRESSURE, 0), abs(ABS_MT_TRACKING_ID, -1), InputEvent::syn()],
                vec![abs(ABS_MT_SLOT, 0), abs(ABS_MT_PRESSURE, 0), abs(ABS_MT_TRACKING_ID, -1), InputEvent::syn()],
            ]
        );
        for k in [GuestKernel::SlotsFromTrackingId, GuestKernel::SlotsFromSlotAxis] {
            let slots = k.probe(&mut dev).0 as i32;
            for e in got.iter().flatten().filter(|e| e.typ == EV_ABS && e.code == ABS_MT_SLOT) {
                assert!((0..slots).contains(&e.value), "{k:?}: slot {} of {slots}", e.value);
            }
        }
    }

    /// Android maps a raw `r` on a 0..=32767 axis to `r × size / 32768` pixels: every pixel of
    /// every reference screen comes back as itself (plus under a pixel), in order.
    #[test]
    fn screen_pixels_land_on_the_same_pixel() {
        for size in [297u32, 454, 720, 1080, 1280, 1600, 1840, 2092, 2208, 2400, 2560, 32768] {
            let mut last = -1;
            for p in 0..size as i32 {
                let r = to_axis(p, size);
                assert!(r > last && r <= SCREEN_ABS_MAX as i32, "{p} of {size}: {r}");
                let guest = f64::from(r) * f64::from(size) / 32768.0;
                assert!(guest >= f64::from(p) && guest < f64::from(p) + 1.0, "{p} of {size}: {r} maps to {guest}");
                last = r;
            }
        }
        assert_eq!((to_axis(-5, 720), to_axis(720, 720), to_axis(5000, 720)), (0, 32767, 32767), "off the screen: clamped");
        assert_eq!(to_axis(10, 0), 0);
        let screen = Kind::Touch { width: 2560, height: 1600, screen: true };
        assert_eq!((screen.axis(0, 0), screen.axis(1280, 800), screen.axis(2559, 1599)), ((0, 0), (16384, 16384), (32756, 32748)));
        assert_eq!(Kind::Touch { width: 1542, height: 297, screen: false }.axis(900, 150), (900, 150));
        assert_eq!(Kind::DualModeMouse.axis(7, 8), (7, 8));
    }

    #[test]
    fn screen_touches_scale_and_carry_no_btn_touch() {
        let (dev, h) = Input::new(Kind::Touch { width: 720, height: 1280, screen: true }, "t");
        h.touch(360, 640, true);
        h.touch(361, 640, true);
        h.touch(361, 640, false);
        let abs = |c: u16, v: i32| InputEvent::new(EV_ABS, c, v);
        assert_eq!(
            dev.take_queued(),
            [
                // Stock's mouse down: tracking id (the slot), position, full pressure, touch size.
                vec![
                    abs(ABS_MT_SLOT, 0),
                    abs(ABS_MT_TRACKING_ID, 0),
                    abs(ABS_MT_POSITION_X, 16384),
                    abs(ABS_MT_POSITION_Y, 16384),
                    abs(ABS_MT_PRESSURE, 1024),
                    abs(ABS_MT_TOUCH_MAJOR, 0x500),
                    abs(ABS_MT_TOUCH_MINOR, 0x500),
                    InputEvent::syn()
                ],
                // The position that changed, no single-touch axes; the lift zeroes the pressure.
                vec![abs(ABS_MT_POSITION_X, 16430), InputEvent::syn()],
                vec![abs(ABS_MT_PRESSURE, 0), abs(ABS_MT_TRACKING_ID, -1), InputEvent::syn()],
            ]
        );
    }

    #[test]
    fn a_pen_hovers_touches_and_leaves_on_its_own_slot() {
        use sim_core::stylus::{PenPhase::*, PenSample};
        let (dev, h) = Input::new(Kind::Touch { width: 2560, height: 1600, screen: true }, "t");
        let s = |x, y, phase| PenSample::new(x, y, phase);
        h.pen(s(10, 20, Hover));
        h.pen(s(10, 20, Hover)); // nothing changed: nothing sent
        h.pen(PenSample { pressure: Some(50), ..s(10, 20, Down) });
        h.pen(s(30, 20, Move));
        h.pen(s(30, 20, Up));
        h.pen(s(30, 20, Leave));
        h.pen(s(30, 20, Leave)); // out of range already
        let b = dev.take_queued();
        let abs = |c: u16, v: i32| InputEvent::new(EV_ABS, c, v);
        let slot = abs(ABS_MT_SLOT, PEN_SLOT);
        // Pixels on stock's axes (10 of 2560 is 128, 20 of 1600 is 410, 30 is 384); the
        // console's pressure 50 of 255 is 201 of 1024; contact is pressure alone.
        assert_eq!(
            b,
            [
                vec![
                    slot,
                    abs(ABS_MT_TRACKING_ID, PEN_SLOT),
                    abs(ABS_MT_POSITION_X, 128),
                    abs(ABS_MT_POSITION_Y, 410),
                    abs(ABS_MT_TOOL_TYPE, MT_TOOL_PEN),
                    abs(ABS_MT_PRESSURE, 0),
                    InputEvent::syn()
                ],
                vec![slot, abs(ABS_MT_PRESSURE, 201), InputEvent::syn()],
                vec![slot, abs(ABS_MT_POSITION_X, 384), InputEvent::syn()],
                vec![slot, abs(ABS_MT_PRESSURE, 0), InputEvent::syn()],
                vec![slot, abs(ABS_MT_PRESSURE, 0), abs(ABS_MT_TRACKING_ID, -1), InputEvent::syn()],
            ]
        );
        // A press without a pressure has stock's raw default.
        let (dev, h) = Input::new(Kind::Touch { width: 2560, height: 1600, screen: true }, "t");
        h.pen(s(10, 20, Down));
        assert!(dev.take_queued()[0].contains(&abs(ABS_MT_PRESSURE, PEN_PRESSURE)));
    }

    #[test]
    fn the_eraser_end_is_a_new_contact_and_fingers_reselect_their_slot() {
        use sim_core::stylus::{PenPhase::*, PenSample};
        let (dev, h) = Input::new(Kind::Touch { width: 2560, height: 1600, screen: true }, "t");
        h.pen(PenSample { button: true, ..PenSample::new(5, 5, Down) });
        h.pen(PenSample { eraser: true, ..PenSample::new(5, 5, Down) });
        h.pen(PenSample { eraser: true, ..PenSample::new(5, 5, Leave) });
        h.touch(100, 100, true);
        let b = dev.take_queued();
        assert!(
            b[0].contains(&InputEvent::new(EV_KEY, BTN_STYLUS, 1))
                && b[0].contains(&InputEvent::new(EV_ABS, ABS_MT_PRESSURE, PEN_PRESSURE))
        );
        // The pen end lifts and leaves (button up, contact ended), then the eraser comes in.
        let leave = b[1].iter().position(|e| *e == InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1)).expect("leave");
        assert!(b[1][..leave].contains(&InputEvent::new(EV_KEY, BTN_STYLUS, 0)));
        assert!(b[1][leave..].contains(&InputEvent::new(EV_ABS, ABS_MT_TOOL_TYPE, MT_TOOL_MAX)));
        assert!(b[1][leave..].contains(&InputEvent::new(EV_KEY, BTN_TOOL_RUBBER, 1)));
        assert!(b[2].contains(&InputEvent::new(EV_KEY, BTN_TOOL_RUBBER, 0)));
        // The finger after the pen selects slot 0 again.
        assert_eq!(b[3][0], InputEvent::new(EV_ABS, ABS_MT_SLOT, 0));
    }

    /// Without `BTN_TOUCH` each contact is its own slot's: a pen lifting (to hover) or leaving
    /// beside a held finger, or a finger lifting under a touching pen, sends nothing for the
    /// other contact.
    #[test]
    fn pen_and_finger_contacts_are_their_own_slots() {
        use sim_core::stylus::{PenPhase::*, PenSample};
        let (dev, h) = Input::new(Kind::Touch { width: 2560, height: 1600, screen: true }, "t");
        h.touch(100, 100, true);
        h.pen(PenSample::new(500, 500, Down));
        h.pen(PenSample::new(500, 500, Up));
        h.pen(PenSample::new(500, 500, Down));
        h.pen(PenSample::new(500, 500, Leave));
        h.touch(100, 100, false);
        let b = dev.take_queued();
        assert_eq!(b.len(), 6, "{b:?}");
        assert!(b.iter().flatten().all(|e| e.code != BTN_TOUCH || e.typ != EV_KEY), "{b:?}");
        assert_eq!(
            b[2],
            [InputEvent::new(EV_ABS, ABS_MT_SLOT, PEN_SLOT), InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0), InputEvent::syn()],
            "the pen hovers by its pressure"
        );
        let abs = |c: u16, v: i32| InputEvent::new(EV_ABS, c, v);
        assert_eq!(b[5], [abs(ABS_MT_SLOT, 0), abs(ABS_MT_PRESSURE, 0), abs(ABS_MT_TRACKING_ID, -1), InputEvent::syn()]);
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
        let mut w = Worker::new(mem.clone(), Arc::new(Interrupt::new(line.clone())), Queue::new(drv.cfg, false), None, Kind::Keyboard);
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

    /// A touch worker over a 64-entry event queue (the device's size) with `bufs` buffers posted:
    /// the touchpad's contacts (pixels, `BTN_TOUCH`), or with [`Rig::screen`] the screen's.
    struct Rig {
        mem: Arc<GuestRam>,
        drv: Driver,
        irq: Arc<Interrupt>,
        raises: Arc<Raises>,
        w: Worker,
        posted: u64,
        read: u16,
        event_idx: bool,
        kicks: u32,
    }

    impl Rig {
        fn new(bufs: u64) -> Rig {
            Rig::with(bufs, false)
        }

        /// A rig whose queue runs with event-idx (`event_idx`) or the flags.
        fn with(bufs: u64, event_idx: bool) -> Rig {
            Rig::of(bufs, event_idx, Kind::Touch { width: 1080, height: 2400, screen: false })
        }

        fn screen(bufs: u64) -> Rig {
            Rig::of(bufs, false, Kind::Touch { width: 720, height: 1280, screen: true })
        }

        fn of(bufs: u64, event_idx: bool, kind: Kind) -> Rig {
            let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
            let raises = Arc::new(Raises::default());
            let irq = Arc::new(Interrupt::new(raises.clone()));
            let drv = Driver::new(64);
            let w = Worker::new(mem.clone(), irq.clone(), Queue::new(drv.cfg, event_idx), None, kind);
            let mut rig = Rig { mem, drv, irq, raises, w, posted: 0, read: 0, event_idx, kicks: 0 };
            rig.post(bufs);
            rig
        }

        /// The guest posts `n` more one-event buffers (descriptors recycled in order), then
        /// kicks if the device asked for it (Linux's `virtqueue_kick_prepare`); returns whether.
        fn post(&mut self, n: u64) -> bool {
            let old = self.drv.avail_idx();
            for _ in 0..n {
                self.drv.push(&self.mem, &[(BUFS + (self.posted % 64) * 8, 8, true)]);
                self.posted += 1;
            }
            let kick = n > 0 && self.drv.needs_kick(&self.mem, old, self.event_idx);
            self.kicks += u32::from(kick);
            kick
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
            // As Linux's `virtqueue_get_buf`: interrupt at the next used entry.
            self.drv.enable_cb(&self.mem, self.read, self.event_idx);
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

    /// The guest refills the event queue after every interrupt, as Linux's driver does. With
    /// event-idx a device that never ran out of buffers asks for no kick, so a drag costs the
    /// guest no exits; with the flags every refill kicks. Interrupts are one per flush either way.
    #[test]
    fn refills_cost_no_kicks_under_event_idx() {
        let (dev, _h) = Input::new(Kind::Touch { width: 1080, height: 2400, screen: false }, "t");
        assert!(dev.features() & F_RING_EVENT_IDX != 0, "the device offers event-idx");
        for (event_idx, want) in [(true, 0), (false, 100)] {
            let mut rig = Rig::with(64, event_idx);
            rig.send([touch(10, 20, true)]);
            let n = rig.take().len() as u64;
            rig.post(n);
            let kicks = rig.kicks;
            for i in 0..100 {
                rig.send([touch(11 + i, 21 + i, true)]);
                let n = rig.take().len() as u64;
                rig.post(n);
            }
            assert_eq!(rig.kicks - kicks, want, "event_idx={event_idx}");
            assert_eq!(rig.raised(), 101, "event_idx={event_idx}: an interrupt per move");
        }
    }

    /// A device that ran out of buffers mid-batch asks for the refill: the guest's next post
    /// kicks (the lost-wakeup case for a queue the device fills), and the rest of the batch goes.
    #[test]
    fn a_device_out_of_buffers_asks_for_the_refill() {
        for event_idx in [true, false] {
            let mut rig = Rig::with(2, event_idx);
            rig.send([touch(10, 20, true)]);
            let first = rig.take();
            assert_eq!(first.len(), 2);
            assert!(rig.post(1), "event_idx={event_idx}: the refill kicks");
            rig.w.flush();
            assert_eq!(rig.take().len(), 1);
            assert!(rig.post(64), "event_idx={event_idx}: out again, so the refill kicks again");
            rig.w.flush();
            assert_eq!(rig.take().len(), 6, "the rest of the down");
            assert!(!rig.post(6) || !event_idx, "buffers to spare: no kick under event-idx");
        }
    }

    /// Interrupts follow the driver's suppression: none while it has callbacks off.
    #[test]
    fn no_interrupt_while_the_driver_has_callbacks_off() {
        let mut rig = Rig::with(64, false);
        rig.mem.write_u16(rig.drv.cfg.avail, 1).expect("NO_INTERRUPT");
        rig.send([touch(10, 20, true)]);
        assert_eq!((rig.raised(), rig.drv.used_idx(&rig.mem)), (0, 9), "delivered, not raised");
        rig.mem.write_u16(rig.drv.cfg.avail, 0).expect("callbacks on");
        rig.send([touch(11, 21, true)]);
        assert_eq!(rig.raised(), 1);
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

    /// The screen's moves collapse the same way, into its two-axis move.
    #[test]
    fn screen_moves_the_guest_has_not_taken_collapse_too() {
        let mut rig = Rig::screen(0);
        rig.send([touch(0, 0, true)]);
        for i in 1..=100 {
            rig.send([touch(i, 2 * i, true)]);
        }
        assert_eq!((rig.w.pending.len(), rig.w.queued), (2, 8 + 3));
        rig.post(64);
        rig.send([Msg::Kick, touch(100, 200, false)]);
        let ev = rig.take();
        assert_eq!(
            ev[8..11],
            [InputEvent::new(EV_ABS, ABS_MT_POSITION_X, 100), InputEvent::new(EV_ABS, ABS_MT_POSITION_Y, 200), InputEvent::syn()]
        );
        assert_eq!(
            ev[11..],
            [InputEvent::new(EV_ABS, ABS_MT_PRESSURE, 0), InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, -1), InputEvent::syn()]
        );
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
        let (mut dev, h) = Input::new(Kind::Touch { width: 100, height: 100, screen: false }, "touch");
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
            // The last touch is in once its x is and its batch has ended (events land one buffer
            // at a time, so stopping at the x alone could miss its y and SYN still on the way).
            if finished
                && keys == (THREADS as usize) * KEYS as usize
                && got.iter().rev().find(|e| e.code == ABS_MT_POSITION_X).is_some_and(|e| e.value == 7)
                && got.last() == Some(&InputEvent::syn())
            {
                break;
            }
            finished = finished || senders.iter().all(|s| s.is_finished());
            assert!(
                start.elapsed() < std::time::Duration::from_secs(20),
                "stalled with {} events, about 2000 expected ({keys} of {} keys; senders done: {finished})",
                got.len(),
                THREADS as usize * KEYS as usize,
            );
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
