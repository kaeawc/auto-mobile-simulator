//! vhost-user backend: serves one of emu's virtio devices to a frontend that runs the guest's CPU
//! (QEMU under TCG, for hosts without hardware virtualization; issue #55).
//!
//! The protocol is QEMU's `docs/interop/vhost-user.rst`: the frontend owns the transport (PCI
//! config space, MSI-X) and the guest's RAM, a memfd it shares here with `SET_MEM_TABLE`; the
//! backend owns the device's virtqueues. A queue notify reaches the backend as a write to the
//! ring's kick eventfd, and the backend completes requests by writing the ring's call eventfd,
//! which the frontend turns into the guest's interrupt. So the device model is the one emu's own
//! machine runs ([`VirtioDevice`]), fed an [`Activation`] whose RAM is the frontend's and whose
//! interrupt is an [`Interrupt::edge`] over the call eventfds.
//!
//! What this backend supports, and refuses otherwise:
//! - Guest RAM in one memory fd mapped at a fixed guest-physical delta (QEMU's
//!   `memory-backend-memfd,share=on` below 4 GiB, i.e. `pc` machines up to 3 GiB of RAM). The
//!   whole fd is mapped once, so later memory tables over the same fd (the firmware's PAM changes
//!   while it boots) only update the address translation.
//! - Split rings starting at index 0 (a fresh start or after a reset, what QEMU sends; resuming a
//!   stopped ring mid-stream would need the device's own ring state).
//! - Kick and call eventfds (no polling mode), protocol features negotiated.
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::thread::JoinHandle;

use emu_hvf::GuestRam;
use rustix::event::{EventfdFlags, PollFd, PollFlags, eventfd, poll};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};

use crate::irq::IrqLine;
use crate::virtio::mmio::F_VERSION_1;
use crate::virtio::queue::QueueConfig;
use crate::virtio::{Activation, Interrupt, VirtioDevice};

/// Frontend requests (vhost-user.rst, "Front-end message types").
pub mod req {
    pub const GET_FEATURES: u32 = 1;
    pub const SET_FEATURES: u32 = 2;
    pub const SET_OWNER: u32 = 3;
    pub const RESET_OWNER: u32 = 4;
    pub const SET_MEM_TABLE: u32 = 5;
    pub const SET_VRING_NUM: u32 = 8;
    pub const SET_VRING_ADDR: u32 = 9;
    pub const SET_VRING_BASE: u32 = 10;
    pub const GET_VRING_BASE: u32 = 11;
    pub const SET_VRING_KICK: u32 = 12;
    pub const SET_VRING_CALL: u32 = 13;
    pub const SET_VRING_ERR: u32 = 14;
    pub const GET_PROTOCOL_FEATURES: u32 = 15;
    pub const SET_PROTOCOL_FEATURES: u32 = 16;
    pub const GET_QUEUE_NUM: u32 = 17;
    pub const SET_VRING_ENABLE: u32 = 18;
    pub const GET_CONFIG: u32 = 24;
    pub const SET_CONFIG: u32 = 25;
    pub const RESET_DEVICE: u32 = 34;
}

/// Protocol feature bits (`GET_PROTOCOL_FEATURES`).
pub mod proto {
    pub const MQ: u64 = 1 << 0;
    pub const REPLY_ACK: u64 = 1 << 3;
    pub const CONFIG: u64 = 1 << 9;
    pub const RESET_DEVICE: u64 = 1 << 13;
}

/// The device feature bit that says the backend speaks protocol features.
pub const F_PROTOCOL_FEATURES: u64 = 1 << 30;
const SUPPORTED_PROTOCOL: u64 = proto::MQ | proto::REPLY_ACK | proto::CONFIG | proto::RESET_DEVICE;

const VERSION: u32 = 1;
const FLAG_REPLY: u32 = 1 << 2;
const FLAG_NEED_REPLY: u32 = 1 << 3;
const HEADER: usize = 12;
/// Largest payload accepted: a memory table (8 regions) or a config space read is far smaller.
const MAX_PAYLOAD: usize = 4096;
/// Most fds one message carries (a memory table's regions).
const MAX_FDS: usize = 8;
/// `SET_VRING_KICK`/`CALL`: no fd follows, poll instead.
const VRING_NOFD: u64 = 1 << 8;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vhost-user socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("vhost-user: {0}")]
    Protocol(String),
}

fn protocol(msg: impl Into<String>) -> Error {
    Error::Protocol(msg.into())
}

pub type Result<T> = std::result::Result<T, Error>;

/// One message from the frontend.
#[derive(Debug, Default)]
pub struct Msg {
    pub request: u32,
    pub flags: u32,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

impl Msg {
    fn u32_at(&self, off: usize) -> Result<u32> {
        let b = self.payload.get(off..off + 4).ok_or_else(|| protocol(format!("request {}: payload too short", self.request)))?;
        Ok(u32::from_le_bytes(b.try_into().unwrap_or_default()))
    }

    fn u64_at(&self, off: usize) -> Result<u64> {
        let b = self.payload.get(off..off + 8).ok_or_else(|| protocol(format!("request {}: payload too short", self.request)))?;
        Ok(u64::from_le_bytes(b.try_into().unwrap_or_default()))
    }
}

/// Reads one message: the header with any fds attached to it, then its payload. `None` at a clean
/// end of stream (the frontend went away between messages).
pub fn read_msg(sock: &mut UnixStream) -> Result<Option<Msg>> {
    let mut head = [0u8; HEADER];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut got = 0;
    let mut fds = Vec::new();
    while got < HEADER {
        let n = recvmsg(&*sock, &mut [std::io::IoSliceMut::new(&mut head[got..])], &mut control, RecvFlags::CMSG_CLOEXEC)
            .map_err(std::io::Error::from)?
            .bytes;
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(r) = m {
                fds.extend(r);
            }
        }
        if n == 0 {
            return if got == 0 { Ok(None) } else { Err(protocol("connection closed inside a message header")) };
        }
        got += n;
    }
    let request = u32::from_le_bytes(head[0..4].try_into().unwrap_or_default());
    let flags = u32::from_le_bytes(head[4..8].try_into().unwrap_or_default());
    let size = u32::from_le_bytes(head[8..12].try_into().unwrap_or_default()) as usize;
    if flags & 3 != VERSION {
        return Err(protocol(format!("request {request}: version {} (want {VERSION})", flags & 3)));
    }
    if size > MAX_PAYLOAD {
        return Err(protocol(format!("request {request}: {size}-byte payload")));
    }
    let mut payload = vec![0u8; size];
    sock.read_exact(&mut payload)?;
    Ok(Some(Msg { request, flags, payload, fds }))
}

/// Encodes a message (reply flag set: the backend only ever replies on this socket).
pub fn encode(request: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&request.to_le_bytes());
    out.extend_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// One region of the frontend's memory table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    gpa: u64,
    size: u64,
    /// The frontend's own address of the region (rings are given in these).
    uaddr: u64,
}

/// The guest's RAM as this backend maps it, and the frontend's view for translating ring
/// addresses.
struct Memory {
    ram: Arc<GuestRam>,
    /// (device, inode) of the fd mapped, and its guest-physical address minus file offset.
    file: (u64, u64),
    delta: u64,
    regions: Vec<Region>,
}

impl Memory {
    /// A frontend address as a guest-physical one.
    fn gpa(&self, uaddr: u64) -> Option<u64> {
        self.regions.iter().find(|r| uaddr >= r.uaddr && uaddr - r.uaddr < r.size).map(|r| r.gpa + (uaddr - r.uaddr))
    }
}

#[derive(Default)]
struct Ring {
    num: u16,
    /// Frontend addresses of the descriptor table, used ring and available ring.
    addr: Option<(u64, u64, u64)>,
    kick: Option<OwnedFd>,
    enabled: bool,
}

/// The call eventfds, by queue; the frontend may swap one at any time (it parks a masked
/// vector's calls on its own notifier and swaps back on unmask), so they're read at each signal.
type Calls = Arc<RwLock<Vec<Option<OwnedFd>>>>;

/// The device's interrupt, over its queues' call eventfds. The device model raises one
/// interrupt for all its queues, so a raise signals each queue whose used ring moved since that
/// queue was last signalled (read from guest RAM): signalling every queue cost the guest a
/// spurious interrupt per completion, and devices such as vsock share one MSI vector between
/// their queues, so each spurious call was a second interrupt.
struct CallLine {
    calls: Calls,
    mem: Arc<GuestRam>,
    /// Per queue: the guest address of its used ring's index, and the index last signalled.
    used: Vec<Option<(u64, AtomicU16)>>,
}

impl IrqLine for CallLine {
    fn set(&self, level: bool) {
        if !level {
            return;
        }
        let calls = self.calls.read().unwrap_or_else(PoisonError::into_inner);
        for (q, used) in self.used.iter().enumerate() {
            let Some((at, last)) = used else { continue };
            let Ok(idx) = self.mem.read_u16(*at) else { continue };
            if last.swap(idx, Ordering::AcqRel) == idx {
                continue;
            }
            if let Some(fd) = calls.get(q).and_then(Option::as_ref) {
                // An eventfd adds to its counter; a full one (2^64 - 2 pending) can't happen here.
                let _ = rustix::io::write(fd, &1u64.to_ne_bytes());
            }
        }
    }
}

/// The running device: the thread forwarding kicks to it, and how to stop that thread.
struct Active {
    stop: Arc<OwnedFd>,
    kicks: Option<JoinHandle<()>>,
}

impl Active {
    fn stop(&mut self) {
        let _ = rustix::io::write(&*self.stop, &1u64.to_ne_bytes());
        if let Some(t) = self.kicks.take() {
            let _ = t.join();
        }
    }
}

/// One connection's state.
pub struct Session {
    dev: Arc<Mutex<Box<dyn VirtioDevice>>>,
    features: Option<u64>,
    protocol: u64,
    mem: Option<Memory>,
    rings: Vec<Ring>,
    calls: Calls,
    active: Option<Active>,
}

impl Session {
    pub fn new(dev: Arc<Mutex<Box<dyn VirtioDevice>>>) -> Session {
        let n = dev.lock().unwrap_or_else(PoisonError::into_inner).num_queues();
        Session {
            dev,
            features: None,
            protocol: 0,
            mem: None,
            rings: (0..n).map(|_| Ring::default()).collect(),
            calls: Arc::new(RwLock::new((0..n).map(|_| None).collect())),
            active: None,
        }
    }

    fn device(&self) -> std::sync::MutexGuard<'_, Box<dyn VirtioDevice>> {
        self.dev.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ring(&mut self, index: u64) -> Result<&mut Ring> {
        let n = self.rings.len();
        self.rings.get_mut(index as usize).ok_or_else(|| protocol(format!("ring {index} of {n}")))
    }

    /// Stops the device (rings stopped, the frontend's reset, or it went away).
    fn deactivate(&mut self) {
        if let Some(mut a) = self.active.take() {
            a.stop();
            self.device().reset();
        }
    }

    /// Starts the device once every ring the frontend has set up has its addresses, kick and call
    /// fds and is enabled, the features are set and the memory table is in.
    fn try_activate(&mut self) -> Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        let (Some(features), Some(mem)) = (self.features, &self.mem) else { return Ok(()) };
        let calls = self.calls.read().unwrap_or_else(PoisonError::into_inner);
        let set_up: Vec<usize> = (0..self.rings.len()).filter(|&i| self.rings[i].addr.is_some()).collect();
        let ready = |i: &usize| self.rings[*i].kick.is_some() && calls[*i].is_some() && self.rings[*i].enabled;
        if set_up.is_empty() || !set_up.iter().all(ready) {
            return Ok(());
        }
        drop(calls);
        let mut queues = Vec::with_capacity(self.rings.len());
        for r in &self.rings {
            queues.push(match r.addr {
                Some((desc, used, avail)) => {
                    let gpa = |a: u64| mem.gpa(a).ok_or_else(|| protocol(format!("ring address {a:#x} outside the memory table")));
                    Some(QueueConfig { size: r.num, desc: gpa(desc)?, avail: gpa(avail)?, used: gpa(used)? })
                }
                None => None,
            });
        }
        let used = queues.iter().map(|q| q.map(|q| (q.used + 2, AtomicU16::new(mem.ram.read_u16(q.used + 2).unwrap_or(0))))).collect();
        let irq = Arc::new(Interrupt::edge(Arc::new(CallLine { calls: self.calls.clone(), mem: mem.ram.clone(), used })));
        let kicks: Vec<(usize, OwnedFd)> =
            self.rings.iter().enumerate().filter_map(|(i, r)| Some((i, r.kick.as_ref()?.try_clone().ok()?))).collect();
        let stop = Arc::new(eventfd(0, EventfdFlags::CLOEXEC).map_err(std::io::Error::from)?);
        self.device().activate(Activation { mem: mem.ram.clone(), irq, queues, features }).map_err(protocol)?;
        let (dev, stop2) = (self.dev.clone(), stop.clone());
        let kicks = std::thread::Builder::new().name("vhost-kick".into()).spawn(move || forward_kicks(&dev, &kicks, &stop2))?;
        self.active = Some(Active { stop, kicks: Some(kicks) });
        Ok(())
    }

    fn set_mem_table(&mut self, m: &mut Msg) -> Result<()> {
        let n = m.u32_at(0)? as usize;
        if n == 0 || n > MAX_FDS || m.fds.len() != n {
            return Err(protocol(format!("memory table of {n} regions with {} fds", m.fds.len())));
        }
        let mut regions = Vec::with_capacity(n);
        let mut file = None;
        let mut delta = None;
        for i in 0..n {
            let o = 8 + i * 32;
            let (gpa, size, uaddr, offset) = (m.u64_at(o)?, m.u64_at(o + 8)?, m.u64_at(o + 16)?, m.u64_at(o + 24)?);
            let st = rustix::fs::fstat(&m.fds[i]).map_err(std::io::Error::from)?;
            let this = (st.st_dev, st.st_ino);
            let d = gpa.wrapping_sub(offset);
            if file.is_some_and(|f| f != this) || delta.is_some_and(|x| x != d) {
                return Err(protocol(
                    "guest RAM in more than one fd or at more than one offset (QEMU: one memory-backend-memfd below 4 GiB)",
                ));
            }
            (file, delta) = (Some(this), Some(d));
            regions.push(Region { gpa, size, uaddr });
        }
        let (file, delta) = (file.unwrap_or_default(), delta.unwrap_or_default());
        if let Some(mem) = &mut self.mem
            && mem.file == file
            && mem.delta == delta
        {
            // Same RAM, another view of it (regions split or joined around ROM and PAM).
            mem.regions = regions;
            return Ok(());
        }
        if self.active.is_some() {
            return Err(protocol("the guest RAM moved to another fd while the device runs"));
        }
        let len = rustix::fs::fstat(&m.fds[0]).map_err(std::io::Error::from)?.st_size as u64;
        let ram = GuestRam::map_shared(delta, len, m.fds[0].as_fd(), 0).map_err(|e| protocol(format!("mapping guest RAM: {e}")))?;
        self.mem = Some(Memory { ram: Arc::new(ram), file, delta, regions });
        Ok(())
    }

    /// Handles one message; returns the reply payload for messages that have one.
    pub fn handle(&mut self, m: &mut Msg) -> Result<Option<Vec<u8>>> {
        let u64_reply = |v: u64| Ok(Some(v.to_le_bytes().to_vec()));
        match m.request {
            req::GET_FEATURES => u64_reply(self.device().features() | F_VERSION_1 | F_PROTOCOL_FEATURES),
            req::SET_FEATURES => {
                self.features = Some(m.u64_at(0)? & !F_PROTOCOL_FEATURES);
                self.try_activate()?;
                Ok(None)
            }
            req::GET_PROTOCOL_FEATURES => u64_reply(SUPPORTED_PROTOCOL),
            req::SET_PROTOCOL_FEATURES => {
                self.protocol = m.u64_at(0)? & SUPPORTED_PROTOCOL;
                Ok(None)
            }
            req::GET_QUEUE_NUM => u64_reply(self.rings.len() as u64),
            req::SET_OWNER => Ok(None),
            req::RESET_OWNER | req::RESET_DEVICE => {
                self.deactivate();
                for r in &mut self.rings {
                    *r = Ring::default();
                }
                self.features = None;
                Ok(None)
            }
            req::SET_MEM_TABLE => {
                self.set_mem_table(m)?;
                self.try_activate()?;
                Ok(None)
            }
            req::SET_VRING_NUM => {
                let (i, num) = (m.u32_at(0)?, m.u32_at(4)?);
                let max = self.device().queue_max_size();
                if num == 0 || num > u32::from(max) || !num.is_power_of_two() {
                    return Err(protocol(format!("ring {i}: size {num} (max {max})")));
                }
                self.ring(u64::from(i))?.num = num as u16;
                Ok(None)
            }
            req::SET_VRING_ADDR => {
                let i = m.u32_at(0)?;
                let (desc, used, avail) = (m.u64_at(8)?, m.u64_at(16)?, m.u64_at(24)?);
                self.ring(u64::from(i))?.addr = Some((desc, used, avail));
                Ok(None)
            }
            req::SET_VRING_BASE => {
                let (i, base) = (m.u32_at(0)?, m.u32_at(4)?);
                self.ring(u64::from(i))?;
                if base != 0 {
                    return Err(protocol(format!("ring {i}: starting at index {base} (only fresh rings are supported)")));
                }
                Ok(None)
            }
            req::GET_VRING_BASE => {
                let i = m.u32_at(0)?;
                // The ring stops; the device with it (it runs all its queues or none).
                self.deactivate();
                let addr = self.ring(u64::from(i))?.addr;
                let used = match (&self.mem, addr) {
                    (Some(mem), Some((_, used, _))) => mem.gpa(used).and_then(|g| mem.ram.read_u16(g + 2).ok()).unwrap_or(0),
                    _ => 0,
                };
                let r = self.ring(u64::from(i))?;
                r.kick = None;
                let mut out = i.to_le_bytes().to_vec();
                out.extend_from_slice(&u32::from(used).to_le_bytes());
                Ok(Some(out))
            }
            req::SET_VRING_KICK | req::SET_VRING_CALL | req::SET_VRING_ERR => {
                let v = m.u64_at(0)?;
                let i = v & 0xff;
                if v & VRING_NOFD != 0 || m.fds.is_empty() {
                    return Err(protocol(format!("request {}: ring {i} without an eventfd (polling isn't supported)", m.request)));
                }
                let fd = m.fds.remove(0);
                self.ring(i)?;
                match m.request {
                    req::SET_VRING_KICK => {
                        // A new kick fd for a running device: restart it so the forwarder waits on it.
                        self.deactivate();
                        self.ring(i)?.kick = Some(fd);
                    }
                    req::SET_VRING_CALL => self.calls.write().unwrap_or_else(PoisonError::into_inner)[i as usize] = Some(fd),
                    _ => {} // errors are the device's to report; none are
                }
                self.try_activate()?;
                Ok(None)
            }
            req::SET_VRING_ENABLE => {
                let (i, on) = (m.u32_at(0)?, m.u32_at(4)?);
                self.ring(u64::from(i))?.enabled = on != 0;
                self.try_activate()?;
                Ok(None)
            }
            req::GET_CONFIG => {
                let (offset, size) = (m.u32_at(0)?, m.u32_at(4)? as usize);
                let mut out = m.payload.get(..12).ok_or_else(|| protocol("GET_CONFIG header"))?.to_vec();
                let mut data = vec![0u8; size.min(256)];
                self.device().read_config(u64::from(offset), &mut data);
                out.extend_from_slice(&data);
                Ok(Some(out))
            }
            req::SET_CONFIG => {
                let (offset, size) = (m.u32_at(0)?, m.u32_at(4)? as usize);
                let data = m.payload.get(12..12 + size).ok_or_else(|| protocol("SET_CONFIG data"))?.to_vec();
                self.device().write_config(u64::from(offset), &data);
                Ok(None)
            }
            other => Err(protocol(format!("request {other} isn't supported"))),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Waits on the kick eventfds and tells the device which queue was kicked, until `stop` fires.
fn forward_kicks(dev: &Mutex<Box<dyn VirtioDevice>>, kicks: &[(usize, OwnedFd)], stop: &OwnedFd) {
    crate::device_thread();
    let mut buf = [0u8; 8];
    loop {
        let mut fds: Vec<PollFd<'_>> =
            std::iter::once(PollFd::new(stop, PollFlags::IN)).chain(kicks.iter().map(|(_, fd)| PollFd::new(fd, PollFlags::IN))).collect();
        if poll(&mut fds, None).is_err() {
            continue;
        }
        if !fds[0].revents().is_empty() {
            return;
        }
        let kicked: Vec<usize> = fds[1..].iter().enumerate().filter(|(_, p)| !p.revents().is_empty()).map(|(k, _)| k).collect();
        drop(fds);
        for k in kicked {
            let (queue, fd) = &kicks[k];
            // Clears the count: several kicks since the last wakeup are one notify.
            let _ = rustix::io::read(fd, &mut buf);
            dev.lock().unwrap_or_else(PoisonError::into_inner).notify(*queue as u16);
        }
    }
}

/// Serves one frontend connection until it closes, then stops the device.
pub fn serve_connection(mut sock: UnixStream, dev: Arc<Mutex<Box<dyn VirtioDevice>>>) -> Result<()> {
    let mut s = Session::new(dev);
    while let Some(mut m) = read_msg(&mut sock)? {
        let reply = s.handle(&mut m);
        let need_ack = s.protocol & proto::REPLY_ACK != 0 && m.flags & FLAG_NEED_REPLY != 0;
        match reply {
            Ok(Some(payload)) => sock.write_all(&encode(m.request, &payload))?,
            Ok(None) if need_ack => sock.write_all(&encode(m.request, &0u64.to_le_bytes()))?,
            Ok(None) => {}
            Err(e) => {
                if need_ack {
                    let _ = sock.write_all(&encode(m.request, &1u64.to_le_bytes()));
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Listens on `path` (replacing a stale socket) and serves frontends one after another for as
/// long as the process lives: QEMU reconnects after a restart of its own. Errors end the
/// connection, not the listener.
pub fn serve(path: &Path, name: &str, dev: Box<dyn VirtioDevice>) -> std::io::Result<JoinHandle<()>> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let dev = Arc::new(Mutex::new(dev));
    let name = name.to_string();
    std::thread::Builder::new().name(format!("vhost-{name}")).spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { continue };
            if let Err(e) = serve_connection(sock, dev.clone()) {
                eprintln!("emu: vhost-user {name}: {e}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};

    use super::*;
    use crate::virtio::queue::tests::Driver;
    use crate::virtio::rng::Rng;

    /// The frontend's side: QEMU's messages, its guest RAM in a memfd at a fake address of its
    /// own (`UADDR`), and the eventfds it would wire to MSI-X.
    struct Frontend {
        sock: UnixStream,
        ram: GuestRam,
        memfd: OwnedFd,
    }

    const RAM: u64 = 0x40_0000;
    const UADDR: u64 = 0x7f00_0000_0000;

    impl Frontend {
        fn new() -> (Frontend, UnixStream) {
            let (a, b) = UnixStream::pair().expect("pair");
            let memfd = rustix::fs::memfd_create("guest", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
            rustix::fs::ftruncate(&memfd, RAM).expect("size");
            let ram = GuestRam::map_shared(0, RAM, memfd.as_fd(), 0).expect("map");
            (Frontend { sock: a, ram, memfd }, b)
        }

        fn send(&mut self, request: u32, flags: u32, payload: &[u8], fds: &[&OwnedFd]) {
            let mut msg = Vec::new();
            msg.extend_from_slice(&request.to_le_bytes());
            msg.extend_from_slice(&(VERSION | flags).to_le_bytes());
            msg.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            msg.extend_from_slice(payload);
            let borrowed: Vec<_> = fds.iter().map(|f| f.as_fd()).collect();
            let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            if !borrowed.is_empty() {
                assert!(control.push(SendAncillaryMessage::ScmRights(&borrowed)));
            }
            sendmsg(&self.sock, &[std::io::IoSlice::new(&msg)], &mut control, SendFlags::empty()).expect("sendmsg");
        }

        fn reply(&mut self, request: u32) -> Vec<u8> {
            let mut head = [0u8; HEADER];
            self.sock.read_exact(&mut head).expect("reply header");
            assert_eq!(u32::from_le_bytes(head[0..4].try_into().unwrap_or_default()), request);
            assert_eq!(u32::from_le_bytes(head[4..8].try_into().unwrap_or_default()), VERSION | FLAG_REPLY);
            let mut p = vec![0u8; u32::from_le_bytes(head[8..12].try_into().unwrap_or_default()) as usize];
            self.sock.read_exact(&mut p).expect("reply payload");
            p
        }

        fn u64_req(&mut self, request: u32, v: u64) {
            self.send(request, 0, &v.to_le_bytes(), &[]);
        }

        fn state(&mut self, request: u32, index: u32, num: u32) {
            let mut p = index.to_le_bytes().to_vec();
            p.extend_from_slice(&num.to_le_bytes());
            self.send(request, 0, &p, &[]);
        }

        /// QEMU's `pc` memory table: RAM split around the legacy VGA hole, one fd.
        fn mem_table(&mut self) {
            let mut p = 2u32.to_le_bytes().to_vec();
            p.extend_from_slice(&[0; 4]);
            for (gpa, size) in [(0u64, 0xa_0000u64), (0xc_0000, RAM - 0xc_0000)] {
                for v in [gpa, size, UADDR + gpa, gpa] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
            }
            let dup = self.memfd.try_clone().expect("dup");
            let fd = self.memfd.try_clone().expect("dup");
            self.send(req::SET_MEM_TABLE, FLAG_NEED_REPLY, &p, &[&fd, &dup]);
            assert_eq!(self.reply(req::SET_MEM_TABLE), 0u64.to_le_bytes(), "acked");
        }

        /// Sets up ring 0 at `d`'s guest addresses, given as frontend addresses.
        fn ring(&mut self, d: &Driver, kick: &OwnedFd, call: &OwnedFd) {
            self.state(req::SET_VRING_NUM, 0, u32::from(d.cfg.size));
            self.state(req::SET_VRING_BASE, 0, 0);
            let mut p = 0u32.to_le_bytes().to_vec();
            p.extend_from_slice(&0u32.to_le_bytes());
            for a in [d.cfg.desc, d.cfg.used, d.cfg.avail, 0] {
                p.extend_from_slice(&(UADDR + a).to_le_bytes());
            }
            self.send(req::SET_VRING_ADDR, 0, &p, &[]);
            self.send(req::SET_VRING_KICK, 0, &0u64.to_le_bytes(), &[kick]);
            self.send(req::SET_VRING_CALL, 0, &0u64.to_le_bytes(), &[call]);
        }
    }

    fn rng_backend(sock: UnixStream) -> JoinHandle<Result<()>> {
        let dev: Box<dyn VirtioDevice> = Box::new(Rng::new());
        std::thread::spawn(move || serve_connection(sock, Arc::new(Mutex::new(dev))))
    }

    fn wait_readable(fd: &OwnedFd) -> bool {
        let mut p = [PollFd::new(fd, PollFlags::IN)];
        let t = rustix::event::Timespec { tv_sec: 5, tv_nsec: 0 };
        poll(&mut p, Some(&t)).is_ok_and(|n| n == 1)
    }

    #[test]
    fn features_and_config_are_the_devices() {
        let (mut fe, be) = Frontend::new();
        let t = rng_backend(be);
        fe.send(req::GET_FEATURES, 0, &[], &[]);
        let f = u64::from_le_bytes(fe.reply(req::GET_FEATURES).try_into().unwrap_or_default());
        assert_eq!(f & (F_VERSION_1 | F_PROTOCOL_FEATURES), F_VERSION_1 | F_PROTOCOL_FEATURES);
        fe.send(req::GET_PROTOCOL_FEATURES, 0, &[], &[]);
        let p = u64::from_le_bytes(fe.reply(req::GET_PROTOCOL_FEATURES).try_into().unwrap_or_default());
        assert_eq!(p, SUPPORTED_PROTOCOL);
        fe.send(req::GET_QUEUE_NUM, 0, &[], &[]);
        assert_eq!(fe.reply(req::GET_QUEUE_NUM), 1u64.to_le_bytes());
        drop(fe);
        t.join().expect("join").expect("clean end");
    }

    /// The whole path QEMU drives: memory table, ring setup, enable; then a guest buffer, a kick,
    /// the device filling it in the shared RAM, and a call.
    #[test]
    fn a_kicked_request_completes_into_shared_ram_and_calls() {
        let (mut fe, be) = Frontend::new();
        let t = rng_backend(be);
        fe.u64_req(req::SET_PROTOCOL_FEATURES, proto::REPLY_ACK);
        fe.send(req::SET_OWNER, 0, &[], &[]);
        fe.u64_req(req::SET_FEATURES, F_VERSION_1);
        fe.mem_table();
        let mut d = Driver::at(0x10_0000, 8);
        let (kick, call) = (eventfd(0, EventfdFlags::CLOEXEC).expect("kick"), eventfd(0, EventfdFlags::CLOEXEC).expect("call"));
        fe.ring(&d, &kick, &call);
        fe.state(req::SET_VRING_ENABLE, 0, 1);
        // A writable buffer in the guest, published and kicked.
        d.push(&fe.ram, &[(0x20_0000, 64, true)]);
        rustix::io::write(&kick, &1u64.to_ne_bytes()).expect("kick");
        assert!(wait_readable(&call), "the device called");
        assert_eq!(d.used_idx(&fe.ram), 1);
        assert_eq!(d.used(&fe.ram, 0), (0, 64), "head 0 filled with 64 bytes");
        let mut got = [0u8; 64];
        fe.ram.read(0x20_0000, &mut got).expect("r");
        assert!(got.iter().any(|&b| b != 0), "random bytes landed in the frontend's RAM");
        // Stopping the ring returns where it got to.
        fe.state(req::GET_VRING_BASE, 0, 0);
        let r = fe.reply(req::GET_VRING_BASE);
        assert_eq!((&r[0..4], &r[4..8]), (&0u32.to_le_bytes()[..], &1u32.to_le_bytes()[..]));
        drop(fe);
        t.join().expect("join").expect("clean end");
    }

    #[test]
    fn unsupported_setups_are_refused_with_a_reason() {
        let (mut fe, be) = Frontend::new();
        let t = rng_backend(be);
        fe.u64_req(req::SET_PROTOCOL_FEATURES, proto::REPLY_ACK);
        fe.send(req::SET_VRING_BASE, FLAG_NEED_REPLY, &[0, 0, 0, 0, 5, 0, 0, 0], &[]);
        assert_eq!(fe.reply(req::SET_VRING_BASE), 1u64.to_le_bytes(), "a nack before the connection ends");
        let err = t.join().expect("join").expect_err("refused");
        assert!(err.to_string().contains("starting at index 5"), "{err}");
    }

    /// Only queues with new used entries are called: before, every raise called every queue.
    #[test]
    fn a_raise_calls_only_the_queues_whose_used_ring_moved() {
        let mem = Arc::new(GuestRam::new(0, 0x10_0000).expect("ram"));
        let efd = || eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd");
        let calls: Calls = Arc::new(RwLock::new(vec![Some(efd()), Some(efd()), None]));
        let used = vec![Some((0x1002, AtomicU16::new(0))), Some((0x2002, AtomicU16::new(0))), None];
        let line = CallLine { calls: calls.clone(), mem: mem.clone(), used };
        let pending = |q: usize| {
            let c = calls.read().unwrap_or_else(PoisonError::into_inner);
            let mut b = [0u8; 8];
            c[q].as_ref().is_some_and(|fd| rustix::io::read(fd, &mut b).is_ok())
        };
        mem.write_u16(0x1002, 1).expect("w");
        line.set(true);
        assert_eq!((pending(0), pending(1)), (true, false), "queue 1 had nothing new");
        line.set(true);
        assert!(!pending(0), "nothing moved since: no call");
        mem.write_u16(0x2002, 3).expect("w");
        mem.write_u16(0x1002, 2).expect("w");
        line.set(true);
        assert_eq!((pending(0), pending(1)), (true, true));
        line.set(false);
        assert_eq!((pending(0), pending(1)), (false, false), "a lowered line is no call");
    }

    #[test]
    fn ring_addresses_translate_through_the_memory_table() {
        let mem = Memory {
            ram: Arc::new(GuestRam::new(0, 0x10_0000).expect("ram")),
            file: (0, 0),
            delta: 0,
            regions: vec![
                Region { gpa: 0, size: 0xa_0000, uaddr: UADDR },
                Region { gpa: 0xc_0000, size: 0x4_0000, uaddr: UADDR + 0xc_0000 },
            ],
        };
        assert_eq!(mem.gpa(UADDR + 0x1000), Some(0x1000));
        assert_eq!(mem.gpa(UADDR + 0xc_1234), Some(0xc_1234));
        assert_eq!(mem.gpa(UADDR + 0xb_0000), None, "the hole");
        assert_eq!(mem.gpa(UADDR + 0x10_0000), None, "past the end");
    }
}
