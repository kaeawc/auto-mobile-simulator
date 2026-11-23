//! `qemud:fingerprintlisten` and `qemud:camera`: the goldfish fingerprint and camera HALs.
//!
//! Fingerprint: the HAL sends a qemud-framed `listen`, then waits for `on:<id>` (a finger with
//! that enrolled id touches the sensor) or `off` (lifted). The console's `finger touch <id>` /
//! `finger remove` drive it through [`Fingerprint`].
//!
//! Camera: the provider sends NUL-terminated queries (`list`, ...) and reads an 8-hex-digit
//! length plus a reply starting `ok` / `ko`. emu-rs has no host cameras yet, so `list` answers
//! with an empty list and other queries fail; without a service the provider retries the pipe
//! in a tight loop.
use std::sync::{Arc, Mutex};

use super::qemud::{frame, take_frames};
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

#[derive(Default)]
pub struct Fingerprint {
    listeners: Mutex<Vec<PipeCtx>>,
}

impl Fingerprint {
    fn broadcast(&self, msg: &str) -> bool {
        let mut out = Vec::new();
        frame(&mut out, msg.as_bytes());
        let mut l = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        l.retain(|c| c.send(HostMsg::Data(out.clone())));
        !l.is_empty()
    }

    /// A finger with enrolled id `id` (> 0) touches the sensor. False if no HAL is listening.
    pub fn touch(&self, id: u32) -> bool {
        self.broadcast(&format!("on:{id}"))
    }

    pub fn remove(&self) -> bool {
        self.broadcast("off")
    }
}

pub struct FingerprintService {
    pub fingerprint: Arc<Fingerprint>,
}

impl PipeService for FingerprintService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "qemud:fingerprintlisten" {
            return None;
        }
        let conn = ctx.conn;
        self.fingerprint.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(ctx);
        Some(Box::new(FingerprintPipe { fingerprint: self.fingerprint.clone(), conn, buf: Vec::new() }))
    }
}

struct FingerprintPipe {
    fingerprint: Arc<Fingerprint>,
    conn: u64,
    buf: Vec<u8>,
}

impl PipeHandler for FingerprintPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        // Only `listen` is ever sent; nothing to answer.
        self.buf.extend_from_slice(data);
        let _ = take_frames(&mut self.buf);
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
        self.fingerprint.listeners.lock().unwrap_or_else(|e| e.into_inner()).retain(|l| l.conn != conn);
    }
}

pub struct CameraService;

impl PipeService for CameraService {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "qemud:camera" || name.starts_with("qemud:camera:")).then(|| Box::new(CameraPipe { buf: Vec::new() }) as _)
    }
}

struct CameraPipe {
    buf: Vec<u8>,
}

fn camera_reply(to_guest: &mut Vec<u8>, payload: &[u8]) {
    to_guest.extend_from_slice(format!("{:08x}", payload.len()).as_bytes());
    to_guest.extend_from_slice(payload);
}

impl PipeHandler for CameraPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        while let Some(nul) = self.buf.iter().position(|&b| b == 0) {
            let query: Vec<u8> = self.buf.drain(..=nul).collect();
            match &query[..query.len() - 1] {
                b"list" => camera_reply(to_guest, b"ok\0"),
                _ => camera_reply(to_guest, b"ko:no cameras\0"),
            }
        }
        self.buf.len() < 4096
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_touch_reaches_listeners() {
        let fp = Arc::new(Fingerprint::default());
        assert!(!fp.touch(1), "nobody listening");
        let (tx, rx) = std::sync::mpsc::channel();
        let svc = FingerprintService { fingerprint: fp.clone() };
        let mut p = svc.open("qemud:fingerprintlisten", PipeCtx { conn: 3, events: tx }).expect("open");
        let mut req = Vec::new();
        frame(&mut req, b"listen");
        let mut out = Vec::new();
        assert!(p.guest_data(&req, &mut out));
        assert!(out.is_empty());
        assert!(fp.touch(2));
        assert!(fp.remove());
        let mut got = Vec::new();
        for _ in 0..2 {
            let crate::pipe::WorkerEvent::Host { msg: HostMsg::Data(d), .. } = rx.try_recv().expect("msg") else { panic!() };
            got.extend_from_slice(&d);
        }
        assert_eq!(take_frames(&mut got), vec![b"on:2".to_vec(), b"off".to_vec()]);
        p.closed();
        assert!(!fp.touch(1));
    }

    #[test]
    fn camera_lists_nothing() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut p = CameraService.open("qemud:camera", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut out = Vec::new();
        assert!(p.guest_data(b"li", &mut out));
        assert!(out.is_empty());
        assert!(p.guest_data(b"st\0connect\0", &mut out));
        assert_eq!(out, [&b"00000003ok\x00"[..], b"0000000eko:no cameras\x00"].concat());
    }
}
