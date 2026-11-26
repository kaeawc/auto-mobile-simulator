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
    /// The id the guest named it by, and which open of that id this is (the driver reuses
    /// freed ids): host threads address it by this, so a closed pipe's late messages find no
    /// pipe instead of the next one with its id.
    conn: u64,
    command: u64,
    /// Collecting `pipe:<name>\0` until the handler exists.
    name: Option<Vec<u8>>,
    handler: Option<Box<dyn PipeHandler>>,
    inbound: VecDeque<u8>,
    wants_read: bool,
    /// Asked to be woken when its handler, full, can take guest bytes again.
    wants_write: bool,
    closed_by_host: bool,
}

#[derive(Default)]
struct State {
    signal_buffer: u64,
    signal_count: u32,
    open_buffer: u64,
    regs: HashMap<u64, u32>,
    pipes: HashMap<u32, Pipe>,
    /// Opens so far, the high half of each pipe's `conn`.
    opens: u32,
    /// (pipe id, wake flags) waiting for GET_SIGNALLED.
    signalled: VecDeque<(u32, u32)>,
    /// The level last driven on the line (it starts low): the controller hears changes only.
    level: bool,
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
        let _ = std::thread::Builder::new().name("goldfish-pipe".into()).spawn(move || {
            crate::device_thread();
            host_loop(&weak, &rx)
        });
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
        // Already up: the driver drains every signal before the line drops.
        if !s.level {
            s.level = true;
            self.irq.set(true);
        }
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

    /// The command's (address, length) buffers: its pointer and size arrays read in one go each,
    /// not two guest reads per buffer.
    fn buffers(&self, cb: u64) -> Vec<(u64, usize)> {
        let n = (self.rd32(cb + CB_BUFFERS_COUNT) as usize).min(MAX_BUFFERS);
        let (mut ptrs, mut sizes) = (vec![0u8; 8 * n], vec![0u8; 4 * n]);
        if self.mem.read(cb + CB_PTRS, &mut ptrs).is_err() || self.mem.read(cb + CB_SIZES, &mut sizes).is_err() {
            return Vec::new();
        }
        let ptrs = ptrs.as_chunks::<8>().0.iter().map(|p| u64::from_le_bytes(*p));
        let sizes = sizes.as_chunks::<4>().0.iter().map(|z| u32::from_le_bytes(*z) as usize);
        ptrs.zip(sizes).collect()
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
                s.opens = s.opens.wrapping_add(1);
                let conn = u64::from(s.opens) << 32 | u64::from(id);
                s.pipes.insert(
                    id,
                    Pipe {
                        conn,
                        command: cb,
                        name: Some(Vec::new()),
                        handler: None,
                        inbound: VecDeque::new(),
                        wants_read: false,
                        wants_write: false,
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
            CMD_WAKE_ON_WRITE => match s.pipes.get_mut(&id) {
                // Woken by the handler's `drained` once it has room.
                Some(p) if p.handler.as_ref().is_some_and(|h| h.write_blocked()) => {
                    p.wants_write = true;
                    (0, 0)
                }
                _ => {
                    self.signal(s, id, WAKE_WRITE);
                    (0, 0)
                }
            },
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
        let bufs = self.buffers(cb);
        let mut data = Vec::with_capacity(bufs.iter().map(|&(_, len)| len).sum());
        for (addr, len) in bufs {
            if self.mem.read_append(addr, len, &mut data).is_err() {
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
                    match self.registry.open(&name, PipeCtx { conn: p.conn, events: self.events.clone() }) {
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
            // Full (a render thread far behind): "try again", never a wait under the device's
            // lock. The guest asks to be woken (WAKE_ON_WRITE) and writes the rest then.
            if h.write_blocked() {
                let taken = (data.len() - rest.len()) as i32;
                return if taken > 0 { (taken, taken) } else { (ERR_AGAIN, 0) };
            }
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
            // Straight from the queue's (at most two) slices.
            let (front, back) = p.inbound.as_slices();
            let (a, b) = if n <= front.len() { (&front[..n], &[][..]) } else { (front, &back[..n - front.len()]) };
            if self.mem.write(addr, a).is_err() || (!b.is_empty() && self.mem.write(addr + a.len() as u64, b).is_err()) {
                return (ERR_INVAL, done as i32);
            }
            p.inbound.drain(..n);
            done += n;
        }
        if let Some(h) = p.handler.as_mut() {
            h.guest_took(done);
        }
        (done as i32, done as i32)
    }
}

/// Applies host-side messages (e.g. data from a host socket) to their pipes.
fn host_loop(dev: &std::sync::Weak<GoldfishPipe>, rx: &Receiver<WorkerEvent>) {
    while let Ok(ev) = rx.recv() {
        let Some(dev) = dev.upgrade() else { return };
        let (conn, msg) = match ev {
            WorkerEvent::Host { conn, msg } => (conn, Some(msg)),
            WorkerEvent::Drained { conn } => (conn, None),
            _ => continue,
        };
        let id = conn as u32;
        let mut s = dev.lock();
        // Gone, or closed and its id given to a newer pipe.
        let Some(p) = s.pipes.get_mut(&id).filter(|p| p.conn == conn) else { continue };
        let Some(h) = p.handler.as_mut() else { continue };
        let Some(msg) = msg else {
            // Room again for a full handler's writer.
            if p.wants_write && !h.write_blocked() {
                p.wants_write = false;
                dev.signal(&mut s, id, WAKE_WRITE);
            }
            continue;
        };
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
                if s.signalled.is_empty() && s.level {
                    s.level = false;
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
/// unique 64-bit process id. With a backend (images whose GLES goes over the `opengles` pipe),
/// the renderer hears of each process when its pipe opens and again when it closes (the process
/// exited), to set up and free that process's GL state, as the stock emulator's pipe does.
pub struct GlProcess {
    next: &'static std::sync::atomic::AtomicU64,
    backend: Option<Arc<dyn crate::pipe::opengles::GlBackend>>,
}

/// Process ids for the whole emu process, as stock's are: the renderer keeps each process's GL
/// state (and may still be cleaning one up) across guest resets, which rebuild the pipes.
static PROCESS_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Default for GlProcess {
    fn default() -> Self {
        GlProcess::with_backend(None)
    }
}

impl GlProcess {
    pub fn with_backend(backend: Option<Arc<dyn crate::pipe::opengles::GlBackend>>) -> GlProcess {
        GlProcess { next: &PROCESS_IDS, backend }
    }

    /// Ids from 1, of its own (tests that check the numbers).
    #[cfg(test)]
    fn numbered_from_one(backend: Option<Arc<dyn crate::pipe::opengles::GlBackend>>) -> GlProcess {
        GlProcess { next: Box::leak(Box::new(std::sync::atomic::AtomicU64::new(1))), backend }
    }
}

impl crate::pipe::PipeService for GlProcess {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "GLProcessPipe").then(|| {
            let puid = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(b) = &self.backend {
                b.process_created(puid);
            }
            Box::new(ProcessPipe { puid, got: 0, backend: self.backend.clone() }) as _
        })
    }
}

struct ProcessPipe {
    puid: u64,
    got: usize,
    backend: Option<Arc<dyn crate::pipe::opengles::GlBackend>>,
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
    fn closed(&mut self) {
        if let Some(b) = self.backend.take() {
            b.process_dead(self.puid);
        }
    }
}

/// A guest reset drops pipes without closing them: their processes are gone all the same.
impl Drop for ProcessPipe {
    fn drop(&mut self) {
        PipeHandler::closed(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::{CountLine, TestLine};

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
        reg.register("GLProcessPipe", Box::new(GlProcess::numbered_from_one(None)));
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

    /// Two pipes woken before the driver drains them, then a drain with nothing left: the
    /// controller hears one rise and one fall.
    #[test]
    fn drives_the_line_only_on_changes() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let line = Arc::new(CountLine::default());
        let mut reg = PipeRegistry::default();
        reg.register("GLProcessPipe", Box::new(GlProcess::default()));
        let d = GoldfishPipe::new(mem.clone(), Arc::new(reg), line.clone());
        wr(&d, REG_VERSION, 4);
        let (open_buf, sig_buf, data) = (RAM + 0x1000, RAM + 0x2000, RAM + 0x8000);
        wr(&d, REG_OPEN_BUFFER_HIGH, 0);
        wr(&d, REG_OPEN_BUFFER, open_buf as u32);
        wr(&d, REG_SIGNAL_BUFFER_HIGH, 0);
        wr(&d, REG_SIGNAL_BUFFER, sig_buf as u32);
        wr(&d, REG_SIGNAL_BUFFER_COUNT, 64);
        for (id, cb) in [(3, RAM + 0x3000), (4, RAM + 0x4000)] {
            mem.write(open_buf, &cb.to_le_bytes()).expect("w");
            assert_eq!(run(&d, &mem, id, cb, CMD_OPEN, None).0, 0);
            mem.write(data, b"pipe:GLProcessPipe\0").expect("w");
            assert_eq!(run(&d, &mem, id, cb, CMD_WRITE, Some((data, 19))).0, 19);
            assert_eq!(run(&d, &mem, id, cb, CMD_WAKE_ON_READ, None).0, 0);
            mem.write(data, &100u32.to_le_bytes()).expect("w");
            assert_eq!(run(&d, &mem, id, cb, CMD_WRITE, Some((data, 4))).0, 4);
            assert!(line.level(), "pipe {id} woken");
        }
        assert_eq!(rd(&d, REG_GET_SIGNALLED), 2);
        assert!(!line.level(), "drained");
        assert_eq!(rd(&d, REG_GET_SIGNALLED), 0);
        assert_eq!((line.calls(), line.transitions()), (2, 2));
    }

    /// Issues `cmd` with several buffers.
    fn run_bufs(d: &GoldfishPipe, mem: &GuestRam, id: u32, cb: u64, cmd: i32, bufs: &[(u64, u32)]) -> (i32, i32) {
        mem.write(cb + CB_CMD, &cmd.to_le_bytes()).expect("w");
        mem.write(cb + CB_BUFFERS_COUNT, &(bufs.len() as u32).to_le_bytes()).expect("w");
        for (i, &(addr, len)) in bufs.iter().enumerate() {
            mem.write(cb + CB_PTRS + 8 * i as u64, &addr.to_le_bytes()).expect("w");
            mem.write(cb + CB_SIZES + 4 * i as u64, &len.to_le_bytes()).expect("w");
        }
        wr(d, REG_CMD, id);
        (mem.read_u32(cb + CB_STATUS).expect("r") as i32, mem.read_u32(cb + CB_CONSUMED).expect("r") as i32)
    }

    #[test]
    fn commands_span_several_buffers() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let mut reg = PipeRegistry::default();
        reg.register("GLProcessPipe", Box::new(GlProcess::numbered_from_one(None)));
        let d = GoldfishPipe::new(mem.clone(), Arc::new(reg), Arc::new(TestLine::default()));
        wr(&d, REG_VERSION, 4);
        let (open_buf, cb) = (RAM + 0x1000, RAM + 0x3000);
        wr(&d, REG_OPEN_BUFFER_HIGH, 0);
        wr(&d, REG_OPEN_BUFFER, open_buf as u32);
        mem.write(open_buf, &cb.to_le_bytes()).expect("w");
        assert_eq!(run_bufs(&d, &mem, 5, cb, CMD_OPEN, &[]).0, 0);
        // The name split across buffers, and the confirmation in the last one.
        let (a, b, c) = (RAM + 0x8000, RAM + 0x9000, RAM + 0xa000);
        mem.write(a, b"pipe:GLPro").expect("w");
        mem.write(b, b"cessPipe\0").expect("w");
        mem.write(c, &7u32.to_le_bytes()).expect("w");
        assert_eq!(run_bufs(&d, &mem, 5, cb, CMD_WRITE, &[(a, 10), (b, 9), (c, 4)]), (23, 23));
        // The 8-byte reply read into a 3- and a 5-byte buffer.
        let (r1, r2) = (RAM + 0xb000, RAM + 0xc000);
        assert_eq!(run_bufs(&d, &mem, 5, cb, CMD_READ, &[(r1, 3), (r2, 5)]), (8, 8));
        let mut reply = [0u8; 8];
        mem.read(r1, &mut reply[..3]).expect("r1");
        mem.read(r2, &mut reply[3..]).expect("r2");
        assert_eq!(u64::from_le_bytes(reply), 1, "the first process id, whole");
        assert_eq!(run_bufs(&d, &mem, 5, cb, CMD_READ, &[(r1, 8)]).0, ERR_AGAIN, "all consumed");
    }

    /// Opens pipe `id` (command page `cb`) on `name`.
    fn open_named(d: &GoldfishPipe, mem: &GuestRam, id: u32, cb: u64, name: &[u8]) -> (i32, i32) {
        let open_buf = RAM + 0x1000;
        mem.write(open_buf, &cb.to_le_bytes()).expect("w");
        assert_eq!(run(d, mem, id, cb, CMD_OPEN, None).0, 0);
        let data = RAM + 0x8000;
        mem.write(data, name).expect("w");
        run(d, mem, id, cb, CMD_WRITE, Some((data, name.len() as u32)))
    }

    fn device(reg: PipeRegistry) -> (Arc<GuestRam>, Arc<TestLine>, Arc<GoldfishPipe>) {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        let d = GoldfishPipe::new(mem.clone(), Arc::new(reg), line.clone());
        wr(&d, REG_VERSION, 4);
        wr(&d, REG_OPEN_BUFFER_HIGH, 0);
        wr(&d, REG_OPEN_BUFFER, (RAM + 0x1000) as u32);
        wr(&d, REG_SIGNAL_BUFFER_HIGH, 0);
        wr(&d, REG_SIGNAL_BUFFER, (RAM + 0x2000) as u32);
        wr(&d, REG_SIGNAL_BUFFER_COUNT, 64);
        (mem, line, d)
    }

    /// The guest's GLES encoder on API 29–30: `pipe:opengles`, a command, a blocking read woken
    /// by the render thread's reply.
    #[test]
    fn opengles_pipes_carry_the_gles_stream_both_ways() {
        let backend = Arc::new(crate::pipe::opengles::tests::Backend::default());
        let mut reg = PipeRegistry::default();
        reg.register("opengles", Box::new(crate::pipe::opengles::OpenglesService { backend: backend.clone() }));
        let (mem, line, d) = device(reg);
        let cb = RAM + 0x3000;
        assert_eq!(open_named(&d, &mem, 9, cb, b"pipe:opengles\0"), (14, 14));
        let data = RAM + 0x8000;
        mem.write(data, b"abc").expect("w");
        assert_eq!(run(&d, &mem, 9, cb, CMD_WRITE, Some((data, 3))), (3, 3));
        // The guest finds nothing yet or the reply already there; either way it gets it.
        if run(&d, &mem, 9, cb, CMD_READ, Some((data + 0x100, 16))).0 == ERR_AGAIN {
            assert_eq!(run(&d, &mem, 9, cb, CMD_WAKE_ON_READ, None).0, 0);
            let t = std::time::Instant::now();
            while !line.level() {
                assert!(t.elapsed() < std::time::Duration::from_secs(5), "never woken");
                std::thread::yield_now();
            }
            assert_eq!(rd(&d, REG_GET_SIGNALLED), 1);
            assert_eq!(mem.read_u32(RAM + 0x2000 + 4).expect("flags"), WAKE_READ);
            assert_eq!(run(&d, &mem, 9, cb, CMD_READ, Some((data + 0x100, 16))), (3, 3));
        }
        let mut reply = [0u8; 3];
        mem.read(data + 0x100, &mut reply).expect("r");
        assert_eq!(&reply, b"cba");
        assert_eq!(run(&d, &mem, 9, cb, CMD_CLOSE, None).0, 0);
        let c = backend.channels.lock().expect("lock")[0].clone();
        assert_eq!(
            crate::pipe::opengles::GlChannel::try_write(&*c, b"x"),
            crate::pipe::opengles::GlWrite::Stopped,
            "the guest's close stopped the channel"
        );
    }

    #[test]
    fn gl_processes_are_announced_to_the_renderer_when_they_come_and_go() {
        let backend = Arc::new(crate::pipe::opengles::tests::Backend::default());
        let mut reg = PipeRegistry::default();
        reg.register("GLProcessPipe", Box::new(GlProcess::numbered_from_one(Some(backend.clone()))));
        let (mem, _line, d) = device(reg);
        for (id, cb) in [(3, RAM + 0x3000), (4, RAM + 0x4000)] {
            assert_eq!(open_named(&d, &mem, id, cb, b"pipe:GLProcessPipe\0").0, 19);
        }
        assert_eq!(run(&d, &mem, 3, RAM + 0x3000, CMD_CLOSE, None).0, 0);
        assert_eq!(*backend.processes.lock().expect("lock"), [(1, true), (2, true), (1, false)]);
    }

    fn wait_for_line(line: &TestLine) {
        let t = std::time::Instant::now();
        while !line.level() {
            assert!(t.elapsed() < std::time::Duration::from_secs(5), "never woken");
            std::thread::yield_now();
        }
    }

    /// The guest closes a GL pipe whose render thread still has a reply and a close on the way,
    /// and the driver hands the same id to the next pipe: that pipe gets neither.
    #[test]
    fn a_closed_gl_pipes_late_replies_never_reach_the_pipe_that_reuses_its_id() {
        let backend = Arc::new(crate::pipe::opengles::tests::Backend::default());
        let mut reg = PipeRegistry::default();
        reg.register("opengles", Box::new(crate::pipe::opengles::OpenglesService { backend: backend.clone() }));
        let (mem, line, d) = device(reg);
        let (cb, data) = (RAM + 0x3000, RAM + 0x8000);
        assert_eq!(open_named(&d, &mem, 9, cb, b"pipe:opengles\0"), (14, 14));
        let old = backend.channels.lock().expect("lock")[0].clone();
        old.set_held(true);
        mem.write(data, b"abc").expect("w");
        assert_eq!(run(&d, &mem, 9, cb, CMD_WRITE, Some((data, 3))), (3, 3));
        assert_eq!(run(&d, &mem, 9, cb, CMD_CLOSE, None).0, 0);
        assert_eq!(open_named(&d, &mem, 9, cb, b"pipe:opengles\0"), (14, 14), "the id, reused");
        // The old render thread's reply ("cba") and its close go out now, after the reuse.
        old.set_held(false);
        let t = std::time::Instant::now();
        // The backend and this test hold the rest.
        while Arc::strong_count(&old) > 2 {
            assert!(t.elapsed() < std::time::Duration::from_secs(5), "the old reader never ended");
            std::thread::yield_now();
        }
        // The new pipe's own round trip, queued behind them.
        mem.write(data, b"xyz").expect("w");
        assert_eq!(run(&d, &mem, 9, cb, CMD_WRITE, Some((data, 3))), (3, 3), "the new pipe is open");
        if run(&d, &mem, 9, cb, CMD_READ, Some((data + 0x100, 16))).0 == ERR_AGAIN {
            assert_eq!(run(&d, &mem, 9, cb, CMD_WAKE_ON_READ, None).0, 0);
            wait_for_line(&line);
            assert_eq!(rd(&d, REG_GET_SIGNALLED), 1);
            assert_eq!(mem.read_u32(RAM + 0x2000 + 4).expect("flags"), WAKE_READ, "woken to read, not closed");
            assert_eq!(run(&d, &mem, 9, cb, CMD_READ, Some((data + 0x100, 16))), (3, 3), "only its own reply");
        }
        let mut reply = [0u8; 3];
        mem.read(data + 0x100, &mut reply).expect("r");
        assert_eq!(&reply, b"zyx");
        assert_eq!(run(&d, &mem, 9, cb, CMD_POLL, None).0, POLL_OUT, "no stale bytes, no hang-up");
    }

    /// A render thread that fell 1024 writes behind: the vCPU gets "try again" and returns (the
    /// device lock with it), and the guest is woken to write once there's room.
    #[test]
    fn a_full_render_queue_answers_try_again_and_wakes_the_writer_later() {
        let backend = Arc::new(crate::pipe::opengles::tests::Backend::default());
        let mut reg = PipeRegistry::default();
        reg.register("opengles", Box::new(crate::pipe::opengles::OpenglesService { backend: backend.clone() }));
        reg.register("GLProcessPipe", Box::new(GlProcess::default()));
        let (mem, line, d) = device(reg);
        let (gl, other) = (RAM + 0x3000, RAM + 0x4000);
        assert_eq!(open_named(&d, &mem, 9, gl, b"pipe:opengles\0"), (14, 14));
        assert_eq!(open_named(&d, &mem, 3, other, b"pipe:GLProcessPipe\0").0, 19);
        let c = backend.channels.lock().expect("lock")[0].clone();
        c.set_full(true);
        let (tx, rx) = channel();
        let (vcpu, vmem) = (d.clone(), mem.clone());
        std::thread::spawn(move || {
            let data = RAM + 0x9000;
            vmem.write(data, b"abc").expect("w");
            let first = run(&vcpu, &vmem, 9, gl, CMD_WRITE, Some((data, 3)));
            vmem.write(data, b"def").expect("w");
            let second = run(&vcpu, &vmem, 9, gl, CMD_WRITE, Some((data, 3)));
            let _ = tx.send((first, second));
        });
        let (first, second) = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("the vCPU stalled on a full render queue");
        assert_eq!(first, (3, 3), "taken and held for the render thread");
        assert_eq!(second.0, ERR_AGAIN, "nothing more until it catches up");
        assert_eq!(run(&d, &mem, 3, other, CMD_POLL, None).0, POLL_OUT, "other pipes are still served");
        assert_eq!(run(&d, &mem, 9, gl, CMD_WAKE_ON_WRITE, None).0, 0);
        assert!(!line.level(), "no wake while the queue is full");
        c.set_full(false);
        wait_for_line(&line);
        assert_eq!(rd(&d, REG_GET_SIGNALLED), 1);
        assert_eq!(mem.read_u32(RAM + 0x2000).expect("id"), 9);
        assert_eq!(mem.read_u32(RAM + 0x2000 + 4).expect("flags"), WAKE_WRITE);
        let data = RAM + 0xa000;
        mem.write(data, b"def").expect("w");
        assert_eq!(run(&d, &mem, 9, gl, CMD_WRITE, Some((data, 3))), (3, 3));
        assert_eq!(&*c.written.lock().expect("lock"), b"abcdef", "in order, once each");
    }

    /// `boot_and_run` builds the pipe services again on a guest reset, but the renderer (and the
    /// GL state it keeps per process id) lives as long as emu: ids never repeat.
    #[test]
    fn gl_process_ids_keep_counting_across_a_guest_reset() {
        let first_boot = Arc::new(crate::pipe::opengles::tests::Backend::default());
        let second_boot = Arc::new(crate::pipe::opengles::tests::Backend::default());
        let mut puids = Vec::new();
        for backend in [&first_boot, &second_boot] {
            let mut reg = PipeRegistry::default();
            reg.register("GLProcessPipe", Box::new(GlProcess::with_backend(Some(backend.clone()))));
            let (mem, _line, d) = device(reg);
            assert_eq!(open_named(&d, &mem, 3, RAM + 0x3000, b"pipe:GLProcessPipe\0").0, 19);
            puids.push(backend.processes.lock().expect("lock")[0].0);
        }
        assert!(puids[1] > puids[0], "the second boot's process reused id {} of the first", puids[0]);
    }
}
