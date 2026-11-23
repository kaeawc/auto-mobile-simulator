//! The guest display as the host sees it: finished scanout frames from the GPU.
use std::sync::{Condvar, Mutex};
use std::time::Duration;

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
}

/// Keeps the latest frame (for screenshots and the desktop app) and counts frames.
#[derive(Default)]
pub struct FrameStore {
    inner: Mutex<FrameState>,
    /// Signalled on every new frame, so streams wake per frame rather than poll.
    posted: Condvar,
}

#[derive(Default, Clone)]
pub struct FrameState {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// virtio-gpu format of `pixels`.
    pub format: u32,
    pub pixels: Vec<u8>,
    pub frames: u64,
}

impl FrameStore {
    pub fn snapshot(&self) -> FrameState {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The latest frame's width, height and number, without copying its pixels.
    pub fn geometry(&self) -> (u32, u32, u64) {
        let s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (s.width, s.height, s.frames)
    }

    /// Blocks until a frame newer than number `seen` is presented or `timeout` passes; returns the
    /// latest frame number either way.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> u64 {
        let s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (s, _) = self.posted.wait_timeout_while(s, timeout, |s| s.frames <= seen).unwrap_or_else(|e| e.into_inner());
        s.frames
    }

    pub fn frames(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).frames
    }
}

impl DisplaySink for FrameStore {
    fn present(&self, width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]) {
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        s.width = width;
        s.height = height;
        s.stride = stride;
        s.format = format;
        s.pixels.clear();
        s.pixels.extend_from_slice(pixels);
        s.frames += 1;
        self.posted.notify_all();
    }

    /// Row by row, reversed, straight into the kept buffer: one copy, each row a `memcpy`.
    fn present_bottom_up(&self, width: u32, height: u32, format: u32, pixels: &[u8]) {
        let row = width as usize * 4;
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        s.width = width;
        s.height = height;
        s.stride = width * 4;
        s.format = format;
        s.pixels.clear();
        if row > 0 {
            for r in pixels.chunks_exact(row).rev() {
                s.pixels.extend_from_slice(r);
            }
        }
        s.frames += 1;
        self.posted.notify_all();
    }
}

#[cfg(test)]
mod tests {
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

    /// A frame posted bottom-up, as host composition posts them (2480x1740 BGRA).
    ///     cargo test --release -p emu-vmm present_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn present_cost() {
        let (w, h) = (2480u32, 1740u32);
        let px: Vec<u8> = (0..w * h * 4).map(|i| i as u8).collect();
        let store = FrameStore::default();
        let n = 30;
        let old = std::time::Instant::now();
        for _ in 0..n {
            let row = w as usize * 4;
            let flipped: Vec<u8> = px.chunks(row).rev().flatten().copied().collect();
            store.present(w, h, w * 4, 1, &flipped);
        }
        let old = old.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
        let new = std::time::Instant::now();
        for _ in 0..n {
            store.present_bottom_up(w, h, 1, &px);
        }
        let new = new.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
        println!("bottom-up frame into the store: {old:.2} ms (flip, then copy) -> {new:.2} ms");
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
        struct Plain(Mutex<Vec<u8>>);
        impl DisplaySink for Plain {
            fn present(&self, _: u32, _: u32, _: u32, _: u32, pixels: &[u8]) {
                *self.0.lock().expect("plain") = pixels.to_vec();
            }
        }
        let plain = Plain(Mutex::default());
        plain.present_bottom_up(1, 3, 1, &rows);
        assert_eq!(*plain.0.lock().expect("plain"), store.snapshot().pixels);
        assert_eq!(store.frames(), 3);
    }
}
