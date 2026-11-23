//! `qemud:xr_service`: the Android XR guest's link to the emulator (head pose, environment, input
//! mode). As on stock (`android/physics/XrDeviceModel.cpp`), the guest connects and listens; the
//! host sends nothing on connect, and a message sent while no guest listens is dropped. Messages
//! both ways are qemud-framed protobufs: `EmulatorRequest`s from the host (encoded by
//! `sim_core::xr`), `EmulatorResponse`s from the guest, kept here as the last reply.
use std::sync::{Arc, Mutex};

use super::qemud::{frame, take_frames};
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

#[derive(Default)]
pub struct Xr {
    listeners: Mutex<Vec<PipeCtx>>,
    last_reply: Mutex<Option<Vec<u8>>>,
}

impl Xr {
    /// Sends one encoded `EmulatorRequest` to every connected guest client. False if none is
    /// connected (the message is dropped, as on stock).
    pub fn send(&self, request: &[u8]) -> bool {
        let mut out = Vec::new();
        frame(&mut out, request);
        let mut l = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        l.retain(|c| c.send(HostMsg::Data(out.clone())));
        !l.is_empty()
    }

    /// Whether a guest client is connected.
    pub fn connected(&self) -> bool {
        !self.listeners.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// The guest's last `EmulatorResponse`, undecoded.
    pub fn last_reply(&self) -> Option<Vec<u8>> {
        self.last_reply.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct XrService {
    pub xr: Arc<Xr>,
}

impl PipeService for XrService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "qemud:xr_service" {
            return None;
        }
        let conn = ctx.conn;
        self.xr.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(ctx);
        Some(Box::new(XrPipe { xr: self.xr.clone(), conn, buf: Vec::new() }))
    }
}

struct XrPipe {
    xr: Arc<Xr>,
    conn: u64,
    buf: Vec<u8>,
}

impl PipeHandler for XrPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        if let Some(reply) = take_frames(&mut self.buf).pop() {
            *self.xr.last_reply.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply);
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
        self.xr.listeners.lock().unwrap_or_else(|e| e.into_inner()).retain(|l| l.conn != conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_reach_the_guest_framed_and_replies_are_kept() {
        let xr = Arc::new(Xr::default());
        assert!(!xr.send(&[0x08, 0x03]), "dropped while no guest listens");
        let (tx, rx) = std::sync::mpsc::channel();
        let svc = XrService { xr: xr.clone() };
        assert!(svc.open("qemud:sensors", PipeCtx { conn: 1, events: tx.clone() }).is_none());
        let mut p = svc.open("qemud:xr_service", PipeCtx { conn: 4, events: tx }).expect("open");
        assert!(xr.connected());
        assert!(xr.send(&[0x08, 0x03]));
        let crate::pipe::WorkerEvent::Host { conn: 4, msg: HostMsg::Data(d) } = rx.try_recv().expect("msg") else { panic!() };
        assert_eq!(d, b"0002\x08\x03");
        let mut to_guest = Vec::new();
        assert!(p.host_msg(HostMsg::Data(d), &mut to_guest));
        assert_eq!(to_guest, b"0002\x08\x03");
        // EmulatorResponse{status: RESULT_OK}, split across two writes.
        assert!(p.guest_data(b"000", &mut to_guest));
        assert_eq!(xr.last_reply(), None);
        assert!(p.guest_data(b"2\x08\x01", &mut to_guest));
        assert_eq!(xr.last_reply(), Some(vec![0x08, 0x01]));
        p.closed();
        assert!(!xr.connected() && !xr.send(&[0x08, 0x03]));
    }
}
