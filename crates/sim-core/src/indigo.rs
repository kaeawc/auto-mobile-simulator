//! The simulator HID channel's messages (Indigo), where simee writes or changes one itself because
//! SimulatorKit has no builder that makes it: the iPad pointer's motion and buttons, and an Apple
//! Pencil contact. Layouts are SimulatorKit's (Xcode 26.6 and 27.x) and the guest's
//! `SimulatorHID.framework`, which turns each event into an `IOHIDEvent`; every patch checks the
//! fields around it first and changes nothing in a message it doesn't recognize.
//!
//! A message is a 0x20-byte header, then `count` events of [`EVENT_SIZE`] bytes each. An event is
//! its type, a timestamp, and a union of per-type fields from offset 0x10.

/// Each event's size, which the header also records at [`STRIDE_OFFSET`].
pub const EVENT_SIZE: usize = 0xa0;
/// Where the first event starts.
pub const HEADER_SIZE: usize = 0x20;
const STRIDE_OFFSET: usize = 0x18;
const COUNT_OFFSET: usize = 0x1c;

/// Event types.
const TOUCH: u32 = 0xb;
const POINTER: u32 = 0x11;
/// A button event inside a pointer message (the guest makes an `IOHIDEventCreateButtonEvent`).
const POINTER_BUTTON: u32 = 0x7a41;
/// A message that creates or removes a HID service in the guest.
const SERVICE: u32 = 0x7fff_0001;

/// Within an event: the timestamp, and the flag byte whose low bit says it is in nanoseconds
/// (the guest converts it to its own clock; SimulatorKit's builders always set it).
const TIMESTAMP: usize = 0x4;
const FLAGS: usize = 0x91;
/// Within a service message's event: the target id that the service's events are sent to.
const SERVICE_TARGET: usize = 0x20;
/// Within a touch event: the transducer type (`IOHIDDigitizerTransducerType`), which the guest sets
/// on the finger event it creates.
const TRANSDUCER: usize = 0x54;
const FINGER: u32 = 2;
const STYLUS: u32 = 0;

/// The pointer's buttons: the bit each sets in the button mask, and its HID button number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
}

impl PointerButton {
    pub const NAMES: &'static str = "primary, secondary";

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "primary" | "left" => Self::Primary,
            "secondary" | "right" => Self::Secondary,
            _ => return None,
        })
    }

    pub fn mask(self) -> u32 {
        match self {
            Self::Primary => 1,
            Self::Secondary => 2,
        }
    }

    pub fn number(self) -> u32 {
        self.mask()
    }
}

fn put_u32(m: &mut [u8], at: usize, v: u32) {
    m[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_f64(m: &mut [u8], at: usize, v: f64) {
    m[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(m: &[u8], at: usize) -> Option<u32> {
    m.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// An empty message of `count` events, each of `kind`, stamped `now_ns` (the host's
/// `CLOCK_UPTIME_RAW` in nanoseconds).
fn message(kinds: &[u32], now_ns: u64) -> Vec<u8> {
    let mut m = vec![0; HEADER_SIZE + kinds.len() * EVENT_SIZE];
    put_u32(&mut m, STRIDE_OFFSET, EVENT_SIZE as u32);
    m[COUNT_OFFSET] = kinds.len() as u8;
    for (i, kind) in kinds.iter().enumerate() {
        let e = HEADER_SIZE + i * EVENT_SIZE;
        put_u32(&mut m, e, *kind);
        m[e + TIMESTAMP..e + TIMESTAMP + 8].copy_from_slice(&now_ns.to_le_bytes());
        m[e + FLAGS] |= 1;
    }
    m
}

/// The pointer moving by `(dx, dy)` points with `buttons` (a mask of [`PointerButton::mask`])
/// held, sent to the pointer service `target`: the guest makes a relative pointer event of it.
pub fn pointer_motion(dx: f64, dy: f64, buttons: u32, target: u32, now_ns: u64) -> Vec<u8> {
    let mut m = message(&[POINTER], now_ns);
    let u = HEADER_SIZE + 0x10;
    put_f64(&mut m, u, dx);
    put_f64(&mut m, u + 0x8, dy);
    put_u32(&mut m, u + 0x18, buttons);
    put_u32(&mut m, u + 0x1c, target);
    m
}

/// A pointer button pressed or released: a pointer event that doesn't move, with `buttons` (the
/// mask after the change), and the button event as its child, which is what makes it a click.
pub fn pointer_button(button: PointerButton, pressed: bool, buttons: u32, target: u32, now_ns: u64) -> Vec<u8> {
    let mut m = message(&[POINTER, POINTER_BUTTON], now_ns);
    let u = HEADER_SIZE + 0x10;
    put_u32(&mut m, u + 0x18, buttons);
    put_u32(&mut m, u + 0x1c, target);
    let b = HEADER_SIZE + EVENT_SIZE + 0x10;
    put_u32(&mut m, b, buttons);
    put_u32(&mut m, b + 0x4, button.number());
    put_u32(&mut m, b + 0x8, u32::from(pressed));
    put_u32(&mut m, b + 0xc, target);
    m
}

/// The target id a service-creating message (`IndigoHIDMessageToCreateMouseService`'s) gives its
/// service, when the message has the layout this module writes; else `None`.
pub fn service_target(m: &[u8]) -> Option<u32> {
    let known = m.len() >= HEADER_SIZE + EVENT_SIZE
        && get_u32(m, STRIDE_OFFSET) == Some(EVENT_SIZE as u32)
        && m[COUNT_OFFSET] == 1
        && get_u32(m, HEADER_SIZE) == Some(SERVICE);
    known.then(|| get_u32(m, HEADER_SIZE + SERVICE_TARGET)).flatten().filter(|t| *t != 0)
}

/// Makes a one-finger touch message (`IndigoHIDMessageForMouseNSEvent`'s: a hand event and its
/// finger) an Apple Pencil's: the finger's transducer becomes a stylus, which UIKit reports as a
/// `UITouch` of type `.pencil`. `false`, leaving it unchanged, for a message it doesn't recognize.
pub fn make_pencil(m: &mut [u8]) -> bool {
    let child = HEADER_SIZE + EVENT_SIZE;
    let known = m.len() >= HEADER_SIZE + 2 * EVENT_SIZE
        && get_u32(m, STRIDE_OFFSET) == Some(EVENT_SIZE as u32)
        && m[COUNT_OFFSET] == 2
        && get_u32(m, HEADER_SIZE) == Some(TOUCH)
        && get_u32(m, child) == Some(TOUCH)
        && get_u32(m, child + TRANSDUCER) == Some(FINGER);
    if known {
        put_u32(m, child + TRANSDUCER, STYLUS);
    }
    known
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f64_at(m: &[u8], at: usize) -> f64 {
        f64::from_le_bytes(m[at..at + 8].try_into().expect("8 bytes"))
    }

    #[test]
    fn motion_is_one_pointer_event() {
        let m = pointer_motion(12.5, -3.0, 1, 0x36, 0x0102_0304_0506_0708);
        assert_eq!(m.len(), 0xc0);
        assert_eq!((get_u32(&m, 0x18), m[0x1c], get_u32(&m, 0x20)), (Some(0xa0), 1, Some(0x11)));
        assert_eq!(&m[0x24..0x2c], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(m[0xb1] & 1, 1, "timestamp in nanoseconds");
        assert_eq!((f64_at(&m, 0x30), f64_at(&m, 0x38), f64_at(&m, 0x40)), (12.5, -3.0, 0.0));
        assert_eq!((get_u32(&m, 0x48), get_u32(&m, 0x4c)), (Some(1), Some(0x36)));
    }

    #[test]
    fn a_click_is_a_pointer_event_with_a_button_child() {
        let m = pointer_button(PointerButton::Secondary, true, 2, 0x36, 7);
        assert_eq!((m.len(), m[0x1c]), (0x160, 2));
        assert_eq!((get_u32(&m, 0x20), get_u32(&m, 0x48), get_u32(&m, 0x4c)), (Some(0x11), Some(2), Some(0x36)));
        assert_eq!(get_u32(&m, 0xc0), Some(0x7a41));
        assert_eq!(
            [0xd0, 0xd4, 0xd8, 0xdc].map(|o| get_u32(&m, o)),
            [Some(2), Some(2), Some(1), Some(0x36)],
            "mask, number, pressed, target"
        );
        assert_eq!(m[0x151] & 1, 1);
        let up = pointer_button(PointerButton::Primary, false, 0, 0x36, 7);
        assert_eq!([0xd0, 0xd4, 0xd8].map(|o| get_u32(&up, o)), [Some(0), Some(1), Some(0)]);
    }

    /// `IndigoHIDMessageToCreateMouseService`'s message as Xcode 26.6 and 27.2 build it.
    fn mouse_service() -> Vec<u8> {
        let mut m = vec![0; 0xc0];
        put_u32(&mut m, 0x18, 0xa0);
        m[0x1c] = 1;
        put_u32(&mut m, 0x20, 0x7fff_0001);
        put_u32(&mut m, 0x30, 5);
        put_u32(&mut m, 0x40, 0x36);
        m
    }

    #[test]
    fn the_service_target_is_read_only_from_a_known_layout() {
        assert_eq!(service_target(&mouse_service()), Some(0x36));
        let mut other = mouse_service();
        put_u32(&mut other, 0x20, 0xb);
        assert_eq!(service_target(&other), None, "not a service message");
        assert_eq!(service_target(&mouse_service()[..0x40]), None, "too short");
        let mut stride = mouse_service();
        put_u32(&mut stride, 0x18, 0xb0);
        assert_eq!(service_target(&stride), None, "another event size");
    }

    /// A one-finger touch as `IndigoHIDMessageForMouseNSEvent` builds it (the fields checked).
    fn touch() -> Vec<u8> {
        let mut m = vec![0; 0x160];
        put_u32(&mut m, 0x18, 0xa0);
        m[0x1c] = 2;
        for e in [0x20, 0xc0] {
            put_u32(&mut m, e, 0xb);
            put_u32(&mut m, e + 0x54, 2);
        }
        m
    }

    #[test]
    fn pencil_turns_the_finger_into_a_stylus() {
        let mut m = touch();
        assert!(make_pencil(&mut m));
        assert_eq!((get_u32(&m, 0x114), get_u32(&m, 0x74)), (Some(0), Some(2)), "the finger, not the hand");
        assert!(!make_pencil(&mut m), "already a stylus: not a message it knows");
        let mut two_fingers = touch();
        two_fingers[0x1c] = 3;
        assert!(!make_pencil(&mut two_fingers));
        assert_eq!(get_u32(&two_fingers, 0x114), Some(2), "left unchanged");
        assert!(!make_pencil(&mut touch()[..0x100].to_vec()));
    }

    #[test]
    fn buttons_parse() {
        assert_eq!(PointerButton::parse("right"), Some(PointerButton::Secondary));
        assert_eq!(PointerButton::parse("primary").map(PointerButton::mask), Some(1));
        assert_eq!(PointerButton::parse("middle"), None);
    }
}
