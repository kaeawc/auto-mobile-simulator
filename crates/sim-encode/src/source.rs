//! Frame sources for the encoder, each paced by the device's own frame announcements and each
//! stamping a frame's capture instant as it is announced.
//!
//! - [`IosSource`]: the framebuffer `IOSurface`'s frame callback; the picture is the surface
//!   itself, wrapped as a `CVPixelBuffer` (no copy; VideoToolbox converts BGRA on the GPU).
//! - [`AndroidSource`]: emu's display socket and frame ring ([`FrameReader`]: a reader thread
//!   keeps the newest frame), and the picture is that frame copied once into an IOSurface-backed
//!   pool buffer. Under `emu -gpu metal` a frame comes as the IOSurface emu's GPU copied it into,
//!   and the picture wraps it as the iOS source does (no copy); it goes back to emu when the next
//!   frame replaces it. A frame that comes as pixels still takes the copy.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use sim_core::stream::Input;
use sim_coresim::videotoolbox::{PixelBuffer, PixelBufferPool, PixelFormat};
use sim_coresim::{Display, FrameWatch};

use crate::android_frames::{FrameReader, HeldSurface};
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
    /// A surface has been seen; one that is gone after that means the device went away.
    seen_surface: bool,
}

/// Whether the device went away: it had a surface and now has none.
fn device_gone(seen_surface: bool, has_surface: bool) -> bool {
    seen_surface && !has_surface
}

impl IosSource {
    /// Follows `display`'s frame callback. The surface already on screen counts as the first frame.
    pub fn new(display: Display) -> Result<IosSource> {
        let signal = Arc::new(Signal::default());
        let s = signal.clone();
        let watch = display.watch_frames(move || s.post(Instant::now())).map_err(|e| Error::Source(e.to_string()))?;
        let display_has_surface = display.surface().is_some();
        if display_has_surface {
            signal.post(Instant::now());
        }
        Ok(IosSource { display, signal, _watch: watch, surface: None, seen_surface: display_has_surface })
    }
}

impl Source for IosSource {
    type Picture = PixelBuffer;

    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        let frame = self.signal.wait(deadline);
        if frame.is_none() && device_gone(self.seen_surface, self.display.surface().is_some()) {
            return Err(Error::Source("the display has no surface any more (the device went away)".into()));
        }
        Ok(frame)
    }

    fn picture(&mut self) -> Result<Option<&PixelBuffer>> {
        let Some(surface) = self.display.surface_handle() else {
            return if device_gone(self.seen_surface, false) {
                Err(Error::Source("the display has no surface any more (the device went away)".into()))
            } else {
                Ok(None)
            };
        };
        self.seen_surface = true;
        let addr = surface.as_raw() as usize;
        if self.surface.as_ref().is_none_or(|(a, _, _)| *a != addr) {
            let buffer = PixelBuffer::wrap_surface(&surface).map_err(|e| Error::Source(e.to_string()))?;
            self.surface = Some((addr, surface, buffer));
        }
        Ok(self.surface.as_ref().map(|(_, _, b)| b))
    }
}

/// A running AVD's display (`<avd>/simee/display.sock`), frames through the shared ring.
pub struct AndroidSource {
    reader: FrameReader,
    pool: Option<PixelBufferPool>,
    current: Option<PixelBuffer>,
    /// The emulator's surface `current` wraps, if it does: released when `current` is replaced
    /// (the encoder is done with a picture by the next `picture` call).
    held: Option<HeldSurface>,
    /// Frames that came as surfaces, and as pixels.
    pub surfaces: u64,
    pub copied: u64,
    /// What the last `picture` call that took a frame saw, until the stream loop reads it.
    taken_at: Option<Instant>,
    unreported: u64,
}

impl AndroidSource {
    /// Connects to the AVD in `avd_dir` and asks for frames at most `max_width` pixels wide (0:
    /// full size; emu shrinks by a whole factor, `ceil(width / max_width)`).
    pub fn connect(avd_dir: &Path, max_width: u32) -> Result<AndroidSource> {
        Ok(AndroidSource {
            reader: FrameReader::connect(avd_dir, max_width)?,
            pool: None,
            current: None,
            held: None,
            surfaces: 0,
            copied: 0,
            taken_at: None,
            unreported: 0,
        })
    }

    /// The device's own display size (what input coordinates are in), once a frame has arrived.
    pub fn device_size(&self) -> Option<(u32, u32)> {
        self.reader.device_size()
    }

    /// A second write handle on the connection, for sending input lines from another thread.
    pub fn input_link(&self) -> Result<UnixStream> {
        self.reader.input_link()
    }

    /// Sends one input line (a touch, a key) on the display connection.
    pub fn input(&mut self, input: &Input) -> Result<()> {
        self.reader.input(input)
    }
}

impl Source for AndroidSource {
    type Picture = PixelBuffer;

    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
        self.reader.wait(deadline)
    }

    fn picture(&mut self) -> Result<Option<&PixelBuffer>> {
        if let Some(frame) = self.reader.take() {
            self.unreported = frame.unreported;
            self.taken_at = Some(frame.at);
            let (w, h) = (frame.width, frame.height);
            if let Some(held) = frame.surface {
                // The emulator's surface itself, wrapped: no copy (VideoToolbox converts BGRA).
                let wrapped = PixelBuffer::wrap_surface(&held.surface).map_err(|e| Error::Source(e.to_string()))?;
                self.current = Some(wrapped);
                // The last picture's surface goes back only after the new one replaced it.
                self.held = Some(held);
                self.surfaces += 1;
                return Ok(self.current.as_ref());
            }
            if self.pool.as_ref().is_none_or(|p| p.size() != (w, h)) {
                self.pool = Some(PixelBufferPool::new(w, h, PixelFormat::Rgba).map_err(|e| Error::Source(e.to_string()))?);
            }
            let pool = self.pool.as_ref().ok_or_else(|| Error::Source("no pool".into()))?;
            let filled = pool.fill(&frame.rgba, w as usize * 4);
            self.reader.recycle(frame.rgba);
            self.current = Some(filled.map_err(|e| Error::Source(e.to_string()))?);
            self.held = None;
            self.copied += 1;
        }
        Ok(self.current.as_ref())
    }

    fn take_captured(&mut self) -> Option<Instant> {
        self.taken_at.take()
    }

    fn take_unreported(&mut self) -> u64 {
        std::mem::take(&mut self.unreported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_that_was_there_and_is_gone_means_the_device_went_away() {
        assert!(device_gone(true, false));
        assert!(!device_gone(true, true));
        assert!(!device_gone(false, false), "not rendered yet is not gone");
        assert!(!device_gone(false, true));
    }

    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    use sim_core::stream::{FrameHeader, Record};
    use sim_coresim::{Surface, SurfaceOutbox};

    /// An emulator under `-gpu metal`, faked: once the client has said `hello`, it sends `frames`
    /// new IOSurfaces of `size` to the port the client named (`surfaces <name>`), each followed by
    /// its record, `gap` apart; the `release` lines it gets come out of the channel. Without a
    /// `surfaces` line it sends nothing. Removes its directory on drop.
    struct FakeMetalEmu {
        dir: std::path::PathBuf,
        releases: std::sync::mpsc::Receiver<u32>,
        ids: Arc<Mutex<Vec<u32>>>,
    }

    impl Drop for FakeMetalEmu {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fake_metal_emu(name: &str, size: (u32, u32), frames: u32, gap: Duration) -> FakeMetalEmu {
        // Short (SUN_LEN), as the frame reader's own fake does.
        let dir = std::path::PathBuf::from("/tmp").join(format!("simee-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = emu_avd::state_dir(&dir);
        std::fs::create_dir_all(&state).expect("state dir");
        let listener = UnixListener::bind(state.join("display.sock")).expect("bind");
        let (tx, releases) = std::sync::mpsc::channel();
        let ids = Arc::new(Mutex::new(Vec::new()));
        let made = ids.clone();
        std::thread::spawn(move || {
            let Ok((conn, _)) = listener.accept() else { return };
            let mut out = conn.try_clone().expect("clone");
            let mut lines = BufReader::new(conn).lines();
            let mut port = None;
            for line in lines.by_ref() {
                let Ok(line) = line else { return };
                if let Some(Input::Surfaces(n)) = Input::parse(&line) {
                    port = SurfaceOutbox::look_up(&n).ok();
                }
                if line.starts_with("hello") {
                    break;
                }
            }
            let Some(port) = port else { return };
            let mut held = Vec::new();
            for seq in 0..frames {
                let s = Surface::new_bgra(size.0, size.1).expect("surface");
                made.lock().expect("ids").push(s.id());
                port.send(&s, seq).expect("send");
                let header = FrameHeader::new(size.0, size.1, size.0, size.1, u64::from(seq)).expect("header");
                if out.write_all(&Record::Surface { header, seq }.encode()).is_err() {
                    return;
                }
                held.push(s);
                std::thread::sleep(gap);
            }
            for line in lines {
                let Ok(line) = line else { return };
                if let Some(Input::Release(seq)) = Input::parse(&line) {
                    let _ = tx.send(seq);
                }
            }
        });
        FakeMetalEmu { dir, releases, ids }
    }

    fn picture_after(source: &mut AndroidSource, frames: u64) -> (u32, u32, [u8; 4]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while source.surfaces + source.copied < frames {
            assert!(Instant::now() < deadline, "no frame");
            if source.wait(deadline).expect("wait").is_some() {
                let _ = source.picture().expect("picture");
            }
        }
        let p = source.current.as_ref().expect("a picture");
        let (w, h) = p.size();
        (w, h, p.fourcc())
    }

    #[test]
    fn an_emulators_surfaces_are_wrapped_without_a_copy_and_given_back_when_replaced() {
        let emu = fake_metal_emu("vt-surf", (64, 32), 2, Duration::from_millis(100));
        let mut source = AndroidSource::connect(&emu.dir, 0).expect("connect");
        assert_eq!(picture_after(&mut source, 1), (64, 32, *b"BGRA"));
        assert_eq!((source.surfaces, source.copied), (1, 0));
        let first = emu.ids.lock().expect("ids")[0];
        assert_eq!(source.held.as_ref().map(|h| h.surface.id()), Some(first), "the emulator's surface, not a copy");
        assert!(source.pool.is_none(), "no pool: nothing to copy into");
        assert!(emu.releases.recv_timeout(Duration::from_millis(50)).is_err(), "held while it's the picture");
        // The next frame replaces it, and the first goes back.
        picture_after(&mut source, 2);
        assert_eq!(emu.releases.recv_timeout(Duration::from_secs(5)), Ok(0));
        assert_eq!(source.device_size(), Some((64, 32)));
    }

    #[test]
    fn frames_that_come_as_pixels_still_take_the_copy() {
        let display = crate::android_frames::tests::fake_display("vt-px", (8, 4), 1, Duration::ZERO);
        let mut source = AndroidSource::connect(&display.dir, 0).expect("connect");
        display.go();
        assert_eq!(picture_after(&mut source, 1), (8, 4, *b"BGRA"));
        assert_eq!((source.surfaces, source.copied), (0, 1));
        assert!(source.held.is_none());
    }
}
