//! A UWB subsystem (UWBS) with no radio, speaking UCI (FiRa's UWB Command Interface) to the
//! guest's `android.hardware.uwb-service` over `/dev/hvc2`. It answers what Android's UWB stack
//! sends to bring the chip up (device info, caps, config, the country code) and refuses ranging
//! sessions: there is never a peer to range with.
//!
//! Framing: a 4-byte header, `MT(3) PBF(1) GID(4)`, `OID(6)`, a reserved byte and the payload
//! length (control packets; data packets carry a 16-bit length in bytes 2..4), then the payload.
//! A message split into segments has PBF set on all but the last.

/// Message types.
const MT_DATA: u8 = 0;
const MT_COMMAND: u8 = 1;
const MT_RESPONSE: u8 = 2;
const MT_NOTIFICATION: u8 = 3;

const GID_CORE: u8 = 0x0;
const GID_ANDROID: u8 = 0xC;

mod oid {
    pub const CORE_DEVICE_RESET: u8 = 0x00;
    pub const CORE_DEVICE_STATUS: u8 = 0x01;
    pub const CORE_GET_DEVICE_INFO: u8 = 0x02;
    pub const CORE_GET_CAPS_INFO: u8 = 0x03;
    pub const CORE_SET_CONFIG: u8 = 0x04;
    pub const CORE_GET_CONFIG: u8 = 0x05;
    pub const ANDROID_SET_COUNTRY_CODE: u8 = 0x01;
}

mod status {
    pub const OK: u8 = 0x00;
    pub const REJECTED: u8 = 0x01;
    pub const UNKNOWN_GID: u8 = 0x07;
    pub const UNKNOWN_OID: u8 = 0x08;
}

/// The most the HAL's bytes may be buffered (an unfinished frame) or a segmented command's
/// payload may grow to. UCI's control payloads are at most 255 bytes a segment; real commands
/// are a few hundred bytes in all.
const MAX_BUFFERED: usize = 4096;

/// `DEVICE_STATE_READY`.
const DEVICE_READY: u8 = 0x01;

/// CORE_GET_DEVICE_INFO's answer after the status: UCI 2.0, MAC and PHY 1.3, test 1.1 (each
/// little-endian, major in the low byte), no vendor data. These are the stock emulator's (pica's)
/// values, which Android's UWB stack accepts.
const DEVICE_INFO: [u8; 9] = [0x02, 0x00, 0x01, 0x30, 0x01, 0x30, 0x01, 0x10, 0x00];

fn message(mt: u8, gid: u8, oid: u8, payload: &[u8]) -> Vec<u8> {
    let mut m = vec![mt << 5 | gid & 0x0f, oid & 0x3f, 0, payload.len() as u8];
    m.extend_from_slice(payload);
    m
}

/// Reassembles UCI messages from the byte stream and answers commands.
#[derive(Default)]
pub struct Uwbs {
    buf: Vec<u8>,
    /// A segmented command's payload so far.
    partial: Vec<u8>,
    /// The GID and OID of the message in `partial`.
    partial_id: (u8, u8),
}

impl Uwbs {
    /// Takes bytes the HAL wrote; returns the bytes to send back.
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut at = 0;
        while self.buf.len() - at >= 4 {
            let h = &self.buf[at..at + 4];
            let mt = h[0] >> 5;
            // Not a UCI message type: the stream is out of step (the HAL restarted mid-frame, or
            // the guest is sending junk). Drop what's buffered and wait for a fresh header.
            if mt > MT_NOTIFICATION {
                self.clear();
                return out;
            }
            let len = if mt == MT_DATA { usize::from(u16::from_le_bytes([h[2], h[3]])) } else { usize::from(h[3]) };
            if 4 + len > MAX_BUFFERED {
                self.clear();
                return out;
            }
            if self.buf.len() - at < 4 + len {
                break;
            }
            let (gid, oid, last) = (h[0] & 0x0f, h[1] & 0x3f, h[0] & 0x10 == 0);
            let payload = self.buf[at + 4..at + 4 + len].to_vec();
            at += 4 + len;
            // Data (there's no session to carry it), responses and notifications from the host
            // are dropped.
            if mt != MT_COMMAND {
                continue;
            }
            // A segment of another message restarts reassembly; a message that outgrows the bound
            // is dropped.
            if !self.partial.is_empty() && self.partial_id != (gid, oid) {
                self.partial.clear();
            }
            self.partial_id = (gid, oid);
            if self.partial.len() + payload.len() > MAX_BUFFERED {
                self.partial.clear();
                continue;
            }
            self.partial.extend_from_slice(&payload);
            if last {
                let payload = std::mem::take(&mut self.partial);
                out.extend(answer(gid, oid, &payload));
            }
        }
        self.buf.drain(..at);
        if self.buf.len() > MAX_BUFFERED {
            self.clear();
        }
        out
    }

    /// Drops a partial message.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.partial.clear();
    }
}

fn answer(gid: u8, oid: u8, _payload: &[u8]) -> Vec<u8> {
    let rsp = |p: &[u8]| message(MT_RESPONSE, gid, oid, p);
    match (gid, oid) {
        // The HAL sends a reset when the stack closes it; the stack re-opens with its own ready
        // notification, but a UWBS answers a reset with one too.
        (GID_CORE, oid::CORE_DEVICE_RESET) => {
            let mut m = rsp(&[status::OK]);
            m.extend(message(MT_NOTIFICATION, GID_CORE, oid::CORE_DEVICE_STATUS, &[DEVICE_READY]));
            m
        }
        (GID_CORE, oid::CORE_GET_DEVICE_INFO) => rsp(&[&[status::OK][..], &DEVICE_INFO].concat()),
        // No capability TLVs; no parameters set or read.
        (GID_CORE, oid::CORE_GET_CAPS_INFO | oid::CORE_SET_CONFIG | oid::CORE_GET_CONFIG) => rsp(&[status::OK, 0]),
        (GID_CORE, _) => rsp(&[status::UNKNOWN_OID]),
        (GID_ANDROID, oid::ANDROID_SET_COUNTRY_CODE) => rsp(&[status::OK]),
        // Sessions (GID 1), ranging (GID 2) and data control (GID 3), and the rest of the Android
        // group: rejected, as there's no peer. Test and vendor groups answer UNKNOWN_GID.
        (0x1..=0x3, _) | (GID_ANDROID, _) => rsp(&[status::REJECTED]),
        _ => rsp(&[status::UNKNOWN_GID]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_what_android_sends_at_enable() {
        let mut u = Uwbs::default();
        // CORE_GET_DEVICE_INFO, then ANDROID_SET_COUNTRY_CODE "US" (what the stock emulator's
        // guest sends), answered as pica answered them.
        assert_eq!(u.push(&[0x20, 0x02, 0x00, 0x00]), [0x40, 0x02, 0x00, 0x0a, 0x00, 0x02, 0x00, 0x01, 0x30, 0x01, 0x30, 0x01, 0x10, 0x00]);
        assert_eq!(u.push(&[0x2c, 0x01, 0x00, 0x02, b'U', b'S']), [0x4c, 0x01, 0x00, 0x01, 0x00]);
        // The reset the HAL sends on close: response, then DEVICE_STATUS_NTF(READY).
        assert_eq!(u.push(&[0x20, 0x00, 0x00, 0x01, 0x00]), [0x40, 0x00, 0x00, 0x01, 0x00, 0x60, 0x01, 0x00, 0x01, 0x01]);
        assert_eq!(u.push(&[0x20, 0x03, 0x00, 0x00]), [0x40, 0x03, 0x00, 0x02, 0x00, 0x00]);
    }

    #[test]
    fn reassembles_split_and_segmented_commands() {
        let mut u = Uwbs::default();
        assert!(u.push(&[0x20, 0x02]).is_empty());
        assert_eq!(u.push(&[0x00, 0x00])[..4], [0x40, 0x02, 0x00, 0x0a]);
        // CORE_SET_CONFIG in two segments (PBF on the first): one response.
        assert!(u.push(&[0x30, 0x04, 0x00, 0x02, 1, 2]).is_empty());
        assert_eq!(u.push(&[0x20, 0x04, 0x00, 0x01, 3]), [0x40, 0x04, 0x00, 0x02, 0x00, 0x00]);
        // A data packet (16-bit length) is dropped whole; the command after it is answered.
        let mut data = vec![0x00, 0x00, 0x03, 0x00, 7, 8, 9];
        data.extend_from_slice(&[0x2c, 0x01, 0x00, 0x02, b'D', b'E']);
        assert_eq!(u.push(&data), [0x4c, 0x01, 0x00, 0x01, 0x00]);
        // A partial message is dropped on close.
        assert!(u.push(&[0x20, 0x02, 0x00]).is_empty());
        u.clear();
        assert!(u.push(&[0x01]).is_empty());
    }

    #[test]
    fn segments_are_bounded_and_per_message() {
        let mut u = Uwbs::default();
        // PBF-set segments forever: memory stays bounded and nothing is answered.
        for _ in 0..1000 {
            assert!(u.push(&[0x30, 0x04, 0x00, 0xff].iter().copied().chain([7; 255]).collect::<Vec<u8>>()).is_empty());
            assert!(u.partial.len() <= MAX_BUFFERED);
        }
        // A segment of a different message restarts reassembly rather than joining it.
        let mut u = Uwbs::default();
        assert!(u.push(&[0x30, 0x04, 0x00, 0x02, 1, 2]).is_empty());
        assert_eq!(u.push(&[0x20, 0x02, 0x00, 0x00])[..4], [0x40, 0x02, 0x00, 0x0a]);
        assert!(u.partial.is_empty());
        // An oversized declared data packet doesn't make the buffer wait for 64 KiB.
        assert!(u.push(&[0x00, 0x00, 0xff, 0xff, 1]).is_empty());
        assert!(u.buf.is_empty());
    }

    #[test]
    fn resyncs_on_an_invalid_header() {
        let mut u = Uwbs::default();
        // A restarted HAL leaves the stream mid-frame: what's read as a header has no message
        // type (0xe0 is 7). The buffered bytes are dropped and the next command answers.
        assert!(u.push(&[0xe0, 0, 0, 0, 0, 0, 0, 0, 0]).is_empty());
        assert!(u.buf.is_empty());
        assert_eq!(u.push(&[0x20, 0x03, 0x00, 0x00]), [0x40, 0x03, 0x00, 0x02, 0x00, 0x00]);
    }

    #[test]
    fn refuses_sessions_and_unknown_groups() {
        let mut u = Uwbs::default();
        // SESSION_INIT (GID 1, OID 0): rejected.
        assert_eq!(u.push(&[0x21, 0x00, 0x00, 0x05, 1, 0, 0, 0, 0]), [0x41, 0x00, 0x00, 0x01, status::REJECTED]);
        assert_eq!(u.push(&[0x20, 0x2a, 0x00, 0x00]), [0x40, 0x2a, 0x00, 0x01, status::UNKNOWN_OID]);
        assert_eq!(u.push(&[0x2e, 0x00, 0x00, 0x00]), [0x4e, 0x00, 0x00, 0x01, status::UNKNOWN_GID]);
    }
}
