//! Frame sources for the encoder, each paced by the device's own frame announcements and each
//! stamping a frame's capture instant as it is announced.
//!
//! - [`IosSource`]: the framebuffer `IOSurface`'s frame callback; the picture is the surface
//!   itself, wrapped as a `CVPixelBuffer` (no copy; VideoToolbox converts BGRA on the GPU).
//! - [`AndroidSource`]: emu's display socket and frame ring; a reader thread keeps the newest
//!   frame, and the picture is that frame copied once into an IOSurface-backed pool buffer.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use sim_android::display::{Display as AndroidDisplay, Frames};
use sim_core::stream::Input;
use sim_coresim::videotoolbox::{PixelBuffer, PixelBufferPool, PixelFormat};
use sim_coresim::{Display, FrameWatch};

use crate::stream::Source;
use crate::{Error, Result};

/// The newest capture instant, set from another thread.
#[derive(Default)]
struct Signal {
    at: Mutex<Option<Instant>>,
    changed: Condvar,
}

impl Signal {
    fn lock(&self) -> MutexGuard<'_, Option<Instant>> {
        self.at.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn post(&self, at: Instant) {
        *self.lock() = Some(at);
        self.changed.notify_one();
    }

    fn wait(&self, deadline: Instant) -> Option<Instant> {
        let mut at = self.lock();
        loop {
            if let Some(t) = at.take() {
                return Some(t);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            at = self.changed.wait_timeout(at, deadline - now).map_or_else(|p| p.into_inner().0, |(g, _)| g);
        }
    }
}

/// An iOS device's main display. Keep it on the thread that made the device's `Context`.
pub struct IosSource {
    display: Display,
    signal: Arc<Signal>,
    _watch: FrameWatch,
    /// The surface the current pixel buffer wraps, by address (a rotation brings a new one).
    surface: Option<(usize, sim_coresim::Surface, PixelBuffer)>,
}

impl IosSource {
    /// Follows `display`'s frame callback. The surface already on screen counts as the first frame.
    pub fn new(display: Display) -> Result<IosSource> {
        let signal = Arc::new(Signal::default());
        let s = signal.clone();
        let watch = display.watch_frames(move || s.post(Instant::now())).map_err(|e| Error::Source(e.to_string()))?;
        if display.surface().is_some() {
            signal.post(Instant::now());
        }
        Ok(IosSource { display, signal, _watch: watch, surface: None })
    }
}

impl Source for IosSource {
    type Picture = PixelBuffer;

    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        Ok(self.signal.wait(deadline))
    }

    fn picture(&mut self) -> Result<Option<&PixelBuffer>> {
        let Some(surface) = self.display.surface_handle() else { return Ok(None) };
        let addr = surface.as_raw() as usize;
        if self.surface.as_ref().is_none_or(|(a, _, _)| *a != addr) {
            let buffer = PixelBuffer::wrap_surface(&surface).map_err(|e| Error::Source(e.to_string()))?;
            self.surface = Some((addr, surface, buffer));
        }
        Ok(self.surface.as_ref().map(|(_, _, b)| b))
    }
}

/// The newest frame the reader thread has, not yet taken.
#[derive(Default)]
struct Mailbox {
    frame: Option<Frame>,
    /// A buffer to read the next frame into (the last one taken).
    spare: Vec<u8>,
    closed: Option<String>,
    device: (u32, u32),
}

struct Frame {
    seq: u64,
    at: Instant,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Mailbox> {
        self.mailbox.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A running AVD's display (`<avd>/simee/display.sock`), frames through the shared ring.
pub struct AndroidSource {
    shared: Arc<Shared>,
    /// The connection's write side: `hello`, and input from [`AndroidSource::input`].
    link: UnixStream,
    pool: Option<PixelBufferPool>,
    current: Option<PixelBuffer>,
    /// The last frame `wait` reported, by sequence number.
    reported: Option<u64>,
    reader: Option<JoinHandle<()>>,
}

impl AndroidSource {
    /// Connects to the AVD in `avd_dir` and asks for frames at most `max_width` pixels wide (0:
    /// full size; emu shrinks by a whole factor, `ceil(width / max_width)`).
    pub fn connect(avd_dir: &Path, max_width: u32) -> Result<AndroidSource> {
        let source = |e: String| Error::Source(e);
        let mut display = AndroidDisplay::connect(avd_dir).map_err(source)?;
        display.request_shared().map_err(source)?;
        // `hello 0` pauses the stream (a hidden window's); full size is a width no frame exceeds.
        let max_width = if max_width == 0 { u32::MAX } else { max_width };
        display.send(&Input::Hello { max_width }).map_err(source)?;
        let mut frames = Frames::new(display.try_clone().map_err(source)?).with_fallback(display.try_clone().map_err(source)?);
        let link = display.try_clone().map_err(source)?;
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let reader = std::thread::Builder::new()
            .name("simee-encode-android".into())
            .spawn(move || {
                let mut buf = Vec::new();
                for seq in 0.. {
                    match frames.newest_frame_into(&mut buf, || true) {
                        Ok(h) => {
                            let at = Instant::now();
                            let mut m = s.lock();
                            m.device = (h.device_width, h.device_height);
                            let frame = Frame { seq, at, width: h.width, height: h.height, rgba: std::mem::take(&mut buf) };
                            let replaced = m.frame.replace(frame);
                            // Read the next frame into the one nobody took, or the spare.
                            buf = replaced.map_or_else(|| std::mem::take(&mut m.spare), |f| f.rgba);
                            drop(m);
                            s.changed.notify_one();
                        }
                        Err(e) => {
                            s.lock().closed = Some(e.to_string());
                            s.changed.notify_one();
                            return;
                        }
                    }
                }
            })
            .map_err(|e| Error::Source(e.to_string()))?;
        Ok(AndroidSource { shared, link, pool: None, current: None, reported: None, reader: Some(reader) })
    }

    /// The device's own display size (what input coordinates are in), once a frame has arrived.
    pub fn device_size(&self) -> Option<(u32, u32)> {
        Some(self.shared.lock().device).filter(|&(w, h)| w > 0 && h > 0)
    }

    /// A second write handle on the connection, for sending input lines from another thread.
    pub fn input_link(&self) -> Result<UnixStream> {
        self.link.try_clone().map_err(|e| Error::Source(e.to_string()))
    }

    /// Sends one input line (a touch, a key) on the display connection.
    pub fn input(&mut self, input: &Input) -> Result<()> {
        use std::io::Write;
        let line = format!("{}\n", input.to_line());
        self.link.write_all(line.as_bytes()).map_err(|e| Error::Source(e.to_string()))
    }
}

impl Source for AndroidSource {
    type Picture = PixelBuffer;

    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        let shared = self.shared.clone();
        let mut m = shared.lock();
        loop {
            // Each frame is reported once; it stays in the mailbox until `picture` takes it or a
            // newer one replaces it.
            if let Some(f) = &m.frame
                && self.reported != Some(f.seq)
            {
                self.reported = Some(f.seq);
                return Ok(Some(f.at));
            }
            if let Some(e) = &m.closed {
                return Err(Error::Source(format!("the display connection closed: {e}")));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            m = self.shared.changed.wait_timeout(m, deadline - now).map_or_else(|p| p.into_inner().0, |(g, _)| g);
        }
    }

    fn picture(&mut self) -> Result<Option<&PixelBuffer>> {
        let taken = self.shared.lock().frame.take();
        if let Some(frame) = taken {
            let (w, h) = (frame.width, frame.height);
            if self.pool.as_ref().is_none_or(|p| p.size() != (w, h)) {
                self.pool = Some(PixelBufferPool::new(w, h, PixelFormat::Rgba).map_err(|e| Error::Source(e.to_string()))?);
            }
            let pool = self.pool.as_ref().ok_or_else(|| Error::Source("no pool".into()))?;
            let filled = pool.fill(&frame.rgba, w as usize * 4);
            self.shared.lock().spare = frame.rgba;
            self.current = Some(filled.map_err(|e| Error::Source(e.to_string()))?);
        }
        Ok(self.current.as_ref())
    }
}

impl Drop for AndroidSource {
    fn drop(&mut self) {
        // Closing both directions wakes the reader out of its blocking read.
        let _ = self.link.shutdown(std::net::Shutdown::Both);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}
