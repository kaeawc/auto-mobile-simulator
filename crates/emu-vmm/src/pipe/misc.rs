//! `QemuMiscPipe`: guest → host notifications from `qemu-props` (`heartbeat`, `bootcomplete`).
//! Each message is an i32 length (including the NUL) and the string; the host answers with an
//! i32 length and a payload (empty here). Without an answer the guest spins on a dead pipe.
use std::sync::Arc;

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

pub type GuestEvent = Arc<dyn Fn(&str) + Send + Sync>;

pub struct MiscService {
    pub on_event: Option<GuestEvent>,
}

impl PipeService for MiscService {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "QemuMiscPipe").then(|| Box::new(MiscPipe { on_event: self.on_event.clone(), buf: Vec::new() }) as _)
    }
}

struct MiscPipe {
    on_event: Option<GuestEvent>,
    buf: Vec<u8>,
}

impl PipeHandler for MiscPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= 4 {
            let len = i32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
            let Ok(len) = usize::try_from(len) else { return false };
            if len > 4096 {
                return false;
            }
            if self.buf.len() < 4 + len {
                break;
            }
            let msg: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
            let text = String::from_utf8_lossy(&msg).trim_end_matches('\0').to_string();
            if let Some(cb) = &self.on_event {
                cb(&text);
            }
            to_guest.extend_from_slice(&0i32.to_le_bytes());
        }
        true
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

/// Accepts a pipe and never sends anything: for guest services that must open their pipe but
/// only hear from the host on events simee doesn't generate (e.g. `multidisplay`, whose JNI
/// crashes on a refused open; with one display the host has nothing to say).
pub struct Silent(pub &'static str);

impl PipeService for Silent {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == self.0).then(|| Box::new(Quiet) as _)
    }
}

struct Quiet;

impl PipeHandler for Quiet {
    fn guest_data(&mut self, _data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        true
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn answers_each_message_and_reports_it() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let svc = MiscService { on_event: Some(Arc::new(move |m: &str| s2.lock().expect("lock").push(m.to_string()))) };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut p = svc.open("QemuMiscPipe", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut req = Vec::new();
        for m in ["heartbeat\0", "bootcomplete\0"] {
            req.extend_from_slice(&(m.len() as i32).to_le_bytes());
            req.extend_from_slice(m.as_bytes());
        }
        let mut out = Vec::new();
        assert!(p.guest_data(&req[..7], &mut out));
        assert!(out.is_empty(), "partial message");
        assert!(p.guest_data(&req[7..], &mut out));
        assert_eq!(out, [0u8; 8]);
        assert_eq!(*seen.lock().expect("lock"), vec!["heartbeat".to_string(), "bootcomplete".to_string()]);
    }
}
