//! virtio-gpu with gfxstream contexts (virtio 1.2 §5.7): the guest's GLES/Vulkan/composer
//! streams go to the host renderer; scanout frames go to a [`DisplaySink`].
//!
//! Requests run on one worker thread. A request carrying a fence is answered only after the
//! renderer signals that fence (its callback posts to the worker), per (context, ring) timeline.
//! The renderer gets the device's own fence ids, not the guest's ([`FenceIds`]).
//!
//! The renderer is process-wide and outlives the device: a reset (the driver's, or the machine's
//! when the guest reboots) gives back everything the guest made in it ([`Worker::teardown`],
//! [`Leftovers`]), so the next boot starts from a clean renderer whatever the last one left
//! behind.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use emu_gfxstream::{Backing, Box3, Fence, HostRange, Renderer, ResourceCreateArgs};
use emu_hvf::GuestRam;

use super::legacy_asg::{self, LegacyAsg};
use super::mmio::{F_RING_EVENT_IDX, F_RING_INDIRECT_DESC};
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

/// Where the guest sees host-visible blobs: virtio shared memory region 1, a window of `size`
/// bytes the device maps blobs into at offsets the guest picks. Emu's own machine maps a blob's
/// host pages into the VM at the region's guest-physical address ([`HvWindow`]); a vhost-user
/// frontend owns the region itself and is sent the blob's fd to map (`SHMEM_MAP`).
pub trait ShmWindow: Send + Sync {
    /// Whether [`ShmWindow::map`] needs the blob's host mapping (`stream_renderer_resource_map`).
    /// A window that maps blobs by fd doesn't, and a renderer with exportable blobs refuses it.
    fn needs_host_mapping(&self) -> bool {
        true
    }
    /// Maps blob `id` (`len` bytes, whole pages) at `offset`; `host` is its host mapping when
    /// [`ShmWindow::needs_host_mapping`].
    fn map(&self, id: u32, host: Option<HostRange>, offset: u64, len: usize) -> Result<(), String>;
    /// Removes the mapping at `offset` (`len` bytes) that [`ShmWindow::map`] made.
    fn unmap(&self, offset: u64, len: usize);
    /// [`ShmWindow::unmap`] while the device is being stopped. A frontend may be the one stopping
    /// it and wait for that to finish before it reads anything else, so a window that would ask
    /// the frontend to unmap defers it (to before its next map) instead of waiting on an answer.
    fn unmap_stopping(&self, offset: u64, len: usize) {
        self.unmap(offset, len);
    }
    /// Blob `id` is gone: anything kept to map it again can go.
    fn forget(&self, _id: u32) {}
}

/// The host-visible region of emu's own machine: blobs mapped into the VM at `base + offset`.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub struct HvWindow {
    pub base: u64,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl ShmWindow for HvWindow {
    fn map(&self, _id: u32, host: Option<HostRange>, offset: u64, _len: usize) -> Result<(), String> {
        let host = host.ok_or("no host mapping")?;
        crate::hv::map_raw(host, self.base + offset).map_err(|e| format!("hv_vm_map: {e}"))
    }

    fn unmap(&self, offset: u64, len: usize) {
        let _ = crate::hv::unmap_raw(self.base + offset, len);
    }
}

pub struct GpuConfig {
    pub width: u32,
    pub height: u32,
    pub renderer: Arc<Renderer>,
    /// Guest physical base and size of the host-visible region (the base matters only to a
    /// transport that places the region itself, virtio-mmio).
    pub shm: (u64, u64),
    /// How blobs get into that region.
    pub window: Arc<dyn ShmWindow>,
    pub display: Arc<dyn DisplaySink>,
    /// Timing of each activation on stderr (`-verbose`): the last boot's release, the first
    /// command and the first fence.
    pub verbose: bool,
}

enum Event {
    Kick,
    Fence(Fence),
    Stop,
}

/// Where the renderer's fence callback posts: the active device's worker, tagged with that
/// device's generation. Set when the device activates and cleared when its worker stops, so a
/// fence the renderer signals for a torn-down device reaches no one.
static FENCE_SINK: Mutex<Option<(u64, Sender<Event>)>> = Mutex::new(None);

/// Generations of [`Gpu`] devices (one per machine run), for [`FENCE_SINK`].
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn fence_sink() -> MutexGuard<'static, Option<(u64, Sender<Event>)>> {
    FENCE_SINK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sends the renderer's fences to `tx`, the worker of device generation `generation`.
fn route_fences(generation: u64, tx: Sender<Event>) {
    *fence_sink() = Some((generation, tx));
}

/// Stops sending fences to generation `generation`; a newer device's route stays.
fn unroute_fences(generation: u64) {
    let mut sink = fence_sink();
    if sink.as_ref().is_some_and(|(g, _)| *g == generation) {
        *sink = None;
    }
}

/// Routes renderer fences to the active GPU worker. Pass to `Renderer::init`.
pub fn fence_handler() -> Arc<dyn Fn(Fence) + Send + Sync> {
    Arc::new(|f| {
        if let Some((_, tx)) = fence_sink().as_ref() {
            let _ = tx.send(Event::Fence(f));
        }
    })
}

/// Longest a reset waits for the renderer to signal the fences the guest still waits on, so the
/// work they stand for is done before the guest's mappings go and its contexts are released. They
/// normally signal within a frame; a renderer stuck on one costs a reset (or a shutdown) this
/// long, no more. One signalled later completes nothing ([`FenceIds`]).
const FENCE_SETTLE: Duration = Duration::from_millis(100);

/// The last fence id any GPU device gave the renderer (see [`FenceIds`]).
static LAST_FENCE_ID: AtomicU64 = AtomicU64::new(0);

/// The fence ids one activation of the device gives the renderer: one more than the last, from a
/// process-wide counter, instead of the guest's own ids. A fence the renderer still holds from
/// before a reset (stopping waits [`FENCE_SETTLE`] for them, no longer) can signal at any time
/// after, and a `Fence` says nothing of which boot made it; the guest's ids start again at 1 each
/// boot, so with them a late fence (ctx 1, ring 0, id 4000) would complete the next boot's
/// requests up to 4000 on that timeline before their work was done. Every id of this activation
/// is above `base`, the last id given out before it, so an earlier boot's fence is known
/// ([`FenceIds::stale`]) and retires nothing. On a timeline the ids keep the order the requests
/// arrived in, which is the order the renderer signals them; the guest's id goes back in the
/// reply, which is written before the fence is made.
#[derive(Clone, Copy)]
struct FenceIds {
    counter: &'static AtomicU64,
    base: u64,
}

impl FenceIds {
    /// Ids for an activation starting now: every one above those `counter` gave out before.
    fn after(counter: &'static AtomicU64) -> FenceIds {
        FenceIds { counter, base: counter.load(Ordering::Acquire) }
    }

    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Whether `f` is an earlier activation's fence (one the renderer signalled after its reset).
    fn stale(&self, f: &Fence) -> bool {
        f.fence_id <= self.base
    }
}

/// The renderer fence for request `h`, with id `id`, and the timeline it signals on.
fn fence_for(h: &Hdr, id: u64) -> ((u32, u8), Fence) {
    let ringed = h.flags & FLAG_RING_IDX != 0;
    let timeline = if ringed { (h.ctx_id, h.ring_idx) } else { (0, 0) };
    let flags = emu_gfxstream::FENCE_FLAG | if ringed { emu_gfxstream::FENCE_FLAG_RING_IDX } else { 0 };
    (timeline, Fence { flags, fence_id: id, ctx_id: timeline.0, ring_idx: timeline.1 })
}

pub struct Gpu {
    cfg: Arc<GpuConfig>,
    num_capsets: u32,
    generation: u64,
    events: Sender<Event>,
    rx: Option<Receiver<Event>>,
    worker: Option<JoinHandle<Receiver<Event>>>,
}

impl Gpu {
    pub fn new(cfg: GpuConfig) -> Gpu {
        let num_capsets = emu_gfxstream::capset::ALL.iter().filter(|&&c| cfg.renderer.capset_info(c).is_some()).count() as u32;
        let (events, rx) = channel();
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        Gpu { cfg: Arc::new(cfg), num_capsets, generation, events, rx: Some(rx), worker: None }
    }

    /// Stops the worker once it has given the guest's renderer state back, then stops routing
    /// fences to it.
    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.events.send(Event::Stop);
            if let Ok(rx) = h.join() {
                while rx.try_recv().is_ok() {}
                self.rx = Some(rx);
            }
        }
        unroute_fences(self.generation);
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
        // Event-idx: the worker drains both queues with kicks off, so commands the guest
        // queues while one runs (a long SUBMIT_3D, a flush) cost it no exit.
        F_VIRGL | F_RESOURCE_BLOB | F_CONTEXT_INIT | F_RING_INDIRECT_DESC | F_RING_EVENT_IDX
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
        route_fences(self.generation, self.events.clone());
        let capsets: Vec<u32> =
            emu_gfxstream::capset::ALL.iter().copied().filter(|&c| self.cfg.renderer.capset_info(c).is_some()).collect();
        let event_idx = act.features & F_RING_EVENT_IDX != 0;
        let mut w = Worker {
            mem: act.mem,
            irq: act.irq,
            ctrl: Queue::new(ctrl, event_idx),
            cursor: cursor.map(|c| Queue::new(c, event_idx)),
            cfg: self.cfg.clone(),
            capsets,
            resources: HashMap::new(),
            contexts: HashSet::new(),
            legacy_asg: LegacyAsg::default(),
            scanout: None,
            pending: Vec::new(),
            fence_ids: FenceIds::after(&LAST_FENCE_ID),
            timing: Timing::new(),
            req: Vec::new(),
            body: Vec::new(),
        };
        let handle = std::thread::Builder::new()
            .name("gpu".into())
            .spawn(move || {
                crate::device_thread();
                w.release_leftovers();
                w.run(&rx);
                w.teardown(&rx);
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
    /// Offset into the host-visible region and length the blob is mapped at (unmapped on unref).
    mapped: Option<(u64, usize)>,
    /// The context that created this blob; None for other resources.
    blob_of: Option<u32>,
    /// A blob's size, as the guest created it.
    blob_size: u64,
    /// A custom buffer's guest backing (`addr`, `len`), kept since the device may write it: the
    /// response page of address-space graphics the old way ([`legacy_asg`]). Other resources
    /// keep none.
    custom_backing: Option<Vec<(u64, u32)>>,
}

/// `RESOURCE_CREATE_3D` of a custom buffer (`PIPE_BUFFER`, `VIRGL_BIND_CUSTOM`): a pipe or
/// response page.
fn is_custom_buffer(target: u32, bind: u32) -> bool {
    const PIPE_BUFFER: u32 = 0;
    const VIRGL_BIND_CUSTOM: u32 = 1 << 17;
    target == PIPE_BUFFER && bind == VIRGL_BIND_CUSTOM
}

/// Writes `words` (little-endian) at the start of guest backing `backing`, across its entries.
fn write_words(mem: &GuestRam, backing: &[(u64, u32)], words: &[u32]) -> bool {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut rest = &bytes[..];
    for &(addr, len) in backing {
        if rest.is_empty() {
            break;
        }
        let n = rest.len().min(len as usize);
        if mem.write(addr, &rest[..n]).is_err() {
            return false;
        }
        rest = &rest[n..];
    }
    rest.is_empty()
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
    /// The renderer's id for the fence ([`FenceIds`]), not the guest's.
    fence_id: u64,
    head: u16,
    len: u32,
}

/// Takes the requests signalled fence `f` completes out of `pending`: those on its timeline with
/// a fence id up to its own.
fn retire(pending: &mut Vec<Pending>, f: &Fence) -> Vec<Pending> {
    let ringed = f.flags & emu_gfxstream::FENCE_FLAG_RING_IDX != 0;
    let timeline = if ringed { (f.ctx_id, f.ring_idx) } else { (0, 0) };
    let (done, rest) = std::mem::take(pending).into_iter().partition(|p| p.timeline == timeline && p.fence_id <= f.fence_id);
    *pending = rest;
    done
}

/// [`retire`] for a fence of this activation; an earlier one's ([`FenceIds::stale`]) retires
/// nothing.
fn complete(pending: &mut Vec<Pending>, ids: &FenceIds, f: &Fence) -> Vec<Pending> {
    if ids.stale(f) { Vec::new() } else { retire(pending, f) }
}

/// What a reset gives back to the renderer, in the order it does. The guest's mappings of blobs
/// (resource, guest physical address, length) go at once, so the guest never reaches memory the
/// renderer frees. The rest waits for the device's next activation ([`Leftovers`]): the
/// resources, whose iovecs point into guest RAM a machine reset frees, before the contexts, since
/// an unref detaches the resource from the contexts it is attached to; then the contexts, whose
/// ids and per-process renderer state (render threads, Vulkan instances) would otherwise outlive
/// the boot, and make the next boot's contexts of the same ids fail ("Failed to create process
/// resource"), leaving it without graphics. Each list is sorted.
#[derive(Debug, Default, PartialEq)]
struct Release {
    unmap: Vec<(u32, u64, usize)>,
    resources: Vec<u32>,
    contexts: Vec<u32>,
}

/// Resources and contexts a reset left in the renderer, released when a GPU device next
/// activates (on its worker, before the guest's first command), not at the reset. Destroying a
/// context hands its Vulkan instances to the renderer's cleanup thread; at a shutdown the process
/// then exits while that thread is still destroying them, racing the renderer's static
/// destructors (a SIGSEGV at exit, in one of four stops). A reset is followed by an activation; a
/// shutdown never is.
static LEFTOVERS: Mutex<Leftovers> = Mutex::new(Leftovers { resources: Vec::new(), contexts: Vec::new() });

#[derive(Debug, Default, PartialEq)]
struct Leftovers {
    resources: Vec<u32>,
    contexts: Vec<u32>,
}

impl Leftovers {
    /// Keeps `r`'s resources and contexts (its mappings are gone already) for the next release.
    fn keep(&mut self, r: Release) {
        self.resources.extend(r.resources);
        self.contexts.extend(r.contexts);
        for v in [&mut self.resources, &mut self.contexts] {
            v.sort_unstable();
            v.dedup();
        }
    }
}

fn leftovers() -> MutexGuard<'static, Leftovers> {
    LEFTOVERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The resource id [`watch_for_resource`] watches for; 0 once created (or when watching none).
static WATCHED: AtomicU32 = AtomicU32::new(0);

/// After a reset: notes when the next boot creates resource `id`. The renderer's color buffers
/// can take the guest's resource ids as their handles, and a boot allocates those the same way
/// every time, so the next boot's buffer may get the handle of the last boot's final post: a
/// different handle can't be the only sign of a new post.
pub fn watch_for_resource(id: u32) {
    WATCHED.store(id, Ordering::Relaxed);
}

/// Whether resource `id`, watched for with [`watch_for_resource`], has been created since.
pub fn resource_created(id: u32) -> bool {
    id != 0 && WATCHED.load(Ordering::Relaxed) != id
}

fn created(id: u32) {
    let _ = WATCHED.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
}

/// Empties the device's resource and context tables into a [`Release`].
fn release_all(resources: &mut HashMap<u32, Resource>, contexts: &mut HashSet<u32>) -> Release {
    let mut r = Release::default();
    for (id, res) in resources.drain() {
        if let Some((ipa, len)) = res.mapped {
            r.unmap.push((id, ipa, len));
        }
        r.resources.push(id);
    }
    r.contexts.extend(contexts.drain());
    r.unmap.sort_unstable();
    r.contexts.sort_unstable();
    r.resources.sort_unstable();
    r
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

    /// The response header: echoes fence fields (the guest driver signals its fence from the
    /// fence id in the response). On the stack: most replies are only this.
    fn reply(&self, typ: u32) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0..4].copy_from_slice(&typ.to_le_bytes());
        out[4..8].copy_from_slice(&(self.flags & (FLAG_FENCE | FLAG_RING_IDX)).to_le_bytes());
        out[8..16].copy_from_slice(&self.fence_id.to_le_bytes());
        out[16..20].copy_from_slice(&self.ctx_id.to_le_bytes());
        out[20] = self.ring_idx;
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

/// Whether the driver wants an interrupt for the queues that got used entries (`(used,
/// queue)`), as blk and vsock ask: a queue whose driver suppressed interrupts
/// (`VIRTQ_AVAIL_F_NO_INTERRUPT`, Linux's `virtqueue_disable_cb` while its dequeue worker
/// drains the ring) raises none. Every used queue is asked, so each records the notification.
fn wants_interrupt(mem: &GuestRam, queues: &mut [(bool, Option<&mut Queue>)]) -> bool {
    let mut raise = false;
    for (used, q) in queues.iter_mut() {
        if let (true, Some(q)) = (*used, q.as_mut()) {
            raise |= q.should_notify(mem);
        }
    }
    raise
}

/// [`Worker::backing`]. The entry count is checked against the request before anything is
/// reserved for it (it comes from the guest).
fn backing(mem: &GuestRam, req: &[u8], at: usize, n: u32) -> Option<Backing> {
    let n = n as usize;
    if n.checked_mul(16).and_then(|b| b.checked_add(at)).is_none_or(|end| end > req.len()) {
        return None;
    }
    let mut b = Backing::with_capacity(n);
    for e in req[at..at + n * 16].as_chunks::<16>().0 {
        b.push(mem.host_range(le64(e, 0), le32(e, 8) as usize)?);
    }
    Some(b)
}

/// The guest memory entries of a request (`addr: u64, len: u32, pad: u32`), as they are; the
/// caller has checked they're all in the request ([`backing`]).
fn guest_entries(req: &[u8], at: usize, n: u32) -> Vec<(u64, u32)> {
    req.get(at..at + n as usize * 16).map_or_else(Vec::new, |b| b.as_chunks::<16>().0.iter().map(|e| (le64(e, 0), le32(e, 8))).collect())
}

/// A two-word gfxstream command (`GFXSTREAM_CONTEXT_CREATE` / `_PING` of `resource`).
fn asg_command(op: u32, resource: u32) -> [u8; 8] {
    let mut c = [0u8; 8];
    c[..4].copy_from_slice(&op.to_le_bytes());
    c[4..].copy_from_slice(&resource.to_le_bytes());
    c
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
    /// Contexts the guest created and hasn't destroyed.
    contexts: HashSet<u32>,
    /// Contexts that set up address-space graphics the way API 31–33 guests do.
    legacy_asg: LegacyAsg,
    scanout: Option<Scanout>,
    pending: Vec<Pending>,
    fence_ids: FenceIds,
    timing: Timing,
    /// The current control request, reused across requests (command streams are KBs to MBs).
    req: Vec<u8>,
    /// The current reply's payload after its header, reused likewise (most replies have none).
    body: Vec<u8>,
}

/// When this activation's first command came and its first fence signalled (`-verbose`).
struct Timing {
    activated: Instant,
    first_command: bool,
    /// The first fence made: its id and when.
    first_fence: Option<(u64, Instant)>,
    fence_reported: bool,
}

impl Timing {
    fn new() -> Timing {
        Timing { activated: Instant::now(), first_command: false, first_fence: None, fence_reported: false }
    }

    fn ms(&self, t: Instant) -> u128 {
        t.saturating_duration_since(self.activated).as_millis()
    }
}

impl Worker {
    fn run(&mut self, rx: &Receiver<Event>) {
        while let Ok(ev) = rx.recv() {
            let mut ev = Some(ev);
            let (mut ctrl, mut cursor) = (false, false);
            while let Some(e) = ev.take() {
                match e {
                    Event::Stop => return,
                    Event::Kick => {
                        let (c, k) = self.process();
                        (ctrl, cursor) = (ctrl | c, cursor | k);
                    }
                    Event::Fence(f) => ctrl |= self.complete_fences(f),
                }
                ev = rx.try_recv().ok();
            }
            if wants_interrupt(&self.mem, &mut [(ctrl, Some(&mut self.ctrl)), (cursor, self.cursor.as_mut())]) {
                self.irq.raise(INT_USED_RING);
            }
        }
    }

    /// Empties the device once the fences the guest still waits on have signalled or
    /// [`FENCE_SETTLE`] has passed: removes the guest's blob mappings and leaves its resources and
    /// contexts for the next activation ([`Release`]). Writes nothing to guest memory: after a
    /// reset the rings are no longer the device's.
    fn teardown(&mut self, rx: &Receiver<Event>) {
        let t = Instant::now();
        let waited = self.pending.len();
        let deadline = t + FENCE_SETTLE;
        while !self.pending.is_empty() {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Event::Fence(f)) => drop(complete(&mut self.pending, &self.fence_ids, &f)),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        let unsignalled = self.pending.len();
        self.pending.clear();
        self.scanout = None;
        let r = &self.cfg.renderer;
        let release = release_all(&mut self.resources, &mut self.contexts);
        self.legacy_asg.clear();
        for &(id, offset, len) in &release.unmap {
            self.cfg.window.unmap_stopping(offset, len);
            let _ = r.unmap(id);
        }
        for &id in &release.resources {
            self.cfg.window.forget(id);
        }
        if self.cfg.verbose || trace() || unsignalled > 0 {
            eprintln!(
                "emu: gpu reset: {} mappings removed in {} ms, {} contexts and {} resources left for the next boot; {unsignalled} of {waited} fences never signalled",
                release.unmap.len(),
                t.elapsed().as_millis(),
                release.contexts.len(),
                release.resources.len(),
            );
        }
        leftovers().keep(release);
    }

    /// Releases what the last reset left in the renderer ([`Leftovers`]), before the guest's
    /// first command.
    fn release_leftovers(&self) {
        let left = std::mem::take(&mut *leftovers());
        let r = &self.cfg.renderer;
        let t = Instant::now();
        for &id in &left.resources {
            r.resource_unref(id);
        }
        let unref = t.elapsed();
        for &ctx in &left.contexts {
            r.context_destroy(ctx);
        }
        if self.cfg.verbose || trace() {
            eprintln!(
                "emu: gpu: released {} resources in {} ms and {} contexts in {} ms of the last boot",
                left.resources.len(),
                unref.as_millis(),
                left.contexts.len(),
                (t.elapsed() - unref).as_millis()
            );
        }
    }

    /// Runs every available request; returns whether the control and cursor queues got used
    /// entries.
    /// Both queues are drained with kicks off ([`Queue::pop_draining`]).
    fn process(&mut self) -> (bool, bool) {
        let (mut used, mut cursor_used) = (false, false);
        let mut chain = Chain::default();
        if let Some(q) = self.cursor.as_mut() {
            while let Ok(true) = q.pop_draining(&self.mem, &mut chain) {
                let _ = q.add_used(&self.mem, chain.head, 0);
                cursor_used = true;
            }
        }
        loop {
            match self.ctrl.pop_draining(&self.mem, &mut chain) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    eprintln!("emu: gpu ctrl: {e}");
                    break;
                }
            }
            let mut req = std::mem::take(&mut self.req);
            if chain.read_all_into(&self.mem, 32 << 20, &mut req).is_err() {
                req.clear();
            }
            let mut body = std::mem::take(&mut self.body);
            body.clear();
            let (hdr, typ) = match Hdr::parse(&req) {
                Some(h) => (h, self.handle(&h, &mut req, &mut body)),
                None => (Hdr::default(), cmd::ERR_UNSPEC),
            };
            if typ >= cmd::ERR_UNSPEC {
                body.clear(); // an error is the header alone
            }
            let resp = hdr.reply(typ);
            let len = chain.write_parts(&self.mem, &[&resp, &body]).unwrap_or(0);
            self.body = body;
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
                let (timeline, f) = fence_for(&hdr, self.fence_ids.next());
                if trace() {
                    eprintln!("gpu: fence {} for the guest's {}", f.fence_id, hdr.fence_id);
                }
                if self.cfg.renderer.create_fence(f).is_ok() {
                    self.timing.first_fence.get_or_insert((f.fence_id, Instant::now()));
                    self.pending.push(Pending { timeline, fence_id: f.fence_id, head: chain.head, len });
                    continue;
                }
            }
            let _ = self.ctrl.add_used(&self.mem, chain.head, len);
            used = true;
        }
        (used, cursor_used)
    }

    fn complete_fences(&mut self, f: Fence) -> bool {
        if trace() {
            eprintln!(
                "gpu: fence signaled id {} ctx {} ring {} flags {:#x} (pending {}){}",
                f.fence_id,
                f.ctx_id,
                f.ring_idx,
                f.flags,
                self.pending.len(),
                if self.fence_ids.stale(&f) { ", from before the reset" } else { "" }
            );
        }
        let done = complete(&mut self.pending, &self.fence_ids, &f);
        if let (false, true, Some((id, made))) = (self.timing.fence_reported, self.cfg.verbose, self.timing.first_fence)
            && !self.fence_ids.stale(&f)
            && f.fence_id >= id
        {
            self.timing.fence_reported = true;
            let now = Instant::now();
            eprintln!(
                "emu: gpu: first fence signalled {} ms after it was made ({} ms after activation)",
                now.saturating_duration_since(made).as_millis(),
                self.timing.ms(now)
            );
        }
        for p in &done {
            let _ = self.ctrl.add_used(&self.mem, p.head, p.len);
        }
        !done.is_empty()
    }

    /// Guest memory entries (`addr: u64, len: u32, pad: u32`) → the renderer's iovecs, built in
    /// place. None if an entry is outside guest RAM or the request is shorter than `n` entries.
    fn backing(&self, req: &[u8], at: usize, n: u32) -> Option<Backing> {
        backing(&self.mem, req, at, n)
    }

    /// Runs one control request; returns the response type. A response payload (after the
    /// header the caller writes) goes into `body`, which arrives empty.
    fn handle(&mut self, h: &Hdr, req: &mut [u8], body: &mut Vec<u8>) -> u32 {
        if !self.timing.first_command {
            self.timing.first_command = true;
            if self.cfg.verbose {
                eprintln!("emu: gpu: first command {:#06x} {} ms after activation", h.typ, self.timing.ms(Instant::now()));
            }
        }
        let r = &self.cfg.renderer;
        let ok = cmd::OK_NODATA;
        let or = |res: Result<(), emu_gfxstream::Error>, err: u32| if res.is_ok() { ok } else { err };
        match h.typ {
            cmd::GET_DISPLAY_INFO => {
                for i in 0..16u32 {
                    let (w, hgt, en): (u32, u32, u32) = if i == 0 { (self.cfg.width, self.cfg.height, 1) } else { (0, 0, 0) };
                    for v in [0u32, 0, w, hgt, en, 0] {
                        body.extend_from_slice(&v.to_le_bytes());
                    }
                }
                cmd::OK_DISPLAY_INFO
            }
            cmd::GET_CAPSET_INFO => {
                let idx = le32(req, 24) as usize;
                match self.capsets.get(idx).and_then(|&id| r.capset_info(id).map(|(v, s)| (id, v, s))) {
                    Some((id, ver, size)) => {
                        for v in [id, ver, size, 0] {
                            body.extend_from_slice(&v.to_le_bytes());
                        }
                        cmd::OK_CAPSET_INFO
                    }
                    None => cmd::ERR_INVALID_PARAMETER,
                }
            }
            cmd::GET_CAPSET => {
                body.extend_from_slice(&r.capset(le32(req, 24), le32(req, 28)));
                cmd::OK_CAPSET
            }
            cmd::CTX_CREATE => {
                let nlen = (le32(req, 24) as usize).min(64);
                let init = le32(req, 28);
                let name = req.get(32..32 + nlen).unwrap_or(&[]);
                let res = r.context_create(h.ctx_id, name, init);
                if res.is_ok() {
                    self.contexts.insert(h.ctx_id);
                }
                or(res, cmd::ERR_UNSPEC)
            }
            cmd::CTX_DESTROY => {
                self.contexts.remove(&h.ctx_id);
                self.legacy_asg.destroy(h.ctx_id);
                r.context_destroy(h.ctx_id);
                ok
            }
            cmd::CTX_ATTACH_RESOURCE => {
                r.ctx_attach_resource(h.ctx_id, le32(req, 24));
                ok
            }
            cmd::CTX_DETACH_RESOURCE => {
                r.ctx_detach_resource(h.ctx_id, le32(req, 24));
                ok
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
                self.create(id, args, width, height, format)
            }
            cmd::RESOURCE_CREATE_3D => {
                let v: Vec<u32> = (0..11).map(|i| le32(req, 24 + i * 4)).collect();
                let custom = is_custom_buffer(v[1], v[3]);
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
                let res = self.create(v[0], args, v[4], v[5], v[2]);
                if custom && let Some(r) = self.resources.get_mut(&v[0]) {
                    r.custom_backing = Some(Vec::new());
                }
                res
            }
            cmd::RESOURCE_UNREF => {
                let id = le32(req, 24);
                self.legacy_asg.unref(id);
                if let Some(res) = self.resources.remove(&id) {
                    if let Some((offset, len)) = res.mapped {
                        self.cfg.window.unmap(offset, len);
                        let _ = r.unmap(id);
                    }
                    self.cfg.window.forget(id);
                    if self.scanout.is_some_and(|s| s.resource == id) {
                        self.scanout = None;
                    }
                }
                r.resource_unref(id);
                ok
            }
            cmd::RESOURCE_ATTACH_BACKING => {
                let (id, n) = (le32(req, 24), le32(req, 28));
                match self.backing(req, 32, n) {
                    Some(b) => {
                        if let Some(keep) = self.resources.get_mut(&id).and_then(|r| r.custom_backing.as_mut()) {
                            *keep = guest_entries(req, 32, n);
                        }
                        or(r.attach_backing(id, b), cmd::ERR_UNSPEC)
                    }
                    None => cmd::ERR_INVALID_PARAMETER,
                }
            }
            cmd::RESOURCE_DETACH_BACKING => {
                r.detach_backing(le32(req, 24));
                ok
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let (x, y, w, hgt) = rect(req, 24);
                let offset = le64(req, 40);
                let id = le32(req, 48);
                let b = Box3 { x, y, z: 0, w, h: hgt, d: 1 };
                or(r.transfer_write(id, 0, 0, 0, 0, b, offset), cmd::ERR_INVALID_RESOURCE_ID)
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
                or(res, cmd::ERR_INVALID_RESOURCE_ID)
            }
            cmd::SUBMIT_3D => {
                let size = le32(req, 24) as usize;
                // The command stream in place in the request buffer: one copy out of guest
                // memory, as crosvm and QEMU make, so the renderer never decodes memory the guest
                // can still change.
                let Some(buf) = req.get_mut(32..32 + size) else { return cmd::ERR_INVALID_PARAMETER };
                let resources = &self.resources;
                let ctx_blob = |id: u32| resources.get(&id).is_some_and(|res| res.blob_of == Some(h.ctx_id));
                match self.legacy_asg.submit(h.ctx_id, buf, ctx_blob) {
                    legacy_asg::Submit::Pass => or(r.submit(h.ctx_id, buf), cmd::ERR_UNSPEC),
                    legacy_asg::Submit::Done => ok,
                    legacy_asg::Submit::Respond { resource, words } => {
                        let page = self.resources.get(&resource).and_then(|res| res.custom_backing.as_deref());
                        if page.is_some_and(|b| write_words(&self.mem, b, &words)) { ok } else { cmd::ERR_INVALID_RESOURCE_ID }
                    }
                    legacy_asg::Submit::Ping { resource } => {
                        let mut ping = asg_command(legacy_asg::OP_PING, resource);
                        or(r.submit(h.ctx_id, &mut ping), cmd::ERR_UNSPEC)
                    }
                }
            }
            cmd::RESOURCE_CREATE_BLOB => {
                let (id, blob_mem, blob_flags, n) = (le32(req, 24), le32(req, 28), le32(req, 32), le32(req, 36));
                let (blob_id, size) = (le64(req, 40), le64(req, 48));
                let Some(backing) = self.backing(req, 56, n) else { return cmd::ERR_INVALID_PARAMETER };
                // An API 31–33 guest's ring, by the host memory id it was given: the renderer's
                // ring blob (blob id 0), then the new protocol's create for it.
                let ring = self.legacy_asg.is_ring(h.ctx_id, blob_id);
                let blob_id = if ring { 0 } else { blob_id };
                match r.create_blob(h.ctx_id, id, blob_mem, blob_flags, blob_id, size, backing) {
                    Ok(()) => {
                        created(id);
                        self.resources.insert(id, Resource { blob_of: Some(h.ctx_id), blob_size: size, ..Default::default() });
                        if ring {
                            let mut create = asg_command(legacy_asg::OP_CREATE, id);
                            if let Err(e) = r.submit(h.ctx_id, &mut create) {
                                eprintln!("emu: gpu: address-space graphics ring {id} on context {}: {e}", h.ctx_id);
                                return cmd::ERR_UNSPEC;
                            }
                            self.legacy_asg.ring_created(h.ctx_id, id);
                        }
                        ok
                    }
                    Err(e) => {
                        eprintln!("emu: gpu: create_blob {id}: {e}");
                        cmd::ERR_UNSPEC
                    }
                }
            }
            cmd::RESOURCE_MAP_BLOB => self.map_blob(le32(req, 24), le64(req, 32), body),
            cmd::RESOURCE_UNMAP_BLOB => {
                let id = le32(req, 24);
                if let Some((offset, len)) = self.resources.get_mut(&id).and_then(|res| res.mapped.take()) {
                    self.cfg.window.unmap(offset, len);
                    let _ = r.unmap(id);
                }
                ok
            }
            cmd::SET_SCANOUT | cmd::SET_SCANOUT_BLOB => {
                let scanout_id = le32(req, 40);
                if scanout_id != 0 {
                    return cmd::ERR_INVALID_SCANOUT_ID;
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
                ok
            }
            cmd::RESOURCE_FLUSH => {
                let id = le32(req, 40);
                if let Some(s) = self.scanout.filter(|s| s.resource == id) {
                    self.present(s);
                }
                ok
            }
            cmd::UPDATE_CURSOR | cmd::MOVE_CURSOR => ok,
            _ => cmd::ERR_UNSPEC,
        }
    }

    fn create(&mut self, id: u32, args: ResourceCreateArgs, width: u32, height: u32, format: u32) -> u32 {
        match self.cfg.renderer.resource_create(args, &[]) {
            Ok(()) => {
                created(id);
                self.resources.insert(id, Resource { width, height, format, ..Default::default() });
                cmd::OK_NODATA
            }
            Err(e) => {
                eprintln!("emu: gpu: resource_create {id}: {e}");
                cmd::ERR_UNSPEC
            }
        }
    }

    fn map_blob(&mut self, id: u32, offset: u64, body: &mut Vec<u8>) -> u32 {
        let r = &self.cfg.renderer;
        let Some(res) = self.resources.get_mut(&id) else { return cmd::ERR_INVALID_RESOURCE_ID };
        let host = if self.cfg.window.needs_host_mapping() {
            match r.map(id) {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("emu: gpu: map blob {id}: {e}");
                    return cmd::ERR_UNSPEC;
                }
            }
        } else {
            None
        };
        let size = self.cfg.shm.1;
        let len = (host.map_or(res.blob_size as usize, |h| h.len())).div_ceil(4096) * 4096;
        if len == 0 || offset.checked_add(len as u64).is_none_or(|end| end > size) {
            let _ = r.unmap(id);
            return cmd::ERR_INVALID_PARAMETER;
        }
        // `host` stays mapped until we unmap it (RESOURCE_UNMAP_BLOB / UNREF / teardown).
        let range = host.map(|h| emu_gfxstream::page_range(h, len));
        if let Err(e) = self.cfg.window.map(id, range, offset, len) {
            eprintln!("emu: gpu: map blob {id} at +{offset:#x} ({len:#x} bytes): {e}");
            let _ = r.unmap(id);
            return cmd::ERR_UNSPEC;
        }
        res.mapped = Some((offset, len));
        body.extend_from_slice(&(r.map_info(id) & 0x0f).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        cmd::OK_MAP_INFO
    }

    /// Reads the flushed scanout straight into the display's buffer: one copy, not a readback
    /// into a staging frame and a second copy into the store.
    fn present(&mut self, s: Scanout) {
        let stride = if s.stride == 0 { s.width * 4 } else { s.stride };
        let b = Box3 { x: 0, y: 0, z: 0, w: s.width, h: s.height, d: 1 };
        let renderer = &self.cfg.renderer;
        self.cfg
            .display
            .present_with(s.width, s.height, stride, s.format, &mut |buf| renderer.read_into(s.resource, stride, b, buf).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::queue::tests::{Driver, RAM_BASE};

    const NO_INTERRUPT: u16 = 1;

    fn ram() -> GuestRam {
        GuestRam::new(RAM_BASE, 0x40_0000).expect("ram")
    }

    #[test]
    fn interrupts_follow_the_drivers_suppression() {
        let mem = ram();
        let (mut cd, mut kd) = (Driver::new(8), Driver::at(RAM_BASE + 0x8000, 8));
        let (mut ctrl, mut cursor) = (Queue::new(cd.cfg, false), Queue::new(kd.cfg, false));
        let complete = |drv: &mut Driver, q: &mut Queue| {
            drv.push(&mem, &[(RAM_BASE + 0x2_0000, 24, true)]);
            let c = q.pop(&mem).expect("pop").expect("chain");
            q.add_used(&mem, c.head, 24).expect("used");
        };
        complete(&mut cd, &mut ctrl);
        assert!(wants_interrupt(&mem, &mut [(true, Some(&mut ctrl)), (false, Some(&mut cursor))]), "interrupts on");
        assert!(!wants_interrupt(&mem, &mut [(false, Some(&mut ctrl)), (false, None)]), "nothing used: no interrupt");
        // The driver drains the control queue with callbacks disabled.
        mem.write_u16(cd.cfg.avail, NO_INTERRUPT).expect("flags");
        complete(&mut cd, &mut ctrl);
        assert!(!wants_interrupt(&mem, &mut [(true, Some(&mut ctrl)), (false, Some(&mut cursor))]), "suppressed");
        // A cursor completion still interrupts when its queue wants it.
        complete(&mut kd, &mut cursor);
        assert!(wants_interrupt(&mem, &mut [(true, Some(&mut ctrl)), (true, Some(&mut cursor))]));
        mem.write_u16(kd.cfg.avail, NO_INTERRUPT).expect("flags");
        assert!(!wants_interrupt(&mem, &mut [(true, Some(&mut ctrl)), (true, Some(&mut cursor))]), "both suppressed");
        mem.write_u16(cd.cfg.avail, 0).expect("flags");
        assert!(wants_interrupt(&mem, &mut [(true, Some(&mut ctrl)), (true, None)]), "re-enabled");
    }

    fn pending(timeline: (u32, u8), fence_id: u64) -> Pending {
        Pending { timeline, fence_id, head: fence_id as u16, len: 24 }
    }

    fn fence(ring: Option<(u32, u8)>, fence_id: u64) -> Fence {
        let (ctx_id, ring_idx) = ring.unwrap_or((0, 0));
        let flags = emu_gfxstream::FENCE_FLAG | if ring.is_some() { emu_gfxstream::FENCE_FLAG_RING_IDX } else { 0 };
        Fence { flags, fence_id, ctx_id, ring_idx }
    }

    #[test]
    fn a_fence_retires_its_own_timeline_up_to_its_id() {
        let mut p = vec![pending((0, 0), 1), pending((0, 0), 3), pending((5, 1), 2), pending((5, 0), 2), pending((0, 0), 4)];
        let ids = |v: &[Pending]| v.iter().map(|p| (p.timeline, p.fence_id)).collect::<Vec<_>>();
        assert_eq!(ids(&retire(&mut p, &fence(None, 3))), [((0, 0), 1), ((0, 0), 3)]);
        assert_eq!(ids(&retire(&mut p, &fence(Some((5, 1)), 9))), [((5, 1), 2)], "another ring of the context stays");
        assert!(retire(&mut p, &fence(Some((6, 0)), 9)).is_empty(), "no requests on that timeline");
        assert_eq!(ids(&p), [((5, 0), 2), ((0, 0), 4)]);
    }

    /// The renderer as far as fences go: it holds the fences it was given until told to signal.
    #[derive(Default)]
    struct FakeRenderer {
        held: Vec<Fence>,
    }

    impl FakeRenderer {
        /// What the worker does with a fenced request: a fence made with the activation's next id.
        fn fenced(&mut self, ids: &FenceIds, pending: &mut Vec<Pending>, ctx: u32, ring: u8, guest_id: u64) {
            let h = Hdr { typ: cmd::SUBMIT_3D, flags: FLAG_FENCE | FLAG_RING_IDX, fence_id: guest_id, ctx_id: ctx, ring_idx: ring };
            let (timeline, f) = fence_for(&h, ids.next());
            self.held.push(f);
            pending.push(Pending { timeline, fence_id: f.fence_id, head: guest_id as u16, len: 24 });
        }

        /// Signals the fence it was given `nth` (0 first).
        fn signal(&self, nth: usize) -> Fence {
            self.held[nth]
        }
    }

    #[test]
    fn a_fence_from_before_a_reset_completes_nothing_of_the_next_boot() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut renderer = FakeRenderer::default();
        let guest_ids = |v: &[Pending]| v.iter().map(|p| p.head).collect::<Vec<_>>();
        // The last boot: fences up to the guest's 4000 on (ctx 1, ring 0); the last hasn't
        // signalled when the reset drops what was pending.
        let old = FenceIds::after(&COUNTER);
        let mut pending = Vec::new();
        for id in [3998, 3999, 4000] {
            renderer.fenced(&old, &mut pending, 1, 0, id);
        }
        assert_eq!(guest_ids(&complete(&mut pending, &old, &renderer.signal(1))), [3998, 3999]);
        pending.clear();
        // The next boot, its fence ids from 1 again on the same timeline.
        let new = FenceIds::after(&COUNTER);
        for id in 1..=5 {
            renderer.fenced(&new, &mut pending, 1, 0, id);
        }
        // The renderer signals the last boot's fence 4000 late: nothing of the new boot is done.
        let late = renderer.signal(2);
        assert!(new.stale(&late));
        assert!(complete(&mut pending, &new, &late).is_empty(), "a late fence retired the next boot's requests");
        assert_eq!(pending.len(), 5);
        // The new boot's own fences retire its requests up to theirs, on their timeline.
        assert_eq!(guest_ids(&complete(&mut pending, &new, &renderer.signal(5))), [1, 2, 3]);
        assert!(!new.stale(&renderer.signal(3)));
        renderer.fenced(&new, &mut pending, 2, 0, 6);
        assert_eq!(guest_ids(&complete(&mut pending, &new, &renderer.signal(8))), [6], "another context's timeline");
        assert_eq!(guest_ids(&complete(&mut pending, &new, &renderer.signal(7))), [4, 5]);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_fence_keeps_its_timeline_and_the_guest_id_stays_in_the_reply() {
        let ring = Hdr { typ: cmd::SUBMIT_3D, flags: FLAG_FENCE | FLAG_RING_IDX, fence_id: 77, ctx_id: 3, ring_idx: 1 };
        let (timeline, f) = fence_for(&ring, 9001);
        assert_eq!(timeline, (3, 1));
        assert_eq!(f, fence(Some((3, 1)), 9001));
        assert_eq!(le64(&ring.reply(cmd::OK_NODATA), 8), 77, "the guest signals its own id");
        let global = Hdr { flags: FLAG_FENCE, ..ring };
        let (timeline, f) = fence_for(&global, 9002);
        assert_eq!(timeline, (0, 0));
        assert_eq!(f, fence(None, 9002), "the global timeline names no context");
    }

    #[test]
    fn a_reset_releases_every_mapping_context_and_resource() {
        let mut resources = HashMap::from([
            (7, Resource { width: 720, height: 1280, format: 1, ..Default::default() }),
            (3, Resource { mapped: Some((0x2_0000_0000, 0x4000)), ..Default::default() }),
            (9, Resource { mapped: Some((0x2_0001_0000, 0x1000)), ..Default::default() }),
            (1, Resource::default()),
        ]);
        let mut contexts = HashSet::from([4, 2]);
        let r = release_all(&mut resources, &mut contexts);
        assert_eq!(
            r,
            Release {
                unmap: vec![(3, 0x2_0000_0000, 0x4000), (9, 0x2_0001_0000, 0x1000)],
                resources: vec![1, 3, 7, 9],
                contexts: vec![2, 4],
            }
        );
        assert!(resources.is_empty() && contexts.is_empty(), "the next boot starts empty");
        assert_eq!(release_all(&mut resources, &mut contexts), Release::default(), "a second reset has nothing to give back");
    }

    #[test]
    fn a_watched_resource_is_seen_when_the_next_boot_creates_it() {
        watch_for_resource(3);
        assert!(!resource_created(3), "not yet");
        created(5);
        assert!(!resource_created(3), "another resource");
        created(3);
        assert!(resource_created(3));
        created(3);
        assert!(resource_created(3), "and stays seen");
        assert!(!resource_created(0), "id 0 is never a resource");
    }

    #[test]
    fn leftovers_wait_for_the_next_activation_and_are_released_once() {
        let mut left = Leftovers::default();
        left.keep(Release { unmap: vec![(3, 0x2_0000_0000, 0x4000)], resources: vec![1, 3], contexts: vec![2] });
        // A second reset before any activation (a driver reset, then the machine's): kept too,
        // each id once.
        left.keep(Release { unmap: vec![], resources: vec![3, 5], contexts: vec![4, 2] });
        assert_eq!(left, Leftovers { resources: vec![1, 3, 5], contexts: vec![2, 4] });
        // The activation takes them all; the one after has nothing to release.
        assert_eq!(std::mem::take(&mut left).contexts, [2, 4]);
        assert_eq!(left, Leftovers::default());
    }

    #[test]
    fn fences_reach_only_the_active_device() {
        let handler = fence_handler();
        let (old_tx, old_rx) = channel();
        let (new_tx, new_rx) = channel();
        let (old, new) = (GENERATION.fetch_add(1, Ordering::Relaxed), GENERATION.fetch_add(1, Ordering::Relaxed));
        route_fences(old, old_tx);
        handler(fence(None, 1));
        assert!(matches!(old_rx.try_recv(), Ok(Event::Fence(f)) if f.fence_id == 1));
        // The device reset: its worker stopped, and a fence signalled late reaches no one.
        unroute_fences(old);
        handler(fence(None, 2));
        assert!(old_rx.try_recv().is_err());
        // The next boot's device; the old one going away late doesn't take its route.
        route_fences(new, new_tx);
        unroute_fences(old);
        handler(fence(None, 3));
        assert!(matches!(new_rx.try_recv(), Ok(Event::Fence(f)) if f.fence_id == 3));
        assert!(old_rx.try_recv().is_err());
        unroute_fences(new);
        assert!(fence_sink().is_none());
    }

    fn entries(list: &[(u64, u32)]) -> Vec<u8> {
        let mut req = vec![0xAA; 8]; // whatever precedes the entries
        for &(addr, len) in list {
            req.extend_from_slice(&addr.to_le_bytes());
            req.extend_from_slice(&len.to_le_bytes());
            req.extend_from_slice(&[0; 4]);
        }
        req
    }

    #[test]
    fn backing_entries_become_host_ranges_or_nothing() {
        let mem = ram();
        let req = entries(&[(RAM_BASE, 4096), (RAM_BASE + 0x3000, 100)]);
        assert_eq!(backing(&mem, &req, 8, 2).map(|b| b.len()), Some(2));
        assert_eq!(backing(&mem, &req, 8, 0).map(|b| b.len()), Some(0));
        assert!(backing(&mem, &req, 8, 3).is_none(), "more entries than the request holds");
        assert!(backing(&mem, &req, 8, u32::MAX).is_none(), "a huge count reserves nothing");
        let outside = entries(&[(RAM_BASE, 4096), (RAM_BASE + 0x40_0000, 1)]);
        assert!(backing(&mem, &outside, 8, 2).is_none(), "an entry outside guest RAM");
        let wraps = entries(&[(u64::MAX - 10, 4096)]);
        assert!(backing(&mem, &wraps, 8, 1).is_none());
    }

    #[test]
    fn a_response_page_gets_the_answer_across_its_entries() {
        let mem = ram();
        // A response page in two pieces, as ATTACH_BACKING lists them.
        let req = entries(&[(RAM_BASE + 0x5000, 12), (RAM_BASE + 0x9000, 4096)]);
        let page = guest_entries(&req, 8, 2);
        assert_eq!(page, [(RAM_BASE + 0x5000, 12), (RAM_BASE + 0x9000, 4096)]);
        let words: Vec<u32> = (1..=10).collect();
        assert!(write_words(&mem, &page, &words));
        let mut got = [0u8; 40];
        mem.read(RAM_BASE + 0x5000, &mut got[..12]).expect("first");
        mem.read(RAM_BASE + 0x9000, &mut got[12..]).expect("second");
        assert_eq!(got.as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w)).collect::<Vec<_>>(), words);
        assert!(!write_words(&mem, &page[..1], &words), "a page too small for the answer");
        assert!(guest_entries(&req, 8, 3).is_empty(), "more entries than the request holds");
        assert!(is_custom_buffer(0, 1 << 17) && !is_custom_buffer(2, 1 << 17) && !is_custom_buffer(0, 2));
    }

    /// One control command through the queue as the worker runs it (pop, read, parse, reply,
    /// write, used), its reply built in a fresh Vec per command (before) vs a header on the
    /// stack and a reused body buffer (now). Interleaved, median of 7.
    ///     cargo test --release -p emu-vmm gpu_command_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn gpu_command_cost() {
        const N: u32 = 200_000;
        let mem = ram();
        let (req_at, resp_at) = (RAM_BASE + 0x2_0000, RAM_BASE + 0x2_1000);
        let mut hdr = [0u8; 24];
        hdr[..4].copy_from_slice(&cmd::CTX_ATTACH_RESOURCE.to_le_bytes());
        hdr[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
        mem.write(req_at, &hdr).expect("req");
        let run = |reuse: bool| {
            let mut drv = Driver::new(256);
            let mut q = Queue::new(drv.cfg, false);
            let (mut req, mut body) = (Vec::new(), Vec::<u8>::with_capacity(64));
            let t = std::time::Instant::now();
            for _ in 0..N {
                drv.push(&mem, &[(req_at, 32, false), (resp_at, 24, true)]);
                let c = q.pop(&mem).expect("pop").expect("chain");
                c.read_all_into(&mem, 32 << 20, &mut req).expect("read");
                let h = Hdr::parse(&req).expect("hdr");
                let len = if reuse {
                    // Now: the header on the stack, the (empty) body in a reused buffer.
                    body.clear();
                    let typ = std::hint::black_box(cmd::OK_NODATA);
                    c.write_parts(&mem, &[&h.reply(typ), &body]).expect("write")
                } else {
                    // Before: every reply a fresh Vec.
                    let mut out = Vec::with_capacity(24);
                    out.extend_from_slice(&cmd::OK_NODATA.to_le_bytes());
                    out.extend_from_slice(&(h.flags & (FLAG_FENCE | FLAG_RING_IDX)).to_le_bytes());
                    out.extend_from_slice(&h.fence_id.to_le_bytes());
                    out.extend_from_slice(&h.ctx_id.to_le_bytes());
                    out.extend_from_slice(&[h.ring_idx, 0, 0, 0]);
                    c.write_all(&mem, &std::hint::black_box(out)).expect("write")
                };
                q.add_used(&mem, c.head, len).expect("used");
            }
            t.elapsed().as_secs_f64() * 1e9 / f64::from(N)
        };
        let (mut fresh, mut reused) = (Vec::new(), Vec::new());
        for _ in 0..7 {
            fresh.push(run(false));
            reused.push(run(true));
        }
        fresh.sort_by(f64::total_cmp);
        reused.sort_by(f64::total_cmp);
        println!("gpu command through the queue: {:.0} ns (reply in a fresh Vec) -> {:.0} ns (header on the stack)", fresh[3], reused[3]);
    }
}
