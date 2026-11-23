//! The live display stream between a device process and a front end: frames one way, input lines
//! the other. The Android emulator serves it on `<avd>/emu-rs/display.sock`; the desktop window
//! reads it.
//!
//! Client → server, one text line each ([`Input`]):
//! - `hello <max_width>`: start streaming, downscaled by a whole factor to at most that width;
//! - `touch <x> <y> down|move|up` in device pixels;
//! - `key <name>`: press and release a key by its console name (`HOME`, `BACK`, `POWER`, `A`, ...);
//! - `wheel <ticks>` on the rotary / scroll device.
//!
//! Server → client, per frame: a 32-byte [`FrameHeader`] (`b"EMUF"`, then little-endian u32
//! width, height, device width, device height, u64 frame number, u32 byte count), then RGBA rows.

use crate::gesture::Phase;

/// The bytes every frame header starts with.
pub const MAGIC: &[u8; 4] = b"EMUF";
/// Size of a frame header in bytes.
pub const HEADER_LEN: usize = 32;

/// Describes the RGBA rows that follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    /// Size of the (possibly downscaled) frame that follows.
    pub width: u32,
    pub height: u32,
    /// The device's own display size: input coordinates are in these pixels.
    pub device_width: u32,
    pub device_height: u32,
    pub frame: u64,
    /// Byte count of the RGBA rows: `width * height * 4`.
    pub len: u32,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(MAGIC);
        for (i, v) in [self.width, self.height, self.device_width, self.device_height].into_iter().enumerate() {
            out[4 + i * 4..8 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        out[20..28].copy_from_slice(&self.frame.to_le_bytes());
        out[28..32].copy_from_slice(&self.len.to_le_bytes());
        out
    }

    /// Parses and checks a header: the magic, and that the byte count matches the size.
    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self, String> {
        if &b[0..4] != MAGIC {
            return Err("bad frame header".into());
        }
        let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let mut frame = [0u8; 8];
        frame.copy_from_slice(&b[20..28]);
        let h = FrameHeader {
            width: u(4),
            height: u(8),
            device_width: u(12),
            device_height: u(16),
            frame: u64::from_le_bytes(frame),
            len: u(28),
        };
        if u64::from(h.len) != u64::from(h.width) * u64::from(h.height) * 4 {
            return Err("frame size mismatch".into());
        }
        Ok(h)
    }
}

/// One client → server line.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Hello { max_width: u32 },
    Touch { x: f32, y: f32, phase: Phase },
    Key(String),
    Wheel(i32),
}

impl Input {
    /// The line, without its newline.
    pub fn to_line(&self) -> String {
        match self {
            Self::Hello { max_width } => format!("hello {max_width}"),
            Self::Touch { x, y, phase } => {
                let phase = match phase {
                    Phase::Down => "down",
                    Phase::Move => "move",
                    Phase::Up => "up",
                };
                format!("touch {x:.0} {y:.0} {phase}")
            }
            Self::Key(name) => format!("key {name}"),
            Self::Wheel(ticks) => format!("wheel {ticks}"),
        }
    }

    /// Parses a line; `None` for anything malformed (servers ignore those).
    pub fn parse(line: &str) -> Option<Self> {
        let w: Vec<&str> = line.split_whitespace().collect();
        Some(match w.as_slice() {
            ["hello", mw] => Self::Hello { max_width: mw.parse().unwrap_or(0) },
            ["touch", x, y, phase] => Self::Touch {
                x: x.parse().ok()?,
                y: y.parse().ok()?,
                phase: match *phase {
                    "down" => Phase::Down,
                    "move" => Phase::Move,
                    _ => Phase::Up,
                },
            },
            ["key", name] => Self::Key((*name).to_owned()),
            ["wheel", ticks] => Self::Wheel(ticks.parse().ok()?),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip_and_are_checked() {
        let h = FrameHeader { width: 360, height: 640, device_width: 720, device_height: 1280, frame: 7, len: 360 * 640 * 4 };
        let bytes = h.encode();
        assert_eq!(&bytes[0..4], b"EMUF");
        assert_eq!(&bytes[12..16], &720u32.to_le_bytes());
        assert_eq!(FrameHeader::decode(&bytes), Ok(h));
        let mut bad = bytes;
        bad[28] ^= 1;
        assert_eq!(FrameHeader::decode(&bad), Err("frame size mismatch".into()));
        bad = bytes;
        bad[0] = b'X';
        assert_eq!(FrameHeader::decode(&bad), Err("bad frame header".into()));
    }

    #[test]
    fn input_lines_round_trip() {
        let inputs = [
            Input::Hello { max_width: 720 },
            Input::Touch { x: 10.0, y: 20.0, phase: Phase::Down },
            Input::Touch { x: 11.0, y: 21.0, phase: Phase::Move },
            Input::Touch { x: 12.0, y: 22.0, phase: Phase::Up },
            Input::Key("HOME".into()),
            Input::Wheel(-2),
        ];
        for i in inputs {
            assert_eq!(Input::parse(&i.to_line()), Some(i.clone()), "{}", i.to_line());
        }
        assert_eq!(Input::Touch { x: 10.4, y: 19.6, phase: Phase::Up }.to_line(), "touch 10 20 up");
        assert_eq!(Input::parse("touch 1 nope up"), None);
        assert_eq!(Input::parse("dance"), None);
    }
}
