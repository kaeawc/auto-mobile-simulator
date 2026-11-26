//! H4 (the UART transport): each HCI packet is a type byte followed by the packet. The guest's
//! `bt_vhci_forwarder` writes whole packets from the kernel's `/dev/vhci` into the `bluetooth`
//! port, but a byte stream may still split or join them, so [`Parser`] reassembles.

/// The H4 packet type byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command = 1,
    Acl = 2,
    Sco = 3,
    Event = 4,
    Iso = 5,
}

impl Kind {
    fn from_byte(b: u8) -> Option<Kind> {
        Some(match b {
            1 => Kind::Command,
            2 => Kind::Acl,
            3 => Kind::Sco,
            4 => Kind::Event,
            5 => Kind::Iso,
            _ => return None,
        })
    }

    /// Header length after the type byte, and the payload length it declares.
    fn header(self) -> (usize, fn(&[u8]) -> usize) {
        match self {
            // opcode u16, length u8
            Kind::Command => (3, |h| usize::from(h[2])),
            // handle u16, length u16
            Kind::Acl => (4, |h| usize::from(u16::from_le_bytes([h[2], h[3]]))),
            // handle u16, length u8
            Kind::Sco => (3, |h| usize::from(h[2])),
            // code u8, length u8
            Kind::Event => (2, |h| usize::from(h[1])),
            // handle u16, length u14 (the top two bits are flags)
            Kind::Iso => (4, |h| usize::from(u16::from_le_bytes([h[2], h[3]]) & 0x3fff)),
        }
    }
}

/// One whole packet: its type and its bytes without the type byte (header included).
#[derive(Debug, PartialEq, Eq)]
pub struct Packet {
    pub kind: Kind,
    pub bytes: Vec<u8>,
}

/// Reassembles packets from a byte stream. A byte that isn't a packet type where one is expected
/// is skipped (and counted), so the stream resynchronises on the next packet boundary.
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// Bytes skipped looking for a packet type.
    pub skipped: u64,
}

impl Parser {
    /// Adds `data` and returns every packet it completes.
    pub fn push(&mut self, data: &[u8]) -> Vec<Packet> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.buf.len() {
            let Some(kind) = Kind::from_byte(self.buf[at]) else {
                at += 1;
                self.skipped += 1;
                continue;
            };
            let (hlen, plen) = kind.header();
            let rest = &self.buf[at + 1..];
            if rest.len() < hlen {
                break;
            }
            let total = hlen + plen(rest);
            if rest.len() < total {
                break;
            }
            out.push(Packet { kind, bytes: rest[..total].to_vec() });
            at += 1 + total;
        }
        self.buf.drain(..at);
        out
    }

    /// Drops a partial packet (the guest closed the port).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// `bytes` framed as an H4 packet of `kind`.
pub fn frame(kind: Kind, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + bytes.len());
    v.push(kind as u8);
    v.extend_from_slice(bytes);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_split_and_joined_packets() {
        let mut p = Parser::default();
        // HCI Reset, split after the type byte and inside the header.
        assert!(p.push(&[1]).is_empty());
        assert!(p.push(&[0x03, 0x0c]).is_empty());
        let got = p.push(&[0x00]);
        assert_eq!(got, [Packet { kind: Kind::Command, bytes: vec![0x03, 0x0c, 0x00] }]);
        // Two commands and an ACL packet in one write; the ACL's 16-bit length.
        let mut joined = vec![1, 0x01, 0x10, 0x00, 1, 0x13, 0x0c, 0x02, b'h', b'i'];
        joined.extend_from_slice(&[2, 0x01, 0x20, 0x03, 0x00, 7, 8, 9]);
        let got = p.push(&joined);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].bytes, [0x13, 0x0c, 0x02, b'h', b'i']);
        assert_eq!(got[2], Packet { kind: Kind::Acl, bytes: vec![0x01, 0x20, 0x03, 0x00, 7, 8, 9] });
        // ISO's length keeps only 14 bits; SCO's is one byte.
        let got = p.push(&[5, 0x01, 0x00, 0x02, 0xc0, 1, 2, 3, 0x01, 0x00, 0x01, 9]);
        assert_eq!(got[0], Packet { kind: Kind::Iso, bytes: vec![0x01, 0x00, 0x02, 0xc0, 1, 2] });
        assert_eq!(got[1], Packet { kind: Kind::Sco, bytes: vec![0x01, 0x00, 0x01, 9] });
        assert_eq!(p.skipped, 0);
    }

    #[test]
    fn skips_bytes_that_are_not_a_packet_type() {
        let mut p = Parser::default();
        let got = p.push(&[0x00, 0xff, 4, 0x0e, 0x01, 0x01]);
        assert_eq!(got, [Packet { kind: Kind::Event, bytes: vec![0x0e, 0x01, 0x01] }]);
        assert_eq!(p.skipped, 2);
        // A partial packet is dropped on close.
        assert!(p.push(&[1, 0x03]).is_empty());
        p.clear();
        assert!(p.push(&[0x0c, 0x00]).is_empty(), "the rest of the dropped command is skipped");
        assert_eq!(frame(Kind::Event, &[0x0e, 0x00]), [4, 0x0e, 0x00]);
    }
}
