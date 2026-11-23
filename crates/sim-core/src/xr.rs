//! Android XR controls: head pose, the surrounding environment, the input mode, hand and eye
//! pointers, hand gestures and the pointer.
//!
//! The stock emulator drives the guest's XR service over the `qemud:xr_service` pipe: qemud
//! framing around one `xr_emulator_proto.EmulatorRequest` per message (`xr_emulator_conn.proto`
//! in the emulator's sources), which the guest may answer with an `EmulatorResponse`. Stock
//! reaches it only through gRPC; emu-rs adds an `xr` console command, whose words
//! [`XrCommand::parse`] reads and [`XrCommand::console`] writes. The messages are proto2 and tiny,
//! so they are encoded by hand: every field that is set is written, defaults included.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How the guest takes pointing input (`InputMode`: 1 mouse and keyboard, 2 hand raycast, 3 eye
/// tracking).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XrInputMode {
    Mouse,
    Hand,
    Eye,
}

/// The artificial room shown behind apps (`XrOptions.Environment`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XrEnvironment {
    Day,
    Night,
}

/// What dragging the view does (`ViewportControlMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XrViewport {
    Pan,
    Zoom,
    Rotate,
}

/// A pointer event on the virtio dual-mode mouse.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XrPointer {
    Move,
    Down,
    Up,
    /// Down then up.
    #[default]
    Click,
}

/// Whether a hand or eye pointer event aims (buttons 0: hover, or release) or selects (buttons 1:
/// a pinch, or a gaze click), as Android Studio's embedded emulator sends them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XrPress {
    #[default]
    Aim,
    Press,
}

/// The stock emulator's simulated hand gestures (`HandGesture`, its toolbar's "hands in space").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum XrHandGesture {
    LeftPinch,
    LeftGrab,
    LeftPoke,
    RightPinch,
    RightGrab,
    RightPoke,
}

impl XrHandGesture {
    const ALL: [XrHandGesture; 6] = [Self::LeftPinch, Self::LeftGrab, Self::LeftPoke, Self::RightPinch, Self::RightGrab, Self::RightPoke];

    fn name(self) -> &'static str {
        match self {
            Self::LeftPinch => "left-pinch",
            Self::LeftGrab => "left-grab",
            Self::LeftPoke => "left-poke",
            Self::RightPinch => "right-pinch",
            Self::RightGrab => "right-grab",
            Self::RightPoke => "right-poke",
        }
    }
}

/// One XR control. Serialized with an `action` tag (`{"action": "rotate", "x": 0, ...}`), the
/// shape the API's `xr` method takes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum XrCommand {
    /// Put the content back in front of the user.
    Recenter,
    /// Turn the head by these angles in radians around x, y and z, relative to where it faces.
    Rotate {
        x: f32,
        y: f32,
        z: f32,
    },
    /// Move the head by these distances in metres.
    Move {
        x: f32,
        y: f32,
        z: f32,
    },
    /// The room (day or night) and how much of it shows through: 0 none, 1 fully.
    Options {
        environment: XrEnvironment,
        passthrough: f32,
    },
    Input {
        mode: XrInputMode,
    },
    Viewport {
        mode: XrViewport,
    },
    /// The dual-mode mouse at device pixels (left button for down, up and click).
    Pointer {
        x: u32,
        y: u32,
        #[serde(default)]
        phase: XrPointer,
    },
    /// The hand ray at display pixels: aim it, or pinch (`xr_hand_event`, what stock's gRPC
    /// `streamInputEvent` forwards to the guest).
    Hand {
        x: u32,
        y: u32,
        #[serde(default)]
        press: XrPress,
    },
    /// Eye gaze at display pixels: look, or select (`xr_eye_event`).
    Eye {
        x: u32,
        y: u32,
        #[serde(default)]
        press: XrPress,
    },
    /// A simulated hand gesture.
    Gesture {
        gesture: XrHandGesture,
    },
}

/// The `xr` console command's syntax.
pub const USAGE: &str = "xr recenter | rotate <x> <y> <z> (radians) | move <dx> <dy> <dz> (metres) | \
                         options day|night <passthrough 0-1> | input mouse|hand|eye | viewport pan|zoom|rotate | \
                         pointer <x> <y> [move|down|up|click] | hand|eye <x> <y> [aim|press] | \
                         gesture left-pinch|left-grab|left-poke|right-pinch|right-grab|right-poke";

impl XrCommand {
    /// Parses the words after `xr` on the console (`rotate 0 0.5 0`).
    pub fn parse(words: &[&str]) -> Result<XrCommand, String> {
        let num = |w: &str| w.parse::<f32>().ok().filter(|v| v.is_finite()).ok_or_else(|| format!("`{w}` is not a number"));
        let px = |w: &str| w.parse::<u32>().map_err(|_| format!("`{w}` is not a pixel coordinate"));
        Ok(match words {
            ["recenter"] => XrCommand::Recenter,
            ["rotate", x, y, z] => XrCommand::Rotate { x: num(x)?, y: num(y)?, z: num(z)? },
            ["move", x, y, z] => XrCommand::Move { x: num(x)?, y: num(y)?, z: num(z)? },
            ["options", environment, passthrough] => {
                let environment = match *environment {
                    "day" => XrEnvironment::Day,
                    "night" => XrEnvironment::Night,
                    other => return Err(format!("unknown environment `{other}` (day, night)")),
                };
                let passthrough = num(passthrough)?;
                if !(0.0..=1.0).contains(&passthrough) {
                    return Err(format!("passthrough {passthrough} is outside 0-1"));
                }
                XrCommand::Options { environment, passthrough }
            }
            ["input", mode] => XrCommand::Input {
                mode: match *mode {
                    "mouse" | "keyboard" => XrInputMode::Mouse,
                    "hand" | "hands" => XrInputMode::Hand,
                    "eye" | "eyes" | "gaze" => XrInputMode::Eye,
                    other => return Err(format!("unknown input mode `{other}` (mouse, hand, eye)")),
                },
            },
            ["viewport", mode] => XrCommand::Viewport {
                mode: match *mode {
                    "pan" => XrViewport::Pan,
                    "zoom" => XrViewport::Zoom,
                    "rotate" => XrViewport::Rotate,
                    other => return Err(format!("unknown viewport mode `{other}` (pan, zoom, rotate)")),
                },
            },
            ["pointer", x, y, rest @ ..] if rest.len() <= 1 => XrCommand::Pointer {
                x: px(x)?,
                y: px(y)?,
                phase: match rest.first().copied().unwrap_or("click") {
                    "move" => XrPointer::Move,
                    "down" => XrPointer::Down,
                    "up" => XrPointer::Up,
                    "click" => XrPointer::Click,
                    other => return Err(format!("unknown pointer phase `{other}` (move, down, up, click)")),
                },
            },
            [which @ ("hand" | "eye"), x, y, rest @ ..] if rest.len() <= 1 => {
                let (x, y) = (px(x)?, px(y)?);
                let press = match rest.first().copied().unwrap_or("aim") {
                    "aim" | "hover" | "up" => XrPress::Aim,
                    "press" | "pinch" | "select" | "down" => XrPress::Press,
                    other => return Err(format!("unknown {which} action `{other}` (aim, press)")),
                };
                if *which == "hand" { XrCommand::Hand { x, y, press } } else { XrCommand::Eye { x, y, press } }
            }
            ["gesture", name] => XrCommand::Gesture {
                gesture: XrHandGesture::ALL.into_iter().find(|g| g.name() == *name).ok_or_else(|| {
                    let names: Vec<&str> = XrHandGesture::ALL.iter().map(|g| g.name()).collect();
                    format!("unknown gesture `{name}` ({})", names.join(", "))
                })?,
            },
            _ => return Err(format!("usage: {USAGE}")),
        })
    }

    /// The console command line (`xr rotate 0 0.5 0`).
    pub fn console(&self) -> String {
        format!("xr {self}")
    }

    /// The `EmulatorRequest` for the guest's XR service; `None` for the pointer, which goes to the
    /// dual-mode mouse instead.
    pub fn request(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let vec3 = |x: f32, y: f32, z: f32| {
            let mut m = Vec::new();
            for (field, v) in [(1, x), (2, y), (3, z)] {
                put_f32(&mut m, field, v);
            }
            m
        };
        match *self {
            XrCommand::Recenter => put_varint_field(&mut out, 1, MSG_RECENTER),
            XrCommand::Rotate { x, y, z } => {
                put_varint_field(&mut out, 1, MSG_HEAD_ROTATION);
                put_bytes(&mut out, 5, &vec3(x, y, z));
            }
            XrCommand::Move { x, y, z } => {
                put_varint_field(&mut out, 1, MSG_HEAD_MOVEMENT);
                put_bytes(&mut out, 6, &vec3(x, y, z));
            }
            XrCommand::Options { environment, passthrough } => {
                put_varint_field(&mut out, 1, MSG_SET_OPTIONS);
                let mut options = Vec::new();
                put_varint_field(&mut options, 1, environment as u64);
                put_f32(&mut options, 2, passthrough);
                put_bytes(&mut out, 9, &options);
            }
            XrCommand::Input { mode } => {
                put_varint_field(&mut out, 1, MSG_SET_INPUT_MODE);
                put_varint_field(&mut out, 3, mode as u64 + 1);
            }
            XrCommand::Viewport { mode } => {
                put_varint_field(&mut out, 1, MSG_SET_VIEWPORT_CONTROL);
                put_varint_field(&mut out, 4, mode as u64 + 1);
            }
            XrCommand::Hand { x, y, press } | XrCommand::Eye { x, y, press } => {
                let (msg, field) = if matches!(self, XrCommand::Hand { .. }) { (MSG_HAND_EVENT, 11) } else { (MSG_EYE_EVENT, 12) };
                put_varint_field(&mut out, 1, msg);
                // MouseEvent: every field is required (x, y, buttons, display 0: the main display).
                let mut event = Vec::new();
                put_varint_field(&mut event, 1, u64::from(x));
                put_varint_field(&mut event, 2, u64::from(y));
                put_varint_field(&mut event, 3, press as u64);
                put_varint_field(&mut event, 4, 0);
                put_bytes(&mut out, field, &event);
            }
            XrCommand::Gesture { gesture } => {
                put_varint_field(&mut out, 1, MSG_HAND_GESTURE);
                put_varint_field(&mut out, 10, gesture as u64 + 1);
            }
            XrCommand::Pointer { .. } => return None,
        }
        Some(out)
    }
}

impl fmt::Display for XrCommand {
    /// The console words after `xr`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XrCommand::Recenter => write!(f, "recenter"),
            XrCommand::Rotate { x, y, z } => write!(f, "rotate {x} {y} {z}"),
            XrCommand::Move { x, y, z } => write!(f, "move {x} {y} {z}"),
            XrCommand::Options { environment, passthrough } => write!(f, "options {} {passthrough}", serde_name(environment)),
            XrCommand::Input { mode } => write!(f, "input {}", serde_name(mode)),
            XrCommand::Viewport { mode } => write!(f, "viewport {}", serde_name(mode)),
            XrCommand::Pointer { x, y, phase } => write!(f, "pointer {x} {y} {}", serde_name(phase)),
            XrCommand::Hand { x, y, press } => write!(f, "hand {x} {y} {}", serde_name(press)),
            XrCommand::Eye { x, y, press } => write!(f, "eye {x} {y} {}", serde_name(press)),
            XrCommand::Gesture { gesture } => write!(f, "gesture {}", gesture.name()),
        }
    }
}

/// A unit enum's serialized name (`day`, `hand`, `click`).
fn serde_name(value: &impl fmt::Debug) -> String {
    format!("{value:?}").to_ascii_lowercase()
}

// `MsgType` values.
const MSG_SET_INPUT_MODE: u64 = 2;
const MSG_RECENTER: u64 = 3;
const MSG_SET_VIEWPORT_CONTROL: u64 = 4;
const MSG_HEAD_ROTATION: u64 = 5;
const MSG_HEAD_MOVEMENT: u64 = 6;
const MSG_SET_OPTIONS: u64 = 9;
const MSG_HAND_GESTURE: u64 = 11;
const MSG_HAND_EVENT: u64 = 12;
const MSG_EYE_EVENT: u64 = 13;

const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_LEN: u64 = 2;
const WIRE_FIXED32: u64 = 5;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_varint_field(out: &mut Vec<u8>, field: u64, v: u64) {
    put_varint(out, field << 3 | WIRE_VARINT);
    put_varint(out, v);
}

fn put_f32(out: &mut Vec<u8>, field: u64, v: f32) {
    put_varint(out, field << 3 | WIRE_FIXED32);
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, field << 3 | WIRE_LEN);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// What the guest's XR service answered (`EmulatorResponse`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum XrResponse {
    /// `Status`: 1 OK, 2 unimplemented command, 3 invalid property.
    Status(u64),
    /// The current options; either may be missing.
    Options { environment: Option<XrEnvironment>, passthrough: Option<f32> },
}

impl XrResponse {
    /// Decodes an `EmulatorResponse`; `None` if it isn't one.
    pub fn decode(bytes: &[u8]) -> Option<XrResponse> {
        let mut out = None;
        for (field, value) in fields(bytes)? {
            out = match (field, value) {
                (1, Value::Varint(s)) => Some(XrResponse::Status(s)),
                (2, Value::Bytes(m)) => {
                    let (mut environment, mut passthrough) = (None, None);
                    for (field, value) in fields(m)? {
                        match (field, value) {
                            (1, Value::Varint(0)) => environment = Some(XrEnvironment::Day),
                            (1, Value::Varint(1)) => environment = Some(XrEnvironment::Night),
                            (2, Value::Fixed32(bits)) => passthrough = Some(f32::from_bits(bits)),
                            _ => {}
                        }
                    }
                    Some(XrResponse::Options { environment, passthrough })
                }
                _ => out,
            };
        }
        out
    }
}

impl fmt::Display for XrResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XrResponse::Status(1) => write!(f, "status ok"),
            XrResponse::Status(2) => write!(f, "status unimplemented command"),
            XrResponse::Status(3) => write!(f, "status invalid property"),
            XrResponse::Status(n) => write!(f, "status {n}"),
            XrResponse::Options { environment, passthrough } => {
                write!(f, "options")?;
                if let Some(e) = environment {
                    write!(f, " environment={}", serde_name(e))?;
                }
                if let Some(p) = passthrough {
                    write!(f, " passthrough={p}")?;
                }
                Ok(())
            }
        }
    }
}

enum Value<'a> {
    Varint(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
    Fixed64,
}

/// A message's (field, value) pairs; `None` if it is malformed.
fn fields(mut b: &[u8]) -> Option<Vec<(u64, Value<'_>)>> {
    fn varint(b: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = b.split_first()?;
            *b = rest;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = varint(&mut b)?;
        let value = match key & 7 {
            WIRE_VARINT => Value::Varint(varint(&mut b)?),
            WIRE_FIXED64 => {
                b = b.get(8..)?;
                Value::Fixed64
            }
            WIRE_LEN => {
                let len = usize::try_from(varint(&mut b)?).ok()?;
                let (m, rest) = (b.get(..len)?, b.get(len..)?);
                b = rest;
                Value::Bytes(m)
            }
            WIRE_FIXED32 => {
                let (n, rest) = (b.get(..4)?, b.get(4..)?);
                b = rest;
                Value::Fixed32(u32::from_le_bytes([n[0], n[1], n[2], n[3]]))
            }
            _ => return None,
        };
        out.push((key >> 3, value));
    }
    Some(out)
}

/// The dual-mode mouse's absolute axes run 0..=32767 across the screen (stock `INPUT_EVENT_ABS_MAX`).
pub const POINTER_ABS_MAX: u32 = 0x7fff;

/// A device pixel coordinate on an axis `extent` pixels long, as a dual-mode mouse axis value.
pub fn pointer_abs(pixel: u32, extent: u32) -> i32 {
    let last = u64::from(extent.saturating_sub(1).max(1));
    (u64::from(pixel.min(extent.saturating_sub(1))) * u64::from(POINTER_ABS_MAX) / last) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    #[test]
    fn console_words_round_trip() {
        for line in [
            "recenter",
            "rotate 0 0.5 -0.25",
            "move 0.1 0 -1",
            "options night 0.5",
            "options day 1",
            "input hand",
            "input eye",
            "viewport zoom",
            "pointer 100 200 down",
            "pointer 1 2 click",
            "hand 100 200 press",
            "eye 5 6 aim",
            "gesture right-pinch",
        ] {
            let cmd = XrCommand::parse(&words(line)).expect(line);
            assert_eq!(cmd.to_string(), line);
            assert_eq!(cmd.console(), format!("xr {line}"));
        }
        assert_eq!(XrCommand::parse(&words("input gaze")), Ok(XrCommand::Input { mode: XrInputMode::Eye }));
        assert_eq!(XrCommand::parse(&words("pointer 3 4")), Ok(XrCommand::Pointer { x: 3, y: 4, phase: XrPointer::Click }));
        for (bad, why) in [
            ("rotate 1 2", "usage: xr recenter"),
            ("rotate 1 x 2", "`x` is not a number"),
            ("rotate 1 NaN 2", "`NaN` is not a number"),
            ("options dusk 0.5", "unknown environment `dusk`"),
            ("options day 2", "passthrough 2 is outside 0-1"),
            ("input feet", "unknown input mode `feet`"),
            ("pointer -1 2", "`-1` is not a pixel coordinate"),
            ("hand 1 2 wave", "unknown hand action `wave`"),
            ("gesture high-five", "unknown gesture `high-five`"),
            ("teleport", "usage: "),
        ] {
            let err = XrCommand::parse(&words(bad)).expect_err(bad);
            assert!(err.starts_with(why), "{bad}: {err}");
        }
    }

    #[test]
    fn requests_encode_as_the_stock_proto() {
        let req = |line: &str| XrCommand::parse(&words(line)).expect(line).request();
        // msg_type = MSG_TYPE_RECENTER_SCREEN (3).
        assert_eq!(req("recenter"), Some(vec![0x08, 0x03]));
        // msg_type 5, xr_head_rotation_event (field 5) {x: 0, y: 0.5, z: 0}: every set field, zeros too.
        assert_eq!(req("rotate 0 0.5 0"), Some(vec![0x08, 0x05, 0x2a, 0x0f, 0x0d, 0, 0, 0, 0, 0x15, 0, 0, 0, 0x3f, 0x1d, 0, 0, 0, 0]));
        // msg_type 6, xr_head_movement_event (field 6) {delta_x: 1, delta_y: -1, delta_z: 0}.
        assert_eq!(
            req("move 1 -1 0"),
            Some(vec![0x08, 0x06, 0x32, 0x0f, 0x0d, 0, 0, 0x80, 0x3f, 0x15, 0, 0, 0x80, 0xbf, 0x1d, 0, 0, 0, 0])
        );
        // msg_type 9, xr_options (field 9) {environment: LIVING_ROOM_DAY (0), passthrough: 1.0}.
        assert_eq!(req("options day 1"), Some(vec![0x08, 0x09, 0x4a, 0x07, 0x08, 0x00, 0x15, 0, 0, 0x80, 0x3f]));
        // msg_type 2, input_mode (field 3) INPUT_MODE_HAND_RAYCAST (2).
        assert_eq!(req("input hand"), Some(vec![0x08, 0x02, 0x18, 0x02]));
        assert_eq!(req("input mouse"), Some(vec![0x08, 0x02, 0x18, 0x01]));
        // msg_type 4, viewport_control_mode (field 4) VIEWPORT_CONTROL_MODE_ZOOM (2).
        assert_eq!(req("viewport zoom"), Some(vec![0x08, 0x04, 0x20, 0x02]));
        assert_eq!(req("pointer 1 1"), None);
        // msg_type MSG_TYPE_SET_HAND_EVENT (12), xr_hand_event (field 11) MouseEvent {x: 100, y: 200,
        // buttons: 1, display: 0}.
        assert_eq!(req("hand 100 200 press"), Some(vec![0x08, 0x0c, 0x5a, 0x09, 0x08, 0x64, 0x10, 0xc8, 0x01, 0x18, 0x01, 0x20, 0x00]));
        // msg_type MSG_TYPE_SET_EYE_EVENT (13), xr_eye_event (field 12), aiming: buttons 0.
        assert_eq!(req("eye 1 2"), Some(vec![0x08, 0x0d, 0x62, 0x08, 0x08, 0x01, 0x10, 0x02, 0x18, 0x00, 0x20, 0x00]));
        // msg_type MSG_TYPE_SET_HAND_GESTURE (11), hand_gesture (field 10) RIGHT_HAND_PINCH (4).
        assert_eq!(req("gesture right-pinch"), Some(vec![0x08, 0x0b, 0x50, 0x04]));
        let mut long = Vec::new();
        put_varint(&mut long, 300);
        assert_eq!(long, [0xac, 0x02]);
    }

    #[test]
    fn responses_decode() {
        assert_eq!(XrResponse::decode(&[0x08, 0x01]), Some(XrResponse::Status(1)));
        assert_eq!(XrResponse::decode(&[0x08, 0x01]).map(|r| r.to_string()).as_deref(), Some("status ok"));
        let options = [0x12, 0x07, 0x08, 0x01, 0x15, 0, 0, 0, 0x3f];
        let decoded = XrResponse::decode(&options).expect("options");
        assert_eq!(decoded, XrResponse::Options { environment: Some(XrEnvironment::Night), passthrough: Some(0.5) });
        assert_eq!(decoded.to_string(), "options environment=night passthrough=0.5");
        // Unknown fields are skipped; truncated or empty messages aren't responses.
        assert_eq!(XrResponse::decode(&[0x19, 1, 2, 3, 4, 5, 6, 7, 8, 0x08, 0x02]), Some(XrResponse::Status(2)));
        assert_eq!(XrResponse::decode(&[0x12, 0x07, 0x08]), None);
        assert_eq!(XrResponse::decode(&[]), None);
    }

    #[test]
    fn commands_serialize_with_an_action_tag() {
        let rotate = XrCommand::Rotate { x: 0.0, y: 0.5, z: 0.0 };
        let json = serde_json::to_value(rotate).expect("json");
        assert_eq!(json, serde_json::json!({"action": "rotate", "x": 0.0, "y": 0.5, "z": 0.0}));
        assert_eq!(serde_json::from_value::<XrCommand>(json).expect("parse"), rotate);
        let opts: XrCommand =
            serde_json::from_value(serde_json::json!({"action": "options", "environment": "night", "passthrough": 0.25})).expect("options");
        assert_eq!(opts, XrCommand::Options { environment: XrEnvironment::Night, passthrough: 0.25 });
        let pointer: XrCommand =
            serde_json::from_value(serde_json::json!({"device": "xr", "action": "pointer", "x": 5, "y": 6})).expect("pointer");
        assert_eq!(pointer, XrCommand::Pointer { x: 5, y: 6, phase: XrPointer::Click });
        assert_eq!(serde_json::to_value(XrCommand::Recenter).expect("json"), serde_json::json!({"action": "recenter"}));
        let hand: XrCommand =
            serde_json::from_value(serde_json::json!({"action": "hand", "x": 1, "y": 2, "press": "press"})).expect("hand");
        assert_eq!(hand, XrCommand::Hand { x: 1, y: 2, press: XrPress::Press });
        let gesture: XrCommand = serde_json::from_value(serde_json::json!({"action": "gesture", "gesture": "left-grab"})).expect("gesture");
        assert_eq!(gesture, XrCommand::Gesture { gesture: XrHandGesture::LeftGrab });
    }

    #[test]
    fn pointer_axes_span_the_screen() {
        assert_eq!(pointer_abs(0, 2560), 0);
        assert_eq!(pointer_abs(2559, 2560), 32767);
        assert_eq!(pointer_abs(9999, 2560), 32767);
        assert_eq!(pointer_abs(1279, 2559), 16383);
        assert_eq!(pointer_abs(0, 0), 0);
    }
}
