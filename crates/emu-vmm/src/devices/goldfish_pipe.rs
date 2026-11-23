//! Goldfish pipe device, v2 ("google,android-pipe", `/dev/goldfish_pipe_dprctd` in the guest).
//!
//! The second transport for host pipe services (vsock is the first). Graphics-related pipes
//! (`refcount`, `GLProcessPipe`, `opengles`) only ever use this one. Each guest pipe has a
//! command page; the guest writes the pipe id to `CMD` and the command in that page says what
//! to do. Host-initiated wakeups are queued as (id, flags) and handed over on `GET_SIGNALLED`.
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use emu_hvf::GuestRam;

use crate::bus::{MmioDevice, le_value, set_le};
use crate::irq::IrqLine;
use crate::pipe::{HostMsg, PipeCtx, PipeHandler, PipeRegistry, WorkerEvent};

const REG_CMD: u64 = 0;
const REG_SIGNAL_BUFFER_HIGH: u64 = 4;
const REG_SIGNAL_BUFFER: u64 = 8;
const REG_SIGNAL_BUFFER_COUNT: u64 = 12;
const REG_OPEN_BUFFER_HIGH: u64 = 20;
const REG_OPEN_BUFFER: u64 = 24;
const REG_VERSION: u64 = 36;
const REG_GET_SIGNALLED: u64 = 48;

const DEVICE_VERSION: u32 = 2;

const CMD_OPEN: i32 = 1;
const CMD_CLOSE: i32 = 2;
const CMD_POLL: i32 = 3;
const CMD_WRITE: i32 = 4;
const CMD_WAKE_ON_WRITE: i32 = 5;
const CMD_READ: i32 = 6;
const CMD_WAKE_ON_READ: i32 = 7;

const POLL_IN: i32 = 1;
const POLL_OUT: i32 = 2;
const POLL_HUP: i32 = 4;
const ERR_INVAL: i32 = -1;
const ERR_AGAIN: i32 = -2;
const ERR_IO: i32 = -4;

const WAKE_CLOSED: u32 = 1;
const WAKE_READ: u32 = 2;
const WAKE_WRITE: u32 = 4;

const MAX_BUFFERS: usize = 336;
/// Offsets in the per-pipe command page.
const CB_CMD: u64 = 0;
const CB_STATUS: u64 = 8;
const CB_BUFFERS_COUNT: u64 = 16;
const CB_CONSUMED: u64 = 20;
const CB_PTRS: u64 = 24;
const CB_SIZES: u64 = CB_PTRS + 8 * MAX_BUFFERS as u64;

struct Pipe {
    command: u64,
    /// Collecting `pipe:<name>\0` until the handler exists.
    name: Option<Vec<u8>>,
    handler: Option<Box<dyn PipeHandler>>,
    inbound: VecDeque<u8>,
    wants_read: bool,
    closed_by_host: bool,
}

#[derive(Default)]
struct State {
    signal_buffer: u64,
    signal_count: u32,
    open_buffer: u64,
    regs: HashMap<u64, u32>,
    pipes: HashMap<u32, Pipe>,
    /// (pipe id, wake flags) waiting for GET_SIGNALLED.
    signalled: VecDeque<(u32, u32)>,
}

pub struct GoldfishPipe {
    state: Arc<Mutex<State>>,
    mem: Arc<GuestRam>,
    registry: Arc<PipeRegistry>,
    irq: Arc<dyn IrqLine>,
    events: Sender<WorkerEvent>,
}

impl GoldfishPipe {
    pub fn new(mem: Arc<GuestRam>, registry: Arc<PipeRegistry>, irq: Arc<dyn IrqLine>) -> Arc<GoldfishPipe> {
        let (events, rx) = channel();
        let dev = Arc::new(GoldfishPipe { state: Arc::new(Mutex::new(State::default())), mem, registry, irq, events });
        let weak = Arc::downgrade(&dev);
        let _ = std::thread::Builder::new().name("goldfish-pipe".into()).spawn(move || host_loop(&weak, &rx));
        dev
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn signal(&self, s: &mut State, id: u32, flags: u32) {
        if let Some(e) = s.signalled.iter_mut().find(|(i, _)| *i == id) {
            e.1 |= flags;
        } else {
            s.signalled.push_back((id, flags));
        }
        self.irq.set(true);
    }

    fn rd64(&self, addr: u64) -> u64 {
        self.mem.read_u64(addr).unwrap_or(0)
    }

    fn rd32(&self, addr: u64) -> u32 {
        self.mem.read_u32(addr).unwrap_or(0)
    }

    fn wr32(&self, addr: u64, v: i32) {
        let _ = self.mem.write(addr, &v.to_le_bytes());
    }

    fn buffers(&self, cb: u64) -> Vec<(u64, usize)> {
        let n = (self.rd32(cb + CB_BUFFERS_COUNT) as usize).min(MAX_BUFFERS);
        (0..n).map(|i| (self.rd64(cb + CB_PTRS + 8 * i as u64), self.rd32(cb + CB_SIZES + 4 * i as u64) as usize)).collect()
    }

    fn command(&self, s: &mut State, id: u32) {
        // OPEN: the command page address arrives in the open buffer.
        let cb = match s.pipes.get(&id) {
            Some(p) => p.command,
            None => {
                let cb = self.rd64(s.open_buffer);
                if cb == 0 {
                    return;
                }
                cb
            }
        };
        let cmd = self.rd32(cb + CB_CMD) as i32;
        let (status, consumed) = match cmd {
            CMD_OPEN => {
                s.pipes.insert(
                    id,
                    Pipe {
                        command: cb,
                        name: Some(Vec::new()),
                        handler: None,
                        inbound: VecDeque::new(),
                        wants_read: false,
                        closed_by_host: false,
                    },
                );
                (0, 0)
            }
            CMD_CLOSE => {
                if let Some(mut p) = s.pipes.remove(&id)
                    && let Some(h) = p.handler.as_mut()
                {
                    h.closed();
                }
                s.signalled.retain(|(i, _)| *i != id);
                (0, 0)
            }
            CMD_POLL => match s.pipes.get(&id) {
                Some(p) => {
                    let mut st = POLL_OUT;
                    if !p.inbound.is_empty() {
                        st |= POLL_IN;
                    }
                    if p.closed_by_host {
                        st |= POLL_HUP;
                    }
                    (st, 0)
                }
                None => (ERR_INVAL, 0),
            },
            CMD_WRITE => self.pipe_write(s, id, cb),
            CMD_READ => self.pipe_read(s, id, cb),
            CMD_WAKE_ON_WRITE => {
                // Writes never block: wake the writer right away.
                self.signal(s, id, WAKE_WRITE);
                (0, 0)
            }
            CMD_WAKE_ON_READ => match s.pipes.get_mut(&id) {
                Some(p) if !p.inbound.is_empty() || p.closed_by_host => {
                    let flags = if p.closed_by_host { WAKE_CLOSED } else { WAKE_READ };
                    self.signal(s, id, flags);
                    (0, 0)
                }
                Some(p) => {
                    p.wants_read = true;
                    (0, 0)
                }
                None => (ERR_INVAL, 0),
            },
            _ => (ERR_INVAL, 0),
        };
        self.wr32(cb + CB_STATUS, status);
        self.wr32(cb + CB_CONSUMED, consumed);
    }

    fn pipe_write(&self, s: &mut State, id: u32, cb: u64) -> (i32, i32) {
        let mut data = Vec::new();
        for (addr, len) in self.buffers(cb) {
            let start = data.len();
            data.resize(start + len, 0);
            if self.mem.read(addr, &mut data[start..]).is_err() {
                return (ERR_INVAL, 0);
            }
        }
        let total = data.len() as i32;
        let Some(p) = s.pipes.get_mut(&id) else { return (ERR_INVAL, 0) };
        if p.closed_by_host {
            return (ERR_IO, 0);
        }
        let mut rest: &[u8] = &data;
        if let Some(name) = p.name.as_mut() {
            match rest.iter().position(|&b| b == 0) {
                None => {
                    name.extend_from_slice(rest);
                    return (total, total);
                }
                Some(nul) => {
                    name.extend_from_slice(&rest[..nul]);
                    rest = &rest[nul + 1..];
                    let raw = p.name.take().unwrap_or_default();
                    let full = String::from_utf8_lossy(&raw).into_owned();
                    let name = full.strip_prefix("pipe:").unwrap_or(&full).to_string();
                    match self.registry.open(&name, PipeCtx { conn: u64::from(id), events: self.events.clone() }) {
                        Some(h) => p.handler = Some(h),
                        None => {
                            eprintln!("emu: guest opened unknown goldfish pipe '{name}'");
                            return (ERR_IO, 0);
                        }
                    }
                }
            }
        }
        if !rest.is_empty()
            && let Some(h) = p.handler.as_mut()
        {
            let mut out = Vec::new();
            let keep = h.guest_data(rest, &mut out);
            p.inbound.extend(out);
            if !keep {
                p.closed_by_host = true;
            }
        }
        if !p.inbound.is_empty() && p.wants_read {
            p.wants_read = false;
            self.signal(s, id, WAKE_READ);
        }
        (total, total)
    }

    fn pipe_read(&self, s: &mut State, id: u32, cb: u64) -> (i32, i32) {
        let bufs = self.buffers(cb);
        let Some(p) = s.pipes.get_mut(&id) else { return (ERR_INVAL, 0) };
        if p.inbound.is_empty() {
            return if p.closed_by_host { (0, 0) } else { (ERR_AGAIN, 0) };
        }
        let mut done = 0usize;
        for (addr, len) in bufs {
            let n = len.min(p.inbound.len());
            if n == 0 {
                break;
            }
            let chunk: Vec<u8> = p.inbound.drain(..n).collect();
            if self.mem.write(addr, &chunk).is_err() {
                return (ERR_INVAL, done as i32);
            }
            done += n;
        }
        (done as i32, done as i32)
    }
}

/// Applies host-side messages (e.g. data from a host socket) to their pipes.
fn host_loop(dev: &std::sync::Weak<GoldfishPipe>, rx: &Receiver<WorkerEvent>) {
    while let Ok(ev) = rx.recv() {
        let Some(dev) = dev.upgrade() else { return };
        let WorkerEvent::Host { conn, msg } = ev else { continue };
        let id = conn as u32;
        let mut s = dev.lock();
        let Some(p) = s.pipes.get_mut(&id) else { continue };
        let Some(h) = p.handler.as_mut() else { continue };
        let mut out = Vec::new();
        let keep = h.host_msg(msg, &mut out);
        p.inbound.extend(out);
        let mut flags = 0;
        if !keep {
            p.closed_by_host = true;
            flags |= WAKE_CLOSED;
        }
        if !p.inbound.is_empty() && p.wants_read {
            p.wants_read = false;
            flags |= WAKE_READ;
        }
        if flags != 0 {
            dev.signal(&mut s, id, flags);
        }
    }
}

impl MmioDevice for GoldfishPipe {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut s = self.lock();
        let v = match offset {
            REG_VERSION => DEVICE_VERSION,
            REG_GET_SIGNALLED => {
                let n = (s.signal_count as usize).min(s.signalled.len()).min(64);
                for i in 0..n {
                    if let Some((id, flags)) = s.signalled.pop_front() {
                        let at = s.signal_buffer + 8 * i as u64;
                        let mut b = [0u8; 8];
                        b[..4].copy_from_slice(&id.to_le_bytes());
                        b[4..].copy_from_slice(&flags.to_le_bytes());
                        let _ = self.mem.write(at, &b);
                    }
                }
                if s.signalled.is_empty() {
                    self.irq.set(false);
                }
                n as u32
            }
            _ => s.regs.get(&offset).copied().unwrap_or(0),
        };
        set_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_value(data) as u32;
        let mut s = self.lock();
        s.regs.insert(offset, v);
        let hi = |s: &State, r| u64::from(s.regs.get(&r).copied().unwrap_or(0)) << 32;
        match offset {
            REG_CMD => self.command(&mut s, v),
            REG_SIGNAL_BUFFER => s.signal_buffer = hi(&s, REG_SIGNAL_BUFFER_HIGH) | u64::from(v),
            REG_SIGNAL_BUFFER_COUNT => s.signal_count = v,
            REG_OPEN_BUFFER => s.open_buffer = hi(&s, REG_OPEN_BUFFER_HIGH) | u64::from(v),
            _ => {}
        }
    }
}

/// Called with a color buffer handle when its last guest reference goes away.
pub type OnLastRef = Arc<dyn Fn(u32) + Send + Sync>;

/// `refcount`: the guest allocator opens one per color buffer and writes the 4-byte host handle;
/// the buffer lives until every guest fd sharing that pipe is closed (buffers cross processes as
/// fds). With the renderer's RefCountPipe feature on, this is the only thing that frees color
/// buffers — explicit closes from consumers don't.
#[derive(Default)]
pub struct RefCount {
    pub on_last_ref: Option<OnLastRef>,
}

impl crate::pipe::PipeService for RefCount {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "refcount").then(|| Box::new(RefPipe { buf: Vec::new(), handle: 0, on_last_ref: self.on_last_ref.clone() }) as _)
    }
}

struct RefPipe {
    buf: Vec<u8>,
    handle: u32,
    on_last_ref: Option<OnLastRef>,
}

impl PipeHandler for RefPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= 4 {
            let h: Vec<u8> = self.buf.drain(..4).collect();
            self.handle = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        }
        true
    }
    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
    fn closed(&mut self) {
        if let (h @ 1.., Some(cb)) = (std::mem::take(&mut self.handle), &self.on_last_ref) {
            cb(h);
        }
    }
}

/// `GLProcessPipe`: each GL-using guest process writes a 4-byte confirm word and reads back a
/// unique 64-bit process id.
pub struct GlProcess {
    next: std::sync::atomic::AtomicU64,
}

impl Default for GlProcess {
    fn default() -> Self {
        GlProcess { next: std::sync::atomic::AtomicU64::new(1) }
    }
}

impl crate::pipe::PipeService for GlProcess {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "GLProcessPipe").then(|| {
            let puid = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(ProcessPipe { puid, got: 0 }) as _
        })
    }
}

struct ProcessPipe {
    puid: u64,
    got: usize,
}

impl PipeHandler for ProcessPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        let before = self.got;
        self.got += data.len();
        if before < 4 && self.got >= 4 {
            to_guest.extend_from_slice(&self.puid.to_le_bytes());
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
    use crate::irq::TestLine;

    const RAM: u64 = 0x4000_0000;

    fn wr(d: &GoldfishPipe, r: u64, v: u32) {
        d.write(r, &v.to_le_bytes());
    }

    fn rd(d: &GoldfishPipe, r: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(r, &mut b);
        u32::from_le_bytes(b)
    }

    /// Issues `cmd` on pipe `id` whose command page is at `cb`, with one buffer.
    fn run(d: &GoldfishPipe, mem: &GuestRam, id: u32, cb: u64, cmd: i32, buf: Option<(u64, u32)>) -> (i32, i32) {
        mem.write(cb + CB_CMD, &cmd.to_le_bytes()).expect("w");
        if let Some((addr, len)) = buf {
            mem.write(cb + CB_BUFFERS_COUNT, &1u32.to_le_bytes()).expect("w");
            mem.write(cb + CB_PTRS, &addr.to_le_bytes()).expect("w");
            mem.write(cb + CB_SIZES, &len.to_le_bytes()).expect("w");
        }
        wr(d, REG_CMD, id);
        (mem.read_u32(cb + CB_STATUS).expect("r") as i32, mem.read_u32(cb + CB_CONSUMED).expect("r") as i32)
    }

    #[test]
    fn open_name_write_read_and_close() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let mut reg = PipeRegistry::default();
        reg.register("GLProcessPipe", Box::new(GlProcess::default()));
        reg.register("refcount", Box::new(RefCount::default()));
        let d = GoldfishPipe::new(mem.clone(), Arc::new(reg), line.clone());
        assert_eq!(
            {
                wr(&d, REG_VERSION, 4);
                rd(&d, REG_VERSION)
            },
            DEVICE_VERSION
        );
        let (open_buf, sig_buf, cb) = (RAM + 0x1000, RAM + 0x2000, RAM + 0x3000);
        wr(&d, REG_OPEN_BUFFER_HIGH, 0);
        wr(&d, REG_OPEN_BUFFER, open_buf as u32);
        wr(&d, REG_SIGNAL_BUFFER_HIGH, 0);
        wr(&d, REG_SIGNAL_BUFFER, sig_buf as u32);
        wr(&d, REG_SIGNAL_BUFFER_COUNT, 64);
        mem.write(open_buf, &cb.to_le_bytes()).expect("w");
        assert_eq!(run(&d, &mem, 3, cb, CMD_OPEN, None).0, 0);

        let data = RAM + 0x8000;
        mem.write(data, b"pipe:GLProcessPipe\0").expect("w");
        assert_eq!(run(&d, &mem, 3, cb, CMD_WRITE, Some((data, 19))), (19, 19));
        assert_eq!(run(&d, &mem, 3, cb, CMD_READ, Some((data, 8))).0, ERR_AGAIN, "nothing yet");
        assert_eq!(run(&d, &mem, 3, cb, CMD_WAKE_ON_READ, None).0, 0);
        mem.write(data, &100u32.to_le_bytes()).expect("w");
        assert_eq!(run(&d, &mem, 3, cb, CMD_WRITE, Some((data, 4))), (4, 4));
        assert!(line.level(), "reader woken");
        assert_eq!(rd(&d, REG_GET_SIGNALLED), 1);
        assert_eq!(mem.read_u32(sig_buf).expect("id"), 3);
        assert_eq!(mem.read_u32(sig_buf + 4).expect("flags"), WAKE_READ);
        assert!(!line.level(), "drained");
        assert_eq!(run(&d, &mem, 3, cb, CMD_READ, Some((data + 0x100, 8))), (8, 8));
        assert_eq!(mem.read_u64(data + 0x100).expect("puid"), 1);
        assert_eq!(run(&d, &mem, 3, cb, CMD_POLL, None).0, POLL_OUT);
        assert_eq!(run(&d, &mem, 3, cb, CMD_CLOSE, None).0, 0);
        assert_eq!(run(&d, &mem, 3, cb, CMD_POLL, None).0, ERR_INVAL, "closed");
    }
}
