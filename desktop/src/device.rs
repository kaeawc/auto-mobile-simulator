//! The live device: a capture thread that streams frames into a [`Pixels`], and the HID session
//! that input goes to. Everything except the capture thread lives on the UI thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use std::time::Instant;

use rune::NativeSurface;
use rune::native_surface::macos::PixelBuffer;
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::geometry::{Point, Size};
use sim_core::gesture::Phase;
use sim_core::hid::{Button, KEY_LEFT_SHIFT, KeyStroke};
use sim_coresim::{Appearance, Context, Device, Direction, Frame, Hid, Surface};

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

/// A connected, booted device.
pub struct Live {
    pub udid: String,
    device: Device,
    hid: Option<Hid>,
    pub shared: Arc<Shared>,
    capture: Option<JoinHandle<()>>,
    pub dark: bool,
}

impl Live {
    /// Connects to a booted device and starts presenting its screen in `native`.
    pub fn connect(ctx: &Context, udid: &str, native: NativeSurface, waker: rune::Waker) -> Result<Self, String> {
        let device = ctx.device(udid).map_err(|e| e.to_string())?;
        let hid = device.hid().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared::default());
        let capture = spawn_capture(udid.to_owned(), native, shared.clone(), waker)?;
        Ok(Self { udid: udid.to_owned(), device, hid: Some(hid), shared, capture: Some(capture), dark: false })
    }

    fn screen(&self) -> Option<Size> {
        self.shared.size().map(|(w, h)| Size::new(f64::from(w), f64::from(h)))
    }

    /// Sends a touch at a point given in device pixels.
    pub fn touch(&self, x: f32, y: f32, phase: Phase) {
        let (Some(hid), Some(screen)) = (&self.hid, self.screen()) else { return };
        if let Err(e) = hid.touch(screen.normalize(Point::new(f64::from(x), f64::from(y))), phase) {
            eprintln!("touch: {e}");
        }
    }

    /// Presses or releases a hardware button. Callers schedule the release so the UI never sleeps.
    pub fn button(&self, button: Button, direction: Direction) {
        let Some(hid) = &self.hid else { return };
        if let Err(e) = hid.button(button, direction) {
            eprintln!("button: {e}");
        }
    }

    /// The device's data directory on the host.
    pub fn data_path(&self) -> String {
        self.device.data_path()
    }

    /// Copies the current frame (for saving screenshots).
    pub fn capture(&self) -> Result<Frame, String> {
        self.device.display().and_then(|d| d.capture()).map_err(|e| e.to_string())
    }

    pub fn strokes(&self, strokes: &[KeyStroke]) {
        let Some(hid) = &self.hid else { return };
        for s in strokes {
            let r = (|| {
                if s.shift {
                    hid.key(KEY_LEFT_SHIFT, Direction::Down)?;
                }
                hid.key(s.usage, Direction::Down)?;
                hid.key(s.usage, Direction::Up)?;
                if s.shift {
                    hid.key(KEY_LEFT_SHIFT, Direction::Up)?;
                }
                Ok::<(), sim_coresim::Error>(())
            })();
            if let Err(e) = r {
                eprintln!("key: {e}");
            }
        }
    }

    pub fn toggle_appearance(&mut self) {
        self.dark = !self.dark;
        let mode = if self.dark { Appearance::Dark } else { Appearance::Light };
        if let Err(e) = self.device.set_appearance(mode) {
            eprintln!("appearance: {e}");
        }
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.device.shutdown().map_err(|e| e.to_string())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.hid = None;
        if let Some(t) = self.capture.take() {
            t.join().ok();
        }
    }
}

/// Shows the device framebuffer without copying it: wraps its IOSurface once (again when the
/// device swaps surfaces, e.g. on rotation) and re-presents it whenever its seed changes. The
/// thread opens its own service connection: service objects stay on the thread that made them.
fn spawn_capture(udid: String, native: NativeSurface, shared: Arc<Shared>, waker: rune::Waker) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("sim-rs-capture".into())
        .spawn(move || {
            let display = match Context::new().and_then(|c| c.device(&udid)).and_then(|d| d.display()) {
                Ok(d) => d,
                Err(e) => return eprintln!("capture: {e}"),
            };
            let mut current: Option<(Surface, PixelBuffer)> = None;
            let mut last_seed = None;
            let mut frame_id = 0u64;
            let mut next_check = Instant::now();
            while !shared.stop.load(Ordering::Relaxed) {
                if current.is_none() || Instant::now() >= next_check {
                    next_check = Instant::now() + Duration::from_millis(250);
                    if let Some(surface) = display.surface_handle()
                        && current.as_ref().is_none_or(|(s, _)| s.as_raw() != surface.as_raw())
                    {
                        match crate::surface::wrap(&surface) {
                            Ok(buffer) => {
                                let (w, h) = surface.size();
                                let packed = (w << 16) | (h & 0xffff);
                                if shared.size.swap(packed, Ordering::Relaxed) != packed {
                                    waker();
                                }
                                current = Some((surface, buffer));
                                last_seed = None;
                            }
                            Err(e) => eprintln!("capture: {e}"),
                        }
                    }
                }
                if let Some((surface, buffer)) = &current {
                    let seed = surface.seed();
                    if Some(seed) != last_seed {
                        last_seed = Some(seed);
                        frame_id += 1;
                        if let Err(e) = native.push(buffer.clone(), frame_id) {
                            eprintln!("present: {e}");
                        }
                        shared.frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            native.clear();
        })
        .map_err(|e| e.to_string())
}

/// Lists devices for the picker. Errors become an empty list plus a message.
pub fn list(ctx: &Context) -> Result<Vec<DeviceInfo>, String> {
    let mut devices: Vec<DeviceInfo> = ctx.devices().map_err(|e| e.to_string())?.iter().map(Device::info).filter(|d| d.available).collect();
    devices.sort_by_key(|d| (d.state != DeviceState::Booted, d.runtime_id.clone(), d.name.clone()));
    Ok(devices)
}

/// Boots a device on a worker thread (booting blocks for seconds).
pub fn boot_async(udid: String, done: Arc<AtomicBool>, error: Arc<std::sync::Mutex<Option<String>>>, waker: rune::Waker) {
    std::thread::spawn(move || {
        let result = Context::new().and_then(|c| c.device(&udid)).and_then(|d| {
            if d.state() != DeviceState::Booted {
                d.boot()?;
            }
            for _ in 0..1200 {
                if d.boot_finished().unwrap_or(true) && d.display().is_ok_and(|x| x.surface().is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        });
        if let Err(e) = result
            && let Ok(mut slot) = error.lock()
        {
            *slot = Some(e.to_string());
        }
        done.store(true, Ordering::Relaxed);
        waker();
    });
}
