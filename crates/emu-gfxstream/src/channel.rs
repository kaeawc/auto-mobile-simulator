//! The renderer's own GLES stream channel, for guests whose GLES encoder talks over the goldfish
//! `opengles` pipe (API 29 and 30 images, `qemu.gltransport=pipe`).
//!
//! The stock emulator serves that pipe from `libandroid-emu-shared` (`EmuglPipe`), which hands each
//! guest connection to a `RenderChannel` the renderer makes; the renderer library itself has no
//! `opengles` pipe service. emu makes the same channel: gfxstream's `RenderChannelImpl`, whose
//! constructor starts the connection's render thread, exported with its methods. Guest bytes go
//! in with `tryWrite`, the render thread's replies come out with `readBefore`; both take the
//! channel's own lock, so the guest's writes and a host reader may run on different threads. No
//! event callback is installed (it would mean building a libc++ `std::function` here): the
//! channel's default one does nothing, and a reader blocked in `readBefore` learns of new data
//! from the queue's condition variable instead.
//!
//! The channel's shape is checked before it is trusted: the allocation the renderer's own
//! `createRenderChannel` makes for one (decoded from its code) must fit the block emu constructs
//! it in, and the constructed object must carry `RenderChannelImpl`'s vtable. Anything missing or
//! different makes [`Renderer::open_channel`] fail; it never guesses.
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{Error, Renderer, Result, macho};

/// `gfxstream::SmallFixedVector<char, 512>`, the channel's `Buffer`: begin, end and capacity,
/// then 512 bytes in place. Heap storage (`malloc`, freed with `free`) when larger. `begin`
/// points into `inline` while it is in place, so a buffer is never moved once reset.
#[repr(C)]
struct ChannelBuffer {
    begin: *mut u8,
    end: *mut u8,
    cap: usize,
    inline: [u8; INLINE],
}

const INLINE: usize = 512;

impl ChannelBuffer {
    const EMPTY: ChannelBuffer = ChannelBuffer { begin: std::ptr::null_mut(), end: std::ptr::null_mut(), cap: 0, inline: [0; INLINE] };

    /// Empty, in place (SmallFixedVector's default constructor).
    fn reset(&mut self) {
        self.begin = self.inline.as_mut_ptr();
        self.end = self.begin;
        self.cap = INLINE;
    }

    fn is_allocated(&self) -> bool {
        !std::ptr::eq(self.begin, self.inline.as_ptr())
    }

    /// Holds a copy of `data`: in place when it fits, else in a `malloc` block the channel takes
    /// over on a successful write.
    fn fill(&mut self, data: &[u8]) -> bool {
        self.reset();
        if data.len() > INLINE {
            // SAFETY: plain allocation; the channel frees it with `free` (SmallVector's dtor), or
            // `release` does if it stays ours.
            let p = unsafe { libc::malloc(data.len()) }.cast::<u8>();
            if p.is_null() {
                return false;
            }
            (self.begin, self.cap) = (p, data.len());
        }
        // SAFETY: `begin` has room for `data.len()` bytes (in place or just allocated), and the
        // two don't overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.begin, data.len());
            self.end = self.begin.add(data.len());
        }
        true
    }

    fn bytes(&self) -> &[u8] {
        let len = (self.end as usize).saturating_sub(self.begin as usize);
        if self.begin.is_null() || len > self.cap {
            return &[];
        }
        // SAFETY: [begin, end) is the vector's contents, inside its storage (checked above).
        unsafe { std::slice::from_raw_parts(self.begin, len) }
    }

    /// SmallVector's destructor: frees heap storage still held.
    fn release(&mut self) {
        if !self.begin.is_null() && self.is_allocated() {
            // SAFETY: heap storage of this buffer, from `malloc` (ours or the renderer's).
            unsafe { libc::free(self.begin.cast()) };
        }
        self.reset();
    }
}

type Ctor = unsafe extern "C" fn(*mut c_void, *mut c_void, u32);
type Dtor = unsafe extern "C" fn(*mut c_void);
type TryWrite = unsafe extern "C" fn(*mut c_void, *mut ChannelBuffer) -> i32;
type ReadBefore = unsafe extern "C" fn(*mut c_void, *mut ChannelBuffer, u64) -> i32;
type Wait = unsafe extern "C" fn(*mut c_void);
type Stop = unsafe extern "C" fn(*mut c_void);
type GetFb = unsafe extern "C" fn() -> *mut c_void;
type ProcessCall = unsafe extern "C" fn(*mut c_void, u64);

/// `RenderChannel::IoResult`.
const IO_OK: i32 = 0;
const IO_TRY_AGAIN: i32 = 1;
const IO_TIMEOUT: i32 = 3;

/// `INVALID_CONTEXT_ID`: a channel of no virtio-gpu context; the guest names its process with
/// `rcSetPuid` (from its `GLProcessPipe`).
const NO_CONTEXT: u32 = u32::MAX;

/// The bytes emu constructs a channel in. `createRenderChannel` must allocate no more than this
/// for its `make_shared` block (control block and channel), checked from its code.
const BLOCK: usize = 4096;
/// A constructed channel had another class's vtable: no more are made.
static WRONG_SHAPE: AtomicBool = AtomicBool::new(false);

/// libc++'s `__shared_ptr_emplace` header before the object: vtable, shared and weak counts.
const SHARED_HEADER: usize = 24;

#[derive(Clone, Copy)]
struct ChannelApi {
    ctor: Ctor,
    dtor: Dtor,
    try_write: TryWrite,
    wait_writable: Wait,
    read_before: ReadBefore,
    stop: Stop,
    /// Where a constructed channel's vtable pointer points: `vtable for RenderChannelImpl` + 16.
    vptr: usize,
    get_fb: GetFb,
    create_process: ProcessCall,
    cleanup_process: ProcessCall,
}

// SAFETY: plain function pointers into the loaded renderer, which is never unloaded.
unsafe impl Send for ChannelApi {}
// SAFETY: as above.
unsafe impl Sync for ChannelApi {}

macro_rules! need {
    ($h:expr, $name:literal) => {{
        // SAFETY: a NUL-terminated literal; the transmute target is the C++ member function's
        // Itanium ABI signature (`this` first, references as pointers) its mangled name fixes.
        unsafe {
            let p = libc::dlsym($h, concat!($name, "\0").as_ptr().cast());
            if p.is_null() {
                return Err(Error::Symbol($name));
            }
            std::mem::transmute::<*mut c_void, _>(p)
        }
    }};
}

impl ChannelApi {
    fn load(h: *mut c_void) -> Result<ChannelApi> {
        let create: *const u32 = need!(h, "_ZN9gfxstream4host12RendererImpl19createRenderChannelEPNS_6StreamEj");
        // SAFETY: an exported function's entry in the loaded image; its first 24 instructions
        // are mapped text (the function is longer than that).
        let code: [u32; 24] = unsafe { std::ptr::read_unaligned(create.cast::<[u32; 24]>()) };
        match macho::first_allocation_size(&code) {
            Some(n) if n > SHARED_HEADER && n - SHARED_HEADER <= BLOCK => {}
            _ => return Err(Error::Unavailable("RenderChannelImpl: unrecognized size".into())),
        }
        let vtable: *const u8 = need!(h, "_ZTVN9gfxstream4host17RenderChannelImplE");
        Ok(ChannelApi {
            ctor: need!(h, "_ZN9gfxstream4host17RenderChannelImplC1EPNS_6StreamEj"),
            dtor: need!(h, "_ZN9gfxstream4host17RenderChannelImplD1Ev"),
            try_write: need!(h, "_ZN9gfxstream4host17RenderChannelImpl8tryWriteEONS_16SmallFixedVectorIcLm512EEE"),
            wait_writable: need!(h, "_ZN9gfxstream4host17RenderChannelImpl17waitUntilWritableEv"),
            read_before: need!(h, "_ZN9gfxstream4host17RenderChannelImpl10readBeforeEPNS_16SmallFixedVectorIcLm512EEEy"),
            stop: need!(h, "_ZN9gfxstream4host17RenderChannelImpl4stopEv"),
            // Itanium vtables start with the offset to top and the type info.
            vptr: vtable as usize + 16,
            get_fb: need!(h, "_ZN9gfxstream4host11FrameBuffer5getFBEv"),
            create_process: need!(h, "_ZN9gfxstream4host11FrameBuffer30createGraphicsProcessResourcesEy"),
            cleanup_process: need!(h, "_ZN9gfxstream4host11FrameBuffer20cleanupProcGLObjectsEy"),
        })
    }
}

/// What [`RenderChannel::read`] got.
#[derive(Debug, PartialEq, Eq)]
pub enum ChannelRead {
    /// This many bytes were appended.
    Data(usize),
    /// Nothing within the timeout.
    Timeout,
    /// The channel is stopped and drained.
    Closed,
}

/// What [`RenderChannel::try_write`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum ChannelWrite {
    /// The render thread has the bytes.
    Done,
    /// Its queue is full; nothing was taken.
    Full,
    /// The channel is stopped (or the bytes couldn't be copied).
    Stopped,
}

/// One guest GLES connection and its render thread on the host. Calls may come from any
/// thread. Dropping it stops the connection; the render thread is joined on a thread of its
/// own, so a drop never waits for it.
pub struct RenderChannel {
    obj: *mut c_void,
    api: ChannelApi,
}

// SAFETY: `obj` is a RenderChannelImpl, whose methods take its own lock.
unsafe impl Send for RenderChannel {}
// SAFETY: as above.
unsafe impl Sync for RenderChannel {}

impl RenderChannel {
    /// Sends guest bytes to the render thread if its queue (1024 writes deep) has room. Never
    /// waits: a full queue is [`ChannelWrite::Full`], as stock's pipe answers "try again".
    pub fn try_write(&self, data: &[u8]) -> ChannelWrite {
        let mut b = ChannelBuffer::EMPTY;
        if !b.fill(data) {
            return ChannelWrite::Stopped;
        }
        // SAFETY: a live channel and a valid buffer; on success the channel moves it out
        // (taking its heap block), otherwise it's left as it was.
        let r = match unsafe { (self.api.try_write)(self.obj, &mut b) } {
            IO_OK => ChannelWrite::Done,
            IO_TRY_AGAIN => ChannelWrite::Full,
            _ => ChannelWrite::Stopped,
        };
        b.release();
        r
    }

    /// Waits until the queue has room or the channel stops.
    pub fn wait_writable(&self) {
        // SAFETY: a live channel; returns when there's room or the channel stops.
        unsafe { (self.api.wait_writable)(self.obj) }
    }

    /// Appends what the render thread sent the guest next to `out`, waiting up to `timeout`.
    pub fn read(&self, timeout: Duration, out: &mut Vec<u8>) -> ChannelRead {
        // `readBefore` takes a deadline in microseconds since the Unix epoch (gettimeofday's).
        let deadline = SystemTime::now().checked_add(timeout).and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        let deadline = deadline.map_or(u64::MAX, |d| d.as_micros() as u64);
        let mut b = ChannelBuffer::EMPTY;
        b.reset();
        // SAFETY: a live channel and an empty in-place buffer, which it move-assigns into.
        let r = unsafe { (self.api.read_before)(self.obj, &mut b, deadline) };
        let got = match r {
            IO_OK => {
                let bytes = b.bytes();
                out.extend_from_slice(bytes);
                ChannelRead::Data(bytes.len())
            }
            IO_TIMEOUT | IO_TRY_AGAIN => ChannelRead::Timeout,
            _ => ChannelRead::Closed,
        };
        b.release();
        got
    }

    /// Stops the connection: pending and later writes fail, a blocked reader returns, and the
    /// render thread exits.
    pub fn stop(&self) {
        // SAFETY: a live channel; stop closes its queues under its lock.
        unsafe { (self.api.stop)(self.obj) };
    }
}

struct Doomed(*mut c_void, Dtor);
// SAFETY: the channel is no longer reachable from anything but this value.
unsafe impl Send for Doomed {}

impl Drop for RenderChannel {
    fn drop(&mut self) {
        self.stop();
        // The destructor waits for the render thread to finish.
        let doomed = Doomed(self.obj, self.api.dtor);
        let _ = std::thread::Builder::new().name("gl-channel-free".into()).spawn(move || {
            let d = doomed;
            // SAFETY: the channel was constructed in this calloc block and nothing else holds
            // it; its destructor joins the render thread, then the block is freed.
            unsafe {
                (d.1)(d.0);
                libc::free(d.0);
            }
        });
    }
}

impl Renderer {
    fn channel_api(&self) -> Result<ChannelApi> {
        static API: OnceLock<std::result::Result<ChannelApi, String>> = OnceLock::new();
        if WRONG_SHAPE.load(Ordering::Relaxed) {
            return Err(Error::Unavailable("RenderChannelImpl: unexpected vtable".into()));
        }
        API.get_or_init(|| ChannelApi::load(self.handle).map_err(|e| e.to_string())).clone().map_err(Error::Unavailable)
    }

    /// A new GLES connection with its own render thread, as the stock emulator makes one per
    /// `opengles` pipe a guest opens. Needs an initialized renderer.
    pub fn open_channel(&self) -> Result<RenderChannel> {
        let api = self.channel_api()?;
        // SAFETY: zeroed storage, malloc-aligned (16), larger than the renderer's own allocation
        // for a channel (checked in `ChannelApi::load`).
        let obj = unsafe { libc::calloc(1, BLOCK) };
        if obj.is_null() {
            return Err(Error::Call("RenderChannelImpl: allocation", -12));
        }
        // SAFETY: constructs a RenderChannelImpl in `obj` with no snapshot to load and no
        // virtio-gpu context; it starts the connection's render thread.
        unsafe { (api.ctor)(obj, std::ptr::null_mut(), NO_CONTEXT) };
        // SAFETY: the object's first word is its vtable pointer.
        let vptr = unsafe { std::ptr::read(obj.cast::<usize>()) };
        if vptr != api.vptr {
            // Not the class this code was written for: leave it alone (and allocated) rather than
            // call into it, and make no more.
            WRONG_SHAPE.store(true, Ordering::Relaxed);
            return Err(Error::Unavailable("RenderChannelImpl: unexpected vtable".into()));
        }
        Ok(RenderChannel { obj, api })
    }

    /// Calls a `FrameBuffer` member taking a process id.
    fn process_call(&self, pick: fn(&ChannelApi) -> ProcessCall, puid: u64) -> Result<()> {
        let api = self.channel_api()?;
        // SAFETY: getFB returns the renderer's singleton or null; the member takes `this` first.
        unsafe {
            let fb = (api.get_fb)();
            if fb.is_null() {
                return Err(Error::Call("FrameBuffer::getFB", 0));
            }
            pick(&api)(fb, puid);
        }
        Ok(())
    }

    /// A guest process opened its `GLProcessPipe` and got `puid`: the per-process state its
    /// render threads look up after `rcSetPuid` (what the renderer's
    /// `onGuestGraphicsProcessCreate` does).
    pub fn graphics_process_created(&self, puid: u64) -> Result<()> {
        self.process_call(|a| a.create_process, puid)
    }

    /// The guest process `puid` is gone: frees the GL objects it left (contexts, surfaces,
    /// images). May take a while; the renderer runs it on a cleanup thread of its own.
    pub fn graphics_process_dead(&self, puid: u64) -> Result<()> {
        self.process_call(|a| a.cleanup_process, puid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_hold_small_writes_in_place_and_large_ones_on_the_heap() {
        let mut b = ChannelBuffer::EMPTY;
        assert!(b.fill(b"opengles"));
        assert!(!b.is_allocated());
        assert_eq!(b.bytes(), b"opengles");
        let big = vec![7u8; INLINE + 1];
        assert!(b.fill(&big));
        assert!(b.is_allocated());
        assert_eq!(b.bytes(), &big[..]);
        b.release();
        assert!(!b.is_allocated());
        assert!(b.bytes().is_empty());
    }

    #[test]
    fn layout_matches_small_fixed_vector() {
        // begin, end, capacity, then the 512 in-place bytes right after the capacity.
        assert_eq!(std::mem::offset_of!(ChannelBuffer, inline), 24);
        assert_eq!(std::mem::size_of::<ChannelBuffer>(), 24 + INLINE);
        assert_eq!(std::mem::align_of::<ChannelBuffer>(), 8);
    }
}
