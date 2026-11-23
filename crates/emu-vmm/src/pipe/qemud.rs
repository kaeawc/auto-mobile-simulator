//! `qemud:` services with the qemud framing: each message is a 4-hex-digit length + payload.
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// Appends one framed message.
pub fn frame(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(format!("{:04x}", payload.len()).as_bytes());
    out.extend_from_slice(payload);
}

/// Splits complete framed messages off the front of `buf`, draining it once (not per message).
pub fn take_frames(buf: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut msgs = Vec::new();
    let mut at = 0;
    while buf.len() - at >= 4 {
        let Some(len) = std::str::from_utf8(&buf[at..at + 4]).ok().and_then(|h| usize::from_str_radix(h, 16).ok()) else {
            buf.clear(); // not qemud framing: drop rather than misparse
            return msgs;
        };
        if buf.len() - at < 4 + len {
            break;
        }
        msgs.push(buf[at + 4..at + 4 + len].to_vec());
        at += 4 + len;
    }
    buf.drain(..at);
    msgs
}

/// `qemud:boot-properties`: `qemu-props` asks for `list` early in boot and sets each
/// `name=value` it receives (LCD density, navigation keys, timezone, ...).
pub struct BootProperties {
    pub props: Vec<(String, String)>,
}

impl PipeService for BootProperties {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "qemud:boot-properties").then(|| Box::new(BootPropsPipe { props: self.props.clone(), buf: Vec::new() }) as _)
    }
}

struct BootPropsPipe {
    props: Vec<(String, String)>,
    buf: Vec<u8>,
}

impl PipeHandler for BootPropsPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        for msg in take_frames(&mut self.buf) {
            if msg == b"list" {
                for (k, v) in &self.props {
                    frame(to_guest, format!("{k}={v}").as_bytes());
                }
                // A lone NUL ends the list.
                frame(to_guest, b"\0");
            }
        }
        true
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trip() {
        let mut out = Vec::new();
        frame(&mut out, b"list");
        frame(&mut out, b"");
        out.extend_from_slice(b"00"); // partial header stays buffered
        let msgs = take_frames(&mut out);
        assert_eq!(msgs, vec![b"list".to_vec(), Vec::new()]);
        assert_eq!(out, b"00");
    }

    #[test]
    fn many_frames_and_a_partial_body() {
        let mut out = Vec::new();
        for i in 0..1000u32 {
            frame(&mut out, i.to_string().as_bytes());
        }
        out.extend_from_slice(b"0005ab"); // header plus part of the body
        let msgs = take_frames(&mut out);
        assert_eq!(msgs.len(), 1000);
        assert_eq!((msgs[0].as_slice(), msgs[999].as_slice()), (&b"0"[..], &b"999"[..]));
        assert_eq!(out, b"0005ab", "the partial frame waits");
        out.extend_from_slice(b"cde");
        assert_eq!(take_frames(&mut out), vec![b"abcde".to_vec()]);
        assert!(out.is_empty());
        let mut junk = b"zz12frame".to_vec();
        assert!(take_frames(&mut junk).is_empty() && junk.is_empty(), "not qemud framing: dropped");
    }

    #[test]
    fn lists_properties() {
        let mut p = BootPropsPipe { props: vec![("qemu.sf.lcd_density".into(), "420".into())], buf: Vec::new() };
        let mut req = Vec::new();
        frame(&mut req, b"list");
        let mut out = Vec::new();
        assert!(p.guest_data(&req[..3], &mut out));
        assert!(out.is_empty(), "waits for the full frame");
        assert!(p.guest_data(&req[3..], &mut out));
        assert_eq!(out, b"0017qemu.sf.lcd_density=4200001\0");
    }
}
