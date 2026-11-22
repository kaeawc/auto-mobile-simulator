//! `clipboard`: the guest clipboard agent's host side.
//!
//! Both directions carry the same message: a u32 (LE) length and that many bytes of UTF-8. The
//! guest sends its clipboard whenever it changes and blocks reading for host updates; we keep
//! the latest value in [`Clipboard`] and push host-side changes (console, desktop) to every
//! open pipe. Without a service the agent reopens the pipe in a tight loop.
use std::sync::{Arc, Mutex};

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// Largest clipboard we accept from the guest (the stock limit is in the same range).
const MAX: usize = 16 << 20;

#[derive(Default)]
pub struct Clipboard {
    text: Mutex<Vec<u8>>,
    listeners: Mutex<Vec<PipeCtx>>,
}

fn message(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

impl Clipboard {
    pub fn get(&self) -> Vec<u8> {
        self.text.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sets the host clipboard and sends it to the guest.
    pub fn set(&self, data: &[u8]) {
        *self.text.lock().unwrap_or_else(|e| e.into_inner()) = data.to_vec();
        let msg = message(data);
        self.listeners.lock().unwrap_or_else(|e| e.into_inner()).retain(|l| l.send(HostMsg::Data(msg.clone())));
    }
}

pub struct ClipboardService {
    pub clipboard: Arc<Clipboard>,
}

impl PipeService for ClipboardService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "clipboard" {
            return None;
        }
        let conn = ctx.conn;
        self.clipboard.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(ctx);
        Some(Box::new(ClipboardPipe { clipboard: self.clipboard.clone(), conn, buf: Vec::new() }))
    }
}

struct ClipboardPipe {
    clipboard: Arc<Clipboard>,
    conn: u64,
    buf: Vec<u8>,
}

impl PipeHandler for ClipboardPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= 4 {
            let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if len > MAX {
                return false;
            }
            if self.buf.len() < 4 + len {
                break;
            }
            let text: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
            *self.clipboard.text.lock().unwrap_or_else(|e| e.into_inner()) = text;
        }
        true
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn closed(&mut self) {
        let conn = self.conn;
        self.clipboard.listeners.lock().unwrap_or_else(|e| e.into_inner()).retain(|l| l.conn != conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_sets_and_host_pushes() {
        let cb = Arc::new(Clipboard::default());
        let svc = ClipboardService { clipboard: cb.clone() };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut p = svc.open("clipboard", PipeCtx { conn: 7, events: tx }).expect("open");
        let msg = message(b"hello");
        let mut out = Vec::new();
        assert!(p.guest_data(&msg[..3], &mut out));
        assert!(cb.get().is_empty(), "partial");
        assert!(p.guest_data(&msg[3..], &mut out));
        assert_eq!(cb.get(), b"hello");
        cb.set(b"from host");
        let crate::pipe::WorkerEvent::Host { conn: 7, msg: HostMsg::Data(d) } = rx.try_recv().expect("pushed") else {
            panic!("data")
        };
        assert_eq!(d, message(b"from host"));
        p.closed();
        cb.set(b"x");
        assert!(rx.try_recv().is_err(), "closed pipes are dropped");
    }
}
