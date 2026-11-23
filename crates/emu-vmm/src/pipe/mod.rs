//! Host services the guest reaches through "qemu pipes".
//!
//! On current system images the guest's pipe library connects a vsock stream to the host
//! (CID 2, port 5000) and sends `pipe:<name>\0`; everything after that belongs to the named
//! service. `qemud:adb` is adbd's transport; others (boot properties, sensors, ...) follow.
pub mod adb;
pub mod clipboard;
pub mod fingerprint;
pub mod gps;
pub mod misc;
pub mod qemud;
pub mod sensors;
pub mod xr;

use std::net::TcpStream;
use std::sync::mpsc::Sender;

/// Messages from host-side threads to the vsock worker, addressed to one pipe connection.
pub enum HostMsg {
    Data(Vec<u8>),
    Close,
    /// An accepted host TCP client for this connection (adb).
    Attach(TcpStream),
}

/// Builds the host end of a host-initiated vsock connection once it has an id.
pub type MakeHandler = Box<dyn FnOnce(PipeCtx) -> Box<dyn PipeHandler> + Send>;

pub enum WorkerEvent {
    /// The guest notified a virtqueue.
    Kick,
    Host {
        conn: u64,
        msg: HostMsg,
    },
    /// Open a stream to `guest_port` in the guest. `refused` runs if nothing listens there.
    Connect {
        guest_port: u32,
        make: MakeHandler,
        refused: Box<dyn FnOnce() + Send>,
    },
    Stop,
}

/// Handed to a service when a guest opens a pipe; lets host threads talk to that connection.
#[derive(Clone)]
pub struct PipeCtx {
    pub conn: u64,
    pub events: Sender<WorkerEvent>,
}

impl PipeCtx {
    pub fn send(&self, msg: HostMsg) -> bool {
        self.events.send(WorkerEvent::Host { conn: self.conn, msg }).is_ok()
    }
}

/// One open pipe. Bytes for the guest are appended to `to_guest`; returning false closes it.
pub trait PipeHandler: Send {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool;
    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool;
    fn closed(&mut self) {}
}

pub trait PipeService: Send + Sync {
    /// Opens `name` (e.g. `qemud:adb:5555`), or returns None for names it doesn't serve.
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>>;
}

/// Routes pipe names to services by prefix.
#[derive(Default)]
pub struct PipeRegistry {
    services: Vec<(String, Box<dyn PipeService>)>,
}

impl PipeRegistry {
    pub fn register(&mut self, prefix: &str, svc: Box<dyn PipeService>) {
        self.services.push((prefix.to_string(), svc));
    }

    pub fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        self.services.iter().filter(|(p, _)| name.starts_with(p.as_str())).find_map(|(_, s)| s.open(name, ctx.clone()))
    }
}
