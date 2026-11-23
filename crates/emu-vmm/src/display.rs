//! The guest display as the host sees it: finished scanout frames from the GPU.
//!
//! Frames reach the host two ways. Host composition posts them to the renderer, which keeps them:
//! the post only records the frame's size ([`DisplaySink::posted`]), and the pixels are read back
//! on demand through the renderer ([`DisplaySink::set_readback`]), once per posted frame however
//! many streams and screenshots want it ([`FrameStore::latest`]). Guest-composed scanouts
//! (virtio-gpu `RESOURCE_FLUSH`) arrive as pixels ([`DisplaySink::present`]), read straight into
//! the store ([`DisplaySink::present_with`]).
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use sim_core::pixels::{PixelFormat, to_rgba_into};

/// Reads the last posted frame back as tightly packed RGBA, top row first, `width` x `height`,
/// into the buffer (exactly `width * height * 4` bytes). False when there's nothing to read.
pub type Readback = Box<dyn Fn(u32, u32, &mut [u8]) -> bool + Send + Sync>;

/// Receives finished scanout frames (BGRA/RGBA rows, `stride` bytes apart).
pub trait DisplaySink: Send + Sync {
    fn present(&self, width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]);

    /// [`DisplaySink::present`] for a frame whose rows arrive bottom row first (GL readback),
    /// tightly packed.
    fn present_bottom_up(&self, width: u32, height: u32, format: u32, pixels: &[u8]) {
        let row = width as usize * 4;
        let flipped: Vec<u8> = pixels.chunks_exact(row.max(1)).rev().flatten().copied().collect();
        self.present(width, height, width * 4, format, &flipped);
    }

    /// [`DisplaySink::present`] for a frame `fill` writes into the buffer it's given
    /// (`stride * height` bytes): a readback lands in the sink's own buffer with no second copy.
    /// Nothing is presented when `fill` returns false.
    fn present_with(&self, width: u32, height: u32, stride: u32, format: u32, fill: &mut dyn FnMut(&mut [u8]) -> bool) {
        let mut buf = vec![0; stride as usize * height as usize];
        if fill(&mut buf) {
            self.present(width, height, stride, format, &buf);
        }
    }

    /// A `width` x `height` frame was posted whose pixels the [`Readback`] produces on demand:
    /// only its size and number are recorded, nothing is copied.
    fn posted(&self, width: u32, height: u32) {
        let _ = (width, height);
    }

    /// How posted frames are read back (see [`DisplaySink::posted`]).
    fn set_readback(&self, readback: Readback) {
        let _ = readback;
    }
}

/// Keeps the latest frame (for screenshots and the desktop app) and counts frames.
#[derive(Default)]
pub struct FrameStore {
    inner: Mutex<FrameState>,
    /// Signalled on every new frame, so streams wake per frame rather than poll.
    posted: Condvar,
    readback: OnceLock<Readback>,
    /// The last frame read back or converted, shared by every reader.
    capture: Mutex<Capture>,
}

#[derive(Default, Clone)]
pub struct FrameState {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// virtio-gpu format of `pixels`.
    pub format: u32,
    /// The presented frame's pixels; empty for a posted frame, whose pixels the renderer keeps.
    pub pixels: Vec<u8>,
    pub frames: u64,
}

/// The latest frame as tightly packed RGBA, top row first.
#[derive(Default)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    /// The frame number it was captured for.
    pub frame: u64,
    pub rgba: Vec<u8>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FrameStore {
    pub fn snapshot(&self) -> FrameState {
        lock(&self.inner).clone()
    }

    /// The latest frame's width, height and number, without copying its pixels.
    pub fn geometry(&self) -> (u32, u32, u64) {
        let s = lock(&self.inner);
        (s.width, s.height, s.frames)
    }

    /// Blocks until a frame newer than number `seen` is presented or `timeout` passes; returns the
    /// latest frame number either way.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> u64 {
        let s = lock(&self.inner);
        let (s, _) = self.posted.wait_timeout_while(s, timeout, |s| s.frames <= seen).unwrap_or_else(|e| e.into_inner());
        s.frames
    }

    pub fn frames(&self) -> u64 {
        lock(&self.inner).frames
    }

    /// Runs `f` on the latest frame as RGBA. The frame is read back (or converted from presented
    /// pixels) only when it is newer than the last capture, so streams and screenshots of the
    /// same frame share one readback. Readers of the same frame wait for each other while `f`
    /// runs; the guest's posts never wait. `None` before the first frame, or when the frame can't
    /// be read.
    pub fn latest<R>(&self, f: impl FnOnce(&Capture) -> R) -> Option<R> {
        let (width, height, frame) = self.geometry();
        if frame == 0 || width == 0 || height == 0 {
            return None;
        }
        let mut cap = lock(&self.capture);
        if (cap.frame, cap.width, cap.height) != (frame, width, height) {
            let len = width as usize * height as usize * 4;
            let read = self.readback.get().is_some_and(|r| {
                cap.rgba.resize(len, 0);
                r(width, height, &mut cap.rgba)
            }) || self.presented_rgba(width, height, &mut cap.rgba);
            if !read {
                cap.frame = 0;
                return None;
            }
            (cap.width, cap.height, cap.frame) = (width, height, frame);
        }
        Some(f(&cap))
    }

    /// Frame `frame` (`width` x `height`, the latest [`FrameStore::geometry`] gave) as RGBA into
    /// `dst` (`width * height * 4` bytes), for a consumer that wants the whole frame and nothing
    /// else, like a full-size stream writing into shared memory: copied from the capture if it
    /// holds that frame, else read back from the renderer straight into `dst`, skipping the
    /// capture (a later [`FrameStore::latest`] reads the frame back again). False when the frame
    /// can't be read.
    pub fn read_into(&self, width: u32, height: u32, frame: u64, dst: &mut [u8]) -> bool {
        if dst.len() != width as usize * height as usize * 4 || frame == 0 {
            return false;
        }
        {
            let cap = lock(&self.capture);
            if (cap.frame, cap.width, cap.height) == (frame, width, height) {
                dst.copy_from_slice(&cap.rgba);
                return true;
            }
        }
        if self.readback.get().is_some_and(|r| r(width, height, dst)) {
            return true;
        }
        let copied = self.latest(|c| {
            let same = (c.width, c.height) == (width, height);
            if same {
                dst.copy_from_slice(&c.rgba);
            }
            same
        });
        copied == Some(true)
    }

    /// The presented pixels of a `width` x `height` frame, as RGBA.
    fn presented_rgba(&self, width: u32, height: u32, out: &mut Vec<u8>) -> bool {
        let s = lock(&self.inner);
        if (s.width, s.height) != (width, height) || s.pixels.is_empty() {
            return false;
        }
        let format = PixelFormat::from_virtio_gpu(s.format).unwrap_or(PixelFormat::Rgba);
        to_rgba_into(&s.pixels, width as usize, height as usize, s.stride as usize, format, out)
    }

    fn update(&self, width: u32, height: u32, stride: u32, format: u32, fill: impl FnOnce(&mut Vec<u8>) -> bool) {
        let mut s = lock(&self.inner);
        if !fill(&mut s.pixels) {
            return;
        }
        (s.width, s.height, s.stride, s.format) = (width, height, stride, format);
        s.frames += 1;
        self.posted.notify_all();
    }
}

impl DisplaySink for FrameStore {
    fn present(&self, width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]) {
        self.update(width, height, stride, format, |kept| {
            kept.clear();
            kept.extend_from_slice(pixels);
            true
        });
    }

    /// Row by row, reversed, straight into the kept buffer: one copy, each row a `memcpy`.
    fn present_bottom_up(&self, width: u32, height: u32, format: u32, pixels: &[u8]) {
        let row = width as usize * 4;
        self.update(width, height, width * 4, format, |kept| {
            kept.clear();
            if row > 0 {
                for r in pixels.chunks_exact(row).rev() {
                    kept.extend_from_slice(r);
                }
            }
            true
        });
    }

    /// Straight into the kept buffer, reused in place. The fill (a renderer readback) runs with
    /// the store unlocked: the renderer may take its FrameBuffer lock, which a render thread holds
    /// while it posts here. Meanwhile the store has no pixels, and a failed fill leaves none rather
    /// than a partly written frame (the frame count stays).
    fn present_with(&self, width: u32, height: u32, stride: u32, format: u32, fill: &mut dyn FnMut(&mut [u8]) -> bool) {
        let mut buf = std::mem::take(&mut lock(&self.inner).pixels);
        buf.resize(stride as usize * height as usize, 0);
        let filled = fill(&mut buf);
        self.update(width, height, stride, format, |kept| {
            *kept = if filled { buf } else { Vec::new() };
            filled
        });
    }

    fn posted(&self, width: u32, height: u32) {
        self.update(width, height, width * 4, PixelFormat::RGBA_VIRTIO, |kept| {
            kept.clear();
            true
        });
    }

    fn set_readback(&self, readback: Readback) {
        let _ = self.readback.set(readback);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn waiters_wake_on_the_next_frame() {
        let store = std::sync::Arc::new(FrameStore::default());
        assert_eq!(store.wait_newer(0, Duration::from_millis(10)), 0, "times out with nothing new");
        let poster = store.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            poster.present(1, 1, 4, 1, &[0; 4]);
        });
        let start = std::time::Instant::now();
        assert_eq!(store.wait_newer(0, Duration::from_secs(5)), 1);
        assert!(start.elapsed() < Duration::from_secs(2), "woken by the frame, not the timeout");
        t.join().expect("poster");
        assert_eq!(store.wait_newer(0, Duration::ZERO), 1, "already newer: no wait");
    }

    /// A frame posted bottom-up, as host composition posts them (BGRA), `n` times at each size:
    /// flipped and copied (the first version), copied row by row (the last), and posted (size
    /// only, now). One line per size.
    fn present_bench(sizes: &[(u32, u32)], n: u32) -> Vec<String> {
        let mut lines = Vec::new();
        for &(w, h) in sizes {
            let px: Vec<u8> = (0..w * h * 4).map(|i| i as u8).collect();
            let store = FrameStore::default();
            let time = |f: &mut dyn FnMut()| {
                let t = std::time::Instant::now();
                for _ in 0..n {
                    f();
                }
                t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            };
            let row = w as usize * 4;
            let old = time(&mut || {
                let flipped: Vec<u8> = px.chunks(row).rev().flatten().copied().collect();
                store.present(w, h, w * 4, 1, &flipped);
            });
            let copied = time(&mut || store.present_bottom_up(w, h, 1, &px));
            let posted = time(&mut || store.posted(w, h));
            assert_eq!(store.frames(), 3 * u64::from(n));
            lines.push(format!(
                "{w}x{h} frame into the store: {old:.2} ms (flip, then copy) -> {copied:.2} ms (copy) -> {posted:.4} ms (posted: size only)"
            ));
        }
        lines
    }

    #[test]
    fn the_benchmark_runs() {
        assert_eq!(present_bench(&[(4, 3)], 2).len(), 1);
    }

    /// Posts at a tablet's and a phone's size.
    ///     cargo test --release -p emu-vmm present_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn present_cost() {
        for l in present_bench(&[(2480, 1740), (1080, 2400)], 30) {
            println!("{l}");
        }
    }

    #[test]
    fn keeps_the_latest_frame_and_counts_them() {
        let store = FrameStore::default();
        assert_eq!((store.frames(), store.snapshot().pixels.len()), (0, 0));
        store.present(2, 1, 8, 1, &[1; 8]);
        store.present(1, 1, 4, 2, &[7, 7, 7, 7]);
        let f = store.snapshot();
        assert_eq!((f.width, f.height, f.stride, f.format, f.frames), (1, 1, 4, 2, 2));
        assert_eq!(f.pixels, [7, 7, 7, 7], "replaced, not appended");
        assert_eq!(store.geometry(), (1, 1, 2));
        // Bottom-up rows come out top-down, whether the store or the default flips them.
        let rows = [1u8, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
        store.present_bottom_up(1, 3, 1, &rows);
        assert_eq!(store.snapshot().pixels, [3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1]);
        #[derive(Default)]
        struct Plain(Mutex<Vec<u8>>);
        impl DisplaySink for Plain {
            fn present(&self, _: u32, _: u32, _: u32, _: u32, pixels: &[u8]) {
                *self.0.lock().expect("plain") = pixels.to_vec();
            }
        }
        let plain = Plain::default();
        plain.present_bottom_up(1, 3, 1, &rows);
        assert_eq!(*plain.0.lock().expect("plain"), store.snapshot().pixels);
        assert_eq!(store.frames(), 3);
        // The trait's defaults: present_with fills a temporary; posted and set_readback are no-ops.
        plain.present_with(1, 1, 4, 1, &mut |b| {
            b.copy_from_slice(&[4, 5, 6, 7]);
            true
        });
        plain.present_with(1, 1, 4, 1, &mut |_| false);
        assert_eq!(*plain.0.lock().expect("plain"), [4, 5, 6, 7], "a failed fill presents nothing");
        plain.posted(1, 1);
        plain.set_readback(Box::new(|_, _, _| true));
    }

    #[test]
    fn presents_straight_into_the_store_and_never_serves_a_failed_fill() {
        let store = FrameStore::default();
        store.present_with(1, 2, 8, 2, &mut |b| {
            assert_eq!(b.len(), 16, "stride x height");
            b.copy_from_slice(&[1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]);
            true
        });
        let f = store.snapshot();
        assert_eq!((f.width, f.height, f.stride, f.format, f.frames), (1, 2, 8, 2, 1));
        assert_eq!(f.pixels, [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]);
        // Captures convert presented pixels (BGRX here) to tightly packed RGBA.
        assert_eq!(store.latest(|c| (c.width, c.height, c.frame, c.rgba.clone())), Some((1, 2, 1, vec![3, 2, 1, 255, 7, 6, 5, 255])));
        store.present_with(1, 2, 8, 2, &mut |b| {
            b.fill(9);
            false
        });
        assert_eq!((store.frames(), store.snapshot().pixels.len()), (1, 0), "no new frame, and no half-written pixels");
        assert_eq!(store.latest(|c| c.rgba[0]), Some(3), "the frame captured before stays");
        // The fill runs with the store unlocked: a renderer readback may wait on a render thread
        // that is posting here (this would deadlock on the store's own lock otherwise).
        store.present_with(1, 1, 4, 67, &mut |b| {
            store.posted(1, 1);
            b.copy_from_slice(&[5, 6, 7, 8]);
            store.geometry().2 == 2
        });
        assert_eq!((store.frames(), store.snapshot().pixels), (3, vec![5, 6, 7, 8]));
    }

    #[test]
    fn posted_frames_are_read_back_once_however_many_readers() {
        let store = FrameStore::default();
        assert_eq!(store.latest(|_| ()), None, "nothing posted yet");
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        store.set_readback(Box::new(move |w, h, buf| {
            let n = counter.fetch_add(1, Ordering::Relaxed) as u8;
            assert_eq!(buf.len(), w as usize * h as usize * 4);
            buf.fill(n + 1);
            true
        }));
        store.posted(2, 1);
        assert_eq!(store.snapshot().pixels.len(), 0, "a post copies no pixels");
        assert_eq!(store.geometry(), (2, 1, 1));
        let first = store.latest(|c| (c.frame, c.rgba.clone()));
        let again = store.latest(|c| (c.frame, c.rgba.clone()));
        assert_eq!(first, Some((1, vec![1; 8])));
        assert_eq!(again, first, "the same frame: no second readback");
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        store.posted(2, 1);
        assert_eq!(store.latest(|c| c.rgba[0]), Some(2), "a new frame is read back");
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        // A whole-frame consumer copies a captured frame, and reads a new one straight in.
        let mut dst = [0u8; 8];
        assert!(store.read_into(2, 1, 2, &mut dst));
        assert_eq!((dst, reads.load(Ordering::Relaxed)), ([2; 8], 2), "copied from the capture");
        store.posted(2, 1);
        assert!(store.read_into(2, 1, 3, &mut dst));
        assert_eq!((dst, reads.load(Ordering::Relaxed)), ([3; 8], 3), "read back into dst");
        assert_eq!(store.latest(|c| c.rgba[0]), Some(4), "the capture wasn't updated: read again");
        assert!(!store.read_into(2, 1, 3, &mut [0; 7]), "wrong size");
        assert!(!store.read_into(2, 1, 0, &mut dst), "no frame");
    }

    #[test]
    fn a_failed_readback_falls_back_to_presented_pixels_or_nothing() {
        let store = FrameStore::default();
        store.set_readback(Box::new(|_, _, _| false));
        store.posted(1, 1);
        assert_eq!(store.latest(|_| ()), None, "no readback, no pixels");
        assert!(!store.read_into(1, 1, 1, &mut [0; 4]));
        // Guest composition presents pixels; the renderer has nothing posted to read.
        store.present(1, 1, 4, 67, &[9, 8, 7, 6]);
        assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![9, 8, 7, 6]));
        let mut dst = [0; 4];
        store.present(1, 1, 4, 67, &[1, 2, 3, 4]);
        assert!(store.read_into(1, 1, 3, &mut dst), "converted presented pixels");
        assert_eq!(dst, [1, 2, 3, 4]);
    }
}
