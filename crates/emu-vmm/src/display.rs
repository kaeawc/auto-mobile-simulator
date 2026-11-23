//! The guest display as the host sees it: finished scanout frames from the GPU.
use std::sync::Mutex;

/// Receives finished scanout frames (BGRA/RGBA rows, `stride` bytes apart).
pub trait DisplaySink: Send + Sync {
    fn present(&self, width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]);
}

/// Keeps the latest frame (for screenshots and the desktop app) and counts frames.
#[derive(Default)]
pub struct FrameStore {
    inner: Mutex<FrameState>,
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
    }
}
