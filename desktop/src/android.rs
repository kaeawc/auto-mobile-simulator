//! A live Android device: frames from its display socket (`<avd>/emu-rs/display.sock`) stream into
//! a [`Pixels`], which wakes only the window showing it; touches and key taps go back as stream
//! lines. Frames come through the emulator's shared frame ring (only a 32-byte notice per frame
//! crosses the socket; the pixels are read from the ring straight into the buffer handed to
//! `Pixels`), or on the socket from an emulator without one. The emulator runs in its own process,
//! so a crashed guest can't take the window down.
//! What the socket doesn't carry (typing with shift, rotation, the hinge) goes over the console
//! through `sim-android`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rune::Pixels;
use sim_core::gesture::Phase;
use sim_core::stream::Input;

pub struct Screen {
    pub udid: String,
    pub avd_dir: PathBuf,
    pub pixels: Pixels,
    link: Arc<Mutex<sim_android::display::Display>>,
    device: Arc<(AtomicU32, AtomicU32)>,
    alive: Arc<AtomicBool>,
}

impl Screen {
    /// Connects and asks for frames at most `max_width` pixels wide.
    pub fn connect(udid: &str, avd_dir: PathBuf, max_width: u32) -> Result<Screen, String> {
        let mut display = sim_android::display::Display::connect(&avd_dir)?;
        display.request_shared()?;
        display.send(&Input::Hello { max_width })?;
        let mut frames = sim_android::display::Frames::new(display.try_clone()?);
        let (pixels, device, alive) = (Pixels::new(), Arc::new((AtomicU32::new(0), AtomicU32::new(0))), Arc::new(AtomicBool::new(true)));
        let (p, d, a) = (pixels.clone(), device.clone(), alive.clone());
        std::thread::Builder::new()
            .name("sim-rs-android-frames".into())
            .spawn(move || {
                while let Ok((header, rgba)) = frames.next_frame() {
                    d.0.store(header.device_width, Ordering::Relaxed);
                    d.1.store(header.device_height, Ordering::Relaxed);
                    p.set(header.width, header.height, rgba);
                }
                a.store(false, Ordering::Relaxed);
            })
            .map_err(|e| e.to_string())?;
        Ok(Screen { udid: udid.to_owned(), avd_dir, pixels, link: Arc::new(Mutex::new(display)), device, alive })
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The device's display size (input coordinates); `None` until the first frame.
    pub fn device_size(&self) -> Option<(u32, u32)> {
        let (w, h) = (self.device.0.load(Ordering::Relaxed), self.device.1.load(Ordering::Relaxed));
        (w > 0 && h > 0).then_some((w, h))
    }

    fn send(&self, input: &Input) {
        let mut link = self.link.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if link.send(input).is_err() {
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    /// A touch at device pixels.
    pub fn touch(&self, x: f32, y: f32, phase: Phase) {
        self.send(&Input::Touch { x, y, phase });
    }

    /// Presses and releases a key by console name or Linux keycode.
    pub fn key(&self, name: &str) {
        self.send(&Input::Key(name.to_owned()));
    }

    /// Turns the rotary input.
    pub fn wheel(&self, ticks: i32) {
        self.send(&Input::Wheel(ticks));
    }
}
