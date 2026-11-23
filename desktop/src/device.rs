//! The live device a window shows. iOS devices come from the simulator service, which exists only
//! on macOS; elsewhere the iOS side lists no devices and never connects, so the window, its
//! headless scenes and its tests still build and run.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[cfg(target_os = "macos")]
pub use crate::ios::{Context, Live, boot_async, list};
#[cfg(not(target_os = "macos"))]
pub use unavailable::{Context, Live, boot_async, list};

/// Shared between the UI and the capture thread.
#[derive(Default)]
pub struct Shared {
    pub stop: AtomicBool,
    /// Device framebuffer size in pixels, packed `w << 16 | h`; 0 until the first frame.
    pub size: AtomicU32,
    pub frames: AtomicU32,
}

impl Shared {
    pub fn size(&self) -> Option<(u32, u32)> {
        match self.size.load(Ordering::Relaxed) {
            0 => None,
            v => Some((v >> 16, v & 0xffff)),
        }
    }
}

/// The iOS side on hosts without the simulator service: no devices, and no way to get one.
#[cfg(not(target_os = "macos"))]
mod unavailable {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use rune::NativeSurface;
    use sim_core::device::DeviceInfo;
    use sim_core::frame::Frame;
    use sim_core::gesture::Phase;
    use sim_core::hid::{Button, Direction, KeyStroke};

    use super::Shared;

    const REASON: &str = "iOS simulator devices need a macOS host with Xcode";

    /// Never constructed: there is no simulator service to connect to.
    pub struct Context(Infallible);

    impl Context {
        pub fn new() -> Result<Self, String> {
            Err(REASON.into())
        }
    }

    /// Never constructed; its methods exist so the window compiles unchanged.
    pub struct Live {
        pub udid: String,
        pub shared: Arc<Shared>,
        pub dark: bool,
        never: Infallible,
    }

    impl Live {
        pub fn connect(ctx: &Context, _: &str, _: NativeSurface, _: rune::Waker) -> Result<Self, String> {
            match ctx.0 {}
        }

        pub fn touch(&self, _: f32, _: f32, _: Phase) {
            match self.never {}
        }

        pub fn button(&self, _: Button, _: Direction) {
            match self.never {}
        }

        pub fn crown(&self, _: i32) {
            match self.never {}
        }

        pub fn rotate(&mut self) {
            match self.never {}
        }

        pub fn data_path(&self) -> String {
            match self.never {}
        }

        pub fn capture(&self) -> Result<Frame, String> {
            match self.never {}
        }

        pub fn strokes(&self, _: &[KeyStroke]) {
            match self.never {}
        }

        pub fn toggle_appearance(&mut self) {
            match self.never {}
        }

        pub fn shutdown(&self) -> Result<(), String> {
            match self.never {}
        }
    }

    pub fn list(ctx: &Context) -> Result<Vec<DeviceInfo>, String> {
        match ctx.0 {}
    }

    /// Reports at once that nothing can boot here.
    pub fn boot_async(_: String, done: Arc<AtomicBool>, error: Arc<Mutex<Option<String>>>, waker: rune::Waker) {
        if let Ok(mut slot) = error.lock() {
            *slot = Some(REASON.into());
        }
        done.store(true, Ordering::Relaxed);
        waker();
    }
}
