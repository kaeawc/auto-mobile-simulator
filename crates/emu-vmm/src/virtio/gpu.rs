//! virtio-gpu with gfxstream contexts (virtio 1.2 §5.7): the guest's GLES/Vulkan/composer
//! streams go to the host renderer; scanout frames go to a [`DisplaySink`].
//!
//! Requests run on one worker thread. A request carrying a fence is answered only after the
//! renderer signals that fence (its callback posts to the worker), per (context, ring) timeline.
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use emu_gfxstream::{Box3, Fence, Renderer, ResourceCreateArgs};
use emu_hvf::{GuestRam, HostRange, Vm};

use super::mmio::F_RING_INDIRECT_DESC;
use super::queue::{Chain, Queue};
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};
use crate::display::DisplaySink;

pub const ID_GPU: u32 = 16;

const F_VIRGL: u64 = 1 << 0;
const F_RESOURCE_BLOB: u64 = 1 << 3;
const F_CONTEXT_INIT: u64 = 1 << 4;

/// Shared memory region id the guest maps host-visible blobs into.
pub const SHM_HOST_VISIBLE: u8 = 1;

const FLAG_FENCE: u32 = 1;
const FLAG_RING_IDX: u32 = 2;

mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const RESOURCE_CREATE_BLOB: u32 = 0x010c;
    pub const SET_SCANOUT_BLOB: u32 = 0x010d;
    pub const CTX_CREATE: u32 = 0x0200;
    pub const CTX_DESTROY: u32 = 0x0201;
    pub const CTX_ATTACH_RESOURCE: u32 = 0x0202;
    pub const CTX_DETACH_RESOURCE: u32 = 0x0203;
    pub const RESOURCE_CREATE_3D: u32 = 0x0204;
    pub const TRANSFER_TO_HOST_3D: u32 = 0x0205;
    pub const TRANSFER_FROM_HOST_3D: u32 = 0x0206;
    pub const SUBMIT_3D: u32 = 0x0207;
    pub const RESOURCE_MAP_BLOB: u32 = 0x0208;
    pub const RESOURCE_UNMAP_BLOB: u32 = 0x0209;
    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;

    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const OK_CAPSET_INFO: u32 = 0x1102;
    pub const OK_CAPSET: u32 = 0x1103;
    pub const OK_MAP_INFO: u32 = 0x1106;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

pub struct GpuConfig {
    pub width: u32,
    pub height: u32,
    pub renderer: Arc<Renderer>,
    /// Guest physical base and size of the host-visible region.
    pub shm: (u64, u64),
    pub display: Arc<dyn DisplaySink>,
}

enum Event {
    Kick,
    Fence(Fence),
    Stop,
}

/// Where the renderer's fence callback posts; replaced when the device (re)activates.
static FENCE_SINK: Mutex<Option<Sender<Event>>> = Mutex::new(None);

/// Routes renderer fences to the active GPU worker. Pass to `Renderer::init`.
pub fn fence_handler() -> Arc<dyn Fn(Fence) + Send + Sync> {
    Arc::new(|f| {
        if let Some(tx) = FENCE_SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(Event::Fence(f));
        }
    })
}

pub struct Gpu {
    cfg: Arc<GpuConfig>,
    num_capsets: u32,
    events: Sender<Event>,
    rx: Option<Receiver<Event>>,
    worker: Option<JoinHandle<Receiver<Event>>>,
}

impl Gpu {
    pub fn new(cfg: GpuConfig) -> Gpu {
        let num_capsets = emu_gfxstream::capset::ALL.iter().filter(|&&c| cfg.renderer.capset_info(c).is_some()).count() as u32;
        let (events, rx) = channel();
        Gpu { cfg: Arc::new(cfg), num_capsets, events, rx: Some(rx), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.events.send(Event::Stop);
            if let Ok(rx) = h.join() {
                while rx.try_recv().is_ok() {}
                self.rx = Some(rx);
            }
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Gpu {
    fn device_id(&self) -> u32 {
        ID_GPU
    }
    fn features(&self) -> u64 {
        F_VIRGL | F_RESOURCE_BLOB | F_CONTEXT_INIT | F_RING_INDIRECT_DESC
    }
    fn num_queues(&self) -> usize {
        2
    }
    fn queue_max_size(&self) -> u16 {
        1024
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut cfg = [0u8; 16];
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        cfg[12..16].copy_from_slice(&self.num_capsets.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn shm_region(&self, id: u8) -> Option<(u64, u64)> {
        (id == SHM_HOST_VISIBLE).then_some(self.cfg.shm)
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let ctrl = act.queues.first().copied().flatten().ok_or("gpu control queue not enabled")?;
        let cursor = act.queues.get(1).copied().flatten();
        self.stop_worker();
        let rx = self.rx.take().ok_or("gpu worker receiver missing")?;
        *FENCE_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(self.events.clone());
        let capsets: Vec<u32> =
            emu_gfxstream::capset::ALL.iter().copied().filter(|&c| self.cfg.renderer.capset_info(c).is_some()).collect();
        let mut w = Worker {
            mem: act.mem,
            irq: act.irq,
            ctrl: Queue::new(ctrl, false),
            cursor: cursor.map(|c| Queue::new(c, false)),
            cfg: self.cfg.clone(),
            capsets,
            resources: HashMap::new(),
            scanout: None,
            pending: Vec::new(),
            frame: Vec::new(),
            req: Vec::new(),
        };
        let handle = std::thread::Builder::new()
            .name("gpu".into())
            .spawn(move || {
                w.run(&rx);
                w.teardown();
                rx
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some(handle);
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        let _ = self.events.send(Event::Kick);
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

#[derive(Default)]
struct Resource {
    width: u32,
    height: u32,
    format: u32,
    /// Guest physical range the blob is mapped at (unmapped on unref).
    mapped: Option<(u64, usize)>,
}

#[derive(Clone, Copy)]
struct Scanout {
    resource: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

struct Pending {
    /// (ctx, ring) for ring-indexed fences; (0, 0) for the global timeline.
    timeline: (u32, u8),
    fence_id: u64,
    head: u16,
    len: u32,
}

#[derive(Clone, Copy, Default)]
struct Hdr {
    typ: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
}

impl Hdr {
    fn parse(b: &[u8]) -> Option<Hdr> {
        (b.len() >= 24).then(|| Hdr { typ: le32(b, 0), flags: le32(b, 4), fence_id: le64(b, 8), ctx_id: le32(b, 16), ring_idx: b[20] })
    }

    /// The response header: echoes fence fields.
    fn reply(&self, typ: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&(self.flags & (FLAG_FENCE | FLAG_RING_IDX)).to_le_bytes());
        out.extend_from_slice(&self.fence_id.to_le_bytes());
        out.extend_from_slice(&self.ctx_id.to_le_bytes());
        out.push(self.ring_idx);
        out.extend_from_slice(&[0, 0, 0]);
        out
    }
}

/// `EMU_GPU_TRACE=1` logs every command, fence creation and fence signal to stderr.
fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("EMU_GPU_TRACE").is_some())
}

fn le32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn le64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap_or_default()))
}

fn rect(b: &[u8], o: usize) -> (u32, u32, u32, u32) {
    (le32(b, o), le32(b, o + 4), le32(b, o + 8), le32(b, o + 12))
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    ctrl: Queue,
    cursor: Option<Queue>,
    cfg: Arc<GpuConfig>,
    capsets: Vec<u32>,
    resources: HashMap<u32, Resource>,
    scanout: Option<Scanout>,
    pending: Vec<Pending>,
    frame: Vec<u8>,
    /// The current control request, reused across requests (command streams are KBs to MBs).
    req: Vec<u8>,
}

impl Worker {
    fn run(&mut self, rx: &Receiver<Event>) {
        while let Ok(ev) = rx.recv() {
            let mut ev = Some(ev);
            let mut used = false;
            while let Some(e) = ev.take() {
                match e {
                    Event::Stop => return,
                    Event::Kick => used |= self.process(),
                    Event::Fence(f) => used |= self.complete_fences(f),
                }
                ev = rx.try_recv().ok();
            }
            if used {
                self.irq.raise(INT_USED_RING);
            }
        }
    }

    fn teardown(&mut self) {
        for (id, r) in self.resources.drain() {
            if let Some((ipa, len)) = r.mapped {
                let _ = Vm::unmap_raw(ipa, len);
                let _ = self.cfg.renderer.unmap(id);
            }
        }
    }

    fn process(&mut self) -> bool {
        let mut used = false;
        if let Some(q) = self.cursor.as_mut() {
            while let Ok(Some(c)) = q.pop(&self.mem) {
                let _ = q.add_used(&self.mem, c.head, 0);
                used = true;
            }
        }
        loop {
            let chain = match self.ctrl.pop(&self.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("emu: gpu ctrl: {e}");
                    break;
                }
            };
            let mut req = std::mem::take(&mut self.req);
            if chain.read_all_into(&self.mem, 32 << 20, &mut req).is_err() {
                req.clear();
            }
            let (hdr, resp) = match Hdr::parse(&req) {
                Some(h) => (h, self.handle(&h, &mut req, &chain)),
                None => (Hdr::default(), Hdr::default().reply(cmd::ERR_UNSPEC)),
            };
            let len = chain.write_all(&self.mem, &resp).unwrap_or(0);
            if trace() {
                // For resource commands, the resource id (and for creates, target/format/bind).
                let detail = match hdr.typ {
                    cmd::RESOURCE_CREATE_3D => format!(
                        " res {} target {} fmt {} bind {:#x} {}x{}",
                        le32(&req, 24),
                        le32(&req, 28),
                        le32(&req, 32),
                        le32(&req, 36),
                        le32(&req, 40),
                        le32(&req, 44)
                    ),
                    cmd::RESOURCE_CREATE_BLOB => format!(
                        " res {} mem {} flags {:#x} id {} size {}",
                        le32(&req, 24),
                        le32(&req, 28),
                        le32(&req, 32),
                        le64(&req, 40),
                        le64(&req, 48)
                    ),
                    cmd::RESOURCE_UNREF | cmd::CTX_ATTACH_RESOURCE | cmd::CTX_DETACH_RESOURCE | cmd::RESOURCE_ATTACH_BACKING => {
                        format!(" res {}", le32(&req, 24))
                    }
                    cmd::TRANSFER_TO_HOST_3D | cmd::TRANSFER_FROM_HOST_3D => format!(" res {}", le32(&req, 56)),
                    cmd::RESOURCE_MAP_BLOB | cmd::RESOURCE_UNMAP_BLOB => format!(" res {}", le32(&req, 24)),
                    _ => String::new(),
                };
                eprintln!(
                    "gpu: cmd {:#06x} ctx {} flags {:#x} fence {} ring {}{detail} -> {:#06x}",
                    hdr.typ,
                    hdr.ctx_id,
                    hdr.flags,
                    hdr.fence_id,
                    hdr.ring_idx,
                    le32(&resp, 0)
                );
            }
            self.req = req;
            if hdr.flags & FLAG_FENCE != 0 {
                let ringed = hdr.flags & FLAG_RING_IDX != 0;
                let timeline = if ringed { (hdr.ctx_id, hdr.ring_idx) } else { (0, 0) };
                let f = Fence {
                    flags: emu_gfxstream::FENCE_FLAG | if ringed { emu_gfxstream::FENCE_FLAG_RING_IDX } else { 0 },
                    fence_id: hdr.fence_id,
                    ctx_id: if ringed { hdr.ctx_id } else { 0 },
                    ring_idx: if ringed { hdr.ring_idx } else { 0 },
                };
                if self.cfg.renderer.create_fence(f).is_ok() {
                    self.pending.push(Pending { timeline, fence_id: hdr.fence_id, head: chain.head, len });
                    continue;
                }
            }
            let _ = self.ctrl.add_used(&self.mem, chain.head, len);
            used = true;
        }
        used
    }

    fn complete_fences(&mut self, f: Fence) -> bool {
        if trace() {
            eprintln!(
                "gpu: fence signaled id {} ctx {} ring {} flags {:#x} (pending {})",
                f.fence_id,
                f.ctx_id,
                f.ring_idx,
                f.flags,
                self.pending.len()
            );
        }
        let ringed = f.flags & emu_gfxstream::FENCE_FLAG_RING_IDX != 0;
        let timeline = if ringed { (f.ctx_id, f.ring_idx) } else { (0, 0) };
        let mut used = false;
        let mut i = 0;
        while i < self.pending.len() {
            let p = &self.pending[i];
            if p.timeline == timeline && p.fence_id <= f.fence_id {
                let p = self.pending.remove(i);
                let _ = self.ctrl.add_used(&self.mem, p.head, p.len);
                used = true;
            } else {
                i += 1;
            }
        }
        used
    }

    /// Guest memory entries (`addr: u64, len: u32, pad: u32`) → host ranges.
    fn backing(&self, req: &[u8], at: usize, n: u32) -> Option<Vec<HostRange>> {
        (0..n as usize)
            .map(|i| {
                let o = at + i * 16;
                self.mem.host_range(le64(req, o), le32(req, o + 8) as usize)
            })
            .collect()
    }

    fn handle(&mut self, h: &Hdr, req: &mut [u8], _chain: &Chain) -> Vec<u8> {
        let r = &self.cfg.renderer;
        let ok = || h.reply(cmd::OK_NODATA);
        match h.typ {
            cmd::GET_DISPLAY_INFO => {
                let mut out = h.reply(cmd::OK_DISPLAY_INFO);
                for i in 0..16u32 {
                    let (w, hgt, en): (u32, u32, u32) = if i == 0 { (self.cfg.width, self.cfg.height, 1) } else { (0, 0, 0) };
                    for v in [0u32, 0, w, hgt, en, 0] {
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
                out
            }
            cmd::GET_CAPSET_INFO => {
                let idx = le32(req, 24) as usize;
                match self.capsets.get(idx).and_then(|&id| r.capset_info(id).map(|(v, s)| (id, v, s))) {
                    Some((id, ver, size)) => {
                        let mut out = h.reply(cmd::OK_CAPSET_INFO);
                        for v in [id, ver, size, 0] {
                            out.extend_from_slice(&v.to_le_bytes());
                        }
                        out
                    }
                    None => h.reply(cmd::ERR_INVALID_PARAMETER),
                }
            }
            cmd::GET_CAPSET => {
                let data = r.capset(le32(req, 24), le32(req, 28));
                let mut out = h.reply(cmd::OK_CAPSET);
                out.extend_from_slice(&data);
                out
            }
            cmd::CTX_CREATE => {
                let nlen = (le32(req, 24) as usize).min(64);
                let init = le32(req, 28);
                let name = req.get(32..32 + nlen).unwrap_or(&[]);
                match r.context_create(h.ctx_id, name, init) {
                    Ok(()) => ok(),
                    Err(_) => h.reply(cmd::ERR_UNSPEC),
                }
            }
            cmd::CTX_DESTROY => {
                r.context_destroy(h.ctx_id);
                ok()
            }
            cmd::CTX_ATTACH_RESOURCE => {
                r.ctx_attach_resource(h.ctx_id, le32(req, 24));
                ok()
            }
            cmd::CTX_DETACH_RESOURCE => {
                r.ctx_detach_resource(h.ctx_id, le32(req, 24));
                ok()
            }
            cmd::RESOURCE_CREATE_2D => {
                let (id, format, width, height) = (le32(req, 24), le32(req, 28), le32(req, 32), le32(req, 36));
                let args = ResourceCreateArgs {
                    handle: id,
                    target: 2, // PIPE_TEXTURE_2D
                    format,
                    bind: 1 << 1, // render target
                    width,
                    height,
                    depth: 1,
                    array_size: 1,
                    ..Default::default()
                };
                self.create(h, id, args, width, height, format)
            }
            cmd::RESOURCE_CREATE_3D => {
                let v: Vec<u32> = (0..11).map(|i| le32(req, 24 + i * 4)).collect();
                let args = ResourceCreateArgs {
                    handle: v[0],
                    target: v[1],
                    format: v[2],
                    bind: v[3],
                    width: v[4],
                    height: v[5],
                    depth: v[6],
                    array_size: v[7],
                    last_level: v[8],
                    nr_samples: v[9],
                    flags: v[10],
                };
                self.create(h, v[0], args, v[4], v[5], v[2])
            }
            cmd::RESOURCE_UNREF => {
                let id = le32(req, 24);
                if let Some(res) = self.resources.remove(&id) {
                    if let Some((ipa, len)) = res.mapped {
                        let _ = Vm::unmap_raw(ipa, len);
                        let _ = r.unmap(id);
                    }
                    if self.scanout.is_some_and(|s| s.resource == id) {
                        self.scanout = None;
                    }
                }
                r.resource_unref(id);
                ok()
            }
            cmd::RESOURCE_ATTACH_BACKING => {
                let (id, n) = (le32(req, 24), le32(req, 28));
                match self.backing(req, 32, n) {
                    Some(b) => match r.attach_backing(id, &b) {
                        Ok(()) => ok(),
                        Err(_) => h.reply(cmd::ERR_UNSPEC),
                    },
                    None => h.reply(cmd::ERR_INVALID_PARAMETER),
                }
            }
            cmd::RESOURCE_DETACH_BACKING => {
                r.detach_backing(le32(req, 24));
                ok()
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let (x, y, w, hgt) = rect(req, 24);
                let offset = le64(req, 40);
                let id = le32(req, 48);
                let b = Box3 { x, y, z: 0, w, h: hgt, d: 1 };
                match r.transfer_write(id, 0, 0, 0, 0, b, offset) {
                    Ok(()) => ok(),
                    Err(_) => h.reply(cmd::ERR_INVALID_RESOURCE_ID),
                }
            }
            cmd::TRANSFER_TO_HOST_3D | cmd::TRANSFER_FROM_HOST_3D => {
                let b = Box3 { x: le32(req, 24), y: le32(req, 28), z: le32(req, 32), w: le32(req, 36), h: le32(req, 40), d: le32(req, 44) };
                let offset = le64(req, 48);
                let (id, level, stride, layer_stride) = (le32(req, 56), le32(req, 60), le32(req, 64), le32(req, 68));
                let res = if h.typ == cmd::TRANSFER_TO_HOST_3D {
                    r.transfer_write(id, h.ctx_id, level, stride, layer_stride, b, offset)
                } else {
                    r.transfer_read(id, h.ctx_id, level, stride, layer_stride, b, offset, &[])
                };
                match res {
                    Ok(()) => ok(),
                    Err(_) => h.reply(cmd::ERR_INVALID_RESOURCE_ID),
                }
            }
            cmd::SUBMIT_3D => {
                let size = le32(req, 24) as usize;
                // The command stream in place, not a copy of it.
                let Some(buf) = req.get_mut(32..32 + size) else { return h.reply(cmd::ERR_INVALID_PARAMETER) };
                match r.submit(h.ctx_id, buf) {
                    Ok(()) => ok(),
                    Err(_) => h.reply(cmd::ERR_UNSPEC),
                }
            }
            cmd::RESOURCE_CREATE_BLOB => {
                let (id, blob_mem, blob_flags, n) = (le32(req, 24), le32(req, 28), le32(req, 32), le32(req, 36));
                let (blob_id, size) = (le64(req, 40), le64(req, 48));
                let Some(backing) = self.backing(req, 56, n) else { return h.reply(cmd::ERR_INVALID_PARAMETER) };
                match r.create_blob(h.ctx_id, id, blob_mem, blob_flags, blob_id, size, &backing) {
                    Ok(()) => {
                        self.resources.insert(id, Resource::default());
                        ok()
                    }
                    Err(e) => {
                        eprintln!("emu: gpu: create_blob {id}: {e}");
                        h.reply(cmd::ERR_UNSPEC)
                    }
                }
            }
            cmd::RESOURCE_MAP_BLOB => self.map_blob(h, le32(req, 24), le64(req, 32)),
            cmd::RESOURCE_UNMAP_BLOB => {
                let id = le32(req, 24);
                if let Some((ipa, len)) = self.resources.get_mut(&id).and_then(|res| res.mapped.take()) {
                    let _ = Vm::unmap_raw(ipa, len);
                    let _ = r.unmap(id);
                }
                ok()
            }
            cmd::SET_SCANOUT | cmd::SET_SCANOUT_BLOB => {
                let scanout_id = le32(req, 40);
                if scanout_id != 0 {
                    return h.reply(cmd::ERR_INVALID_SCANOUT_ID);
                }
                let id = le32(req, 44);
                self.scanout = if id == 0 {
                    None
                } else if h.typ == cmd::SET_SCANOUT_BLOB {
                    Some(Scanout {
                        resource: id,
                        width: le32(req, 48),
                        height: le32(req, 52),
                        format: le32(req, 56),
                        stride: le32(req, 64),
                    })
                } else {
                    let res = self.resources.get(&id);
                    let (w, hgt, fmt) = res.map_or((self.cfg.width, self.cfg.height, 1), |r| (r.width, r.height, r.format));
                    Some(Scanout { resource: id, width: w, height: hgt, format: fmt, stride: w * 4 })
                };
                ok()
            }
            cmd::RESOURCE_FLUSH => {
                let id = le32(req, 40);
                if let Some(s) = self.scanout.filter(|s| s.resource == id) {
                    self.present(s);
                }
                ok()
            }
            cmd::UPDATE_CURSOR | cmd::MOVE_CURSOR => ok(),
            _ => h.reply(cmd::ERR_UNSPEC),
        }
    }

    /// Every reply must echo the request's fence fields: the guest driver signals its fence
    /// from the fence id in the response header.
    fn create(&mut self, h: &Hdr, id: u32, args: ResourceCreateArgs, width: u32, height: u32, format: u32) -> Vec<u8> {
        match self.cfg.renderer.resource_create(args, &[]) {
            Ok(()) => {
                self.resources.insert(id, Resource { width, height, format, mapped: None });
                h.reply(cmd::OK_NODATA)
            }
            Err(e) => {
                eprintln!("emu: gpu: resource_create {id}: {e}");
                h.reply(cmd::ERR_UNSPEC)
            }
        }
    }

    fn map_blob(&mut self, h: &Hdr, id: u32, offset: u64) -> Vec<u8> {
        let r = &self.cfg.renderer;
        let Some(res) = self.resources.get_mut(&id) else { return h.reply(cmd::ERR_INVALID_RESOURCE_ID) };
        let host = match r.map(id) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("emu: gpu: map blob {id}: {e}");
                return h.reply(cmd::ERR_UNSPEC);
            }
        };
        let (base, size) = self.cfg.shm;
        let len = host.len().div_ceil(4096) * 4096;
        if offset.checked_add(len as u64).is_none_or(|end| end > size) {
            let _ = r.unmap(id);
            return h.reply(cmd::ERR_INVALID_PARAMETER);
        }
        // `host` stays mapped until we unmap it (RESOURCE_UNMAP_BLOB / UNREF / teardown).
        let range = emu_gfxstream::page_range(host, len);
        if let Err(e) = Vm::map_raw(range, base + offset) {
            eprintln!("emu: gpu: hv_vm_map blob {id} at +{offset:#x} ({len:#x} bytes): {e}");
            let _ = r.unmap(id);
            return h.reply(cmd::ERR_UNSPEC);
        }
        res.mapped = Some((base + offset, len));
        let mut out = h.reply(cmd::OK_MAP_INFO);
        out.extend_from_slice(&(r.map_info(id) & 0x0f).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    fn present(&mut self, s: Scanout) {
        let stride = if s.stride == 0 { s.width * 4 } else { s.stride };
        let size = stride as usize * s.height as usize;
        self.frame.resize(size, 0);
        let b = Box3 { x: 0, y: 0, z: 0, w: s.width, h: s.height, d: 1 };
        if self.cfg.renderer.read_into(s.resource, stride, b, &mut self.frame).is_ok() {
            self.cfg.display.present(s.width, s.height, stride, s.format, &self.frame);
        }
    }
}
