//! `device:<udid or name>[:x,y,w,h]`: another simee device's live screen as a camera, optionally
//! cropped to a region of it, so one device can scan a QR code another one shows.
//!
//! A backend's reader thread posts the source device's frames to a [`Screen`] as the device
//! presents them, and a [`DeviceSource`] turns the newest one into camera frames:
//!
//! - **Android** (an AVD name): emu's display socket and shared frame ring
//!   (`sim_android::display`), at full size: one `pread` from the ring per device frame.
//! - **iOS** (a simulator's UDID or name, macOS only): the framebuffer `IOSurface`'s frame
//!   callback, then one copy out of the locked surface (`sim_coresim::Display::capture`) when its
//!   seed says the contents changed.
//!
//! Each subscriber size is then drawn in one pass from that frame: the region cropped, the result
//! scaled to fill the size (aspect kept, overflow cropped from both sides, nearest neighbour) and
//! swizzled to BGRA. Pacing follows the device: a capture tick takes a frame the device presented
//! since the last one, waiting up to one tick for it, and repeats the last frame (shared, not
//! redrawn) while the screen is still. The source device going away (shut down, its display
//! closed) is [`Error::Missing`], which the hub reports as `SourceMissing`.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::id::SourceSpec;
use crate::source::Source;

/// A rectangle of a device's screen, in its own pixels (what `simee screenshot` gives).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

/// A parsed `device:` source: the device as written, and the region to crop to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceSpec {
    /// An AVD name, or a simulator's UDID or name.
    pub device: String,
    pub region: Option<Region>,
}

const EXAMPLE: &str = "device:Pixel_9 or device:<udid>:0,0,540,960";

impl DeviceSpec {
    /// Parses what follows `device:`. A last `:x,y,w,h` (four numbers) is the region; anything
    /// else after a `:` belongs to the name.
    pub fn parse(rest: &str) -> Result<DeviceSpec> {
        let rest = rest.trim();
        let (device, region) = match rest.rsplit_once(':') {
            Some((device, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == ',' || c == ' ') => {
                (device.trim(), Some(parse_region(tail)?))
            }
            _ => (rest, None),
        };
        if device.is_empty() {
            return Err(Error::Invalid(format!("device: needs a device's UDID or name, as in {EXAMPLE}")));
        }
        Ok(DeviceSpec { device: device.to_string(), region })
    }

    /// Whether this names the device that goes by one of `names` (its UDID or AVD name, its
    /// name), ignoring case.
    pub fn names(&self, names: &[&str]) -> bool {
        names.iter().map(|n| n.trim()).any(|n| !n.is_empty() && n.eq_ignore_ascii_case(&self.device))
    }
}

impl fmt::Display for DeviceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.region {
            Some(r) => write!(f, "{}:{r}", self.device),
            None => f.write_str(&self.device),
        }
    }
}

fn parse_region(text: &str) -> Result<Region> {
    let bad = || Error::Invalid(format!("device: region {text:?} is not x,y,width,height in the device's pixels, as in {EXAMPLE}"));
    let n: Vec<u32> = text.split(',').map(|p| p.trim().parse::<u32>()).collect::<std::result::Result<_, _>>().map_err(|_| bad())?;
    let [x, y, width, height] = n[..] else { return Err(bad()) };
    if width == 0 || height == 0 {
        return Err(Error::Invalid(format!("device: region {text:?} is empty; give it a width and a height")));
    }
    Ok(Region { x, y, width, height })
}

/// Refuses linking a device's camera to its own screen: the feed would film itself, each frame
/// the last one again, smaller. `target` holds the names the target device goes by (its UDID or
/// AVD name, and its name). Anything that isn't a `device:` source passes.
pub fn refuse_feedback(source: &SourceSpec, target: &[&str]) -> Result<()> {
    match source {
        SourceSpec::Device(d) if d.names(target) => Err(Error::Invalid(format!(
            "device:{}: a device's camera can't show its own screen (a feedback loop); link another device's screen",
            d.device
        ))),
        _ => Ok(()),
    }
}

/// One frame of a device's screen as the device presented it: RGBA rows, top row first.
#[derive(Clone, Debug)]
pub struct Shot {
    /// Counts up from 0 with every frame posted to the [`Screen`].
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// The device's own screen size, which regions are given in (the frame may be smaller).
    pub screen: (u32, u32),
    pub rgba: Arc<Vec<u8>>,
}

/// The newest frame of a device's screen: what a backend's reader thread (or a test) posts, and a
/// [`DeviceSource`] reads.
#[derive(Default)]
pub struct Screen {
    latest: Mutex<Latest>,
    arrived: Condvar,
}

#[derive(Default)]
struct Latest {
    shot: Option<Shot>,
    next_seq: u64,
    gone: Option<String>,
}

impl Screen {
    fn lock(&self) -> MutexGuard<'_, Latest> {
        self.latest.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Posts a `width`x`height` RGBA frame of a `screen`-sized display. Returns a buffer to read
    /// the next frame into: the replaced frame's, when no source holds it any more.
    pub fn post(&self, width: u32, height: u32, screen: (u32, u32), rgba: Vec<u8>) -> Vec<u8> {
        let mut latest = self.lock();
        let seq = latest.next_seq;
        latest.next_seq += 1;
        let old = latest.shot.replace(Shot { seq, width, height, screen, rgba: Arc::new(rgba) });
        drop(latest);
        self.arrived.notify_all();
        let mut spare = old.and_then(|s| Arc::try_unwrap(s.rgba).ok()).unwrap_or_default();
        spare.clear();
        spare
    }

    /// The device went away: every read from now on is [`Error::Missing`] with `why`.
    pub fn close(&self, why: impl Into<String>) {
        self.lock().gone = Some(why.into());
        self.arrived.notify_all();
    }

    /// The newest frame after frame `after` (any frame, with `None`), waiting up to `timeout` for
    /// one: `Ok(None)` when none came.
    fn newer(&self, after: Option<u64>, timeout: Duration) -> Result<Option<Shot>> {
        let deadline = Instant::now() + timeout;
        let mut latest = self.lock();
        loop {
            if let Some(why) = &latest.gone {
                return Err(Error::Missing(why.clone()));
            }
            if let Some(shot) = latest.shot.as_ref().filter(|s| after.is_none_or(|a| s.seq > a)) {
                return Ok(Some(shot.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            latest = self.arrived.wait_timeout(latest, deadline - now).map_or_else(|p| p.into_inner().0, |(g, _)| g);
        }
    }
}

/// How long a capture waits for the source device's first frame.
const FIRST_FRAME: Duration = Duration::from_secs(5);

/// Sizes drawn from one device frame at once; more distinct sizes than this start over.
const SIZES: usize = 8;

/// What keeps a backend's reader running; dropping it stops the reader.
type Reader = Box<dyn Send>;

/// A camera frame drawn from the current device frame, by its size.
type Drawn = ((u32, u32), Arc<[u8]>);

/// A device's screen as a camera source.
pub struct DeviceSource {
    name: String,
    region: Option<Region>,
    screen: Arc<Screen>,
    first_frame: Duration,
    /// The device frame the camera frames are drawn from now.
    shot: Option<Shot>,
    /// The capture tick that last waited for a device frame: the other sizes of that tick are
    /// drawn from the same one.
    waited: Option<u64>,
    drawn: Vec<Drawn>,
    _reader: Option<Reader>,
}

impl DeviceSource {
    /// A source over `screen`, which something else posts `name`'s frames to (a test's fake).
    pub fn new(name: &str, region: Option<Region>, screen: Arc<Screen>) -> DeviceSource {
        DeviceSource {
            name: name.to_string(),
            region,
            screen,
            first_frame: FIRST_FRAME,
            shot: None,
            waited: None,
            drawn: Vec::new(),
            _reader: None,
        }
    }

    fn reading(mut self, reader: Reader) -> DeviceSource {
        self._reader = Some(reader);
        self
    }
}

impl Source for DeviceSource {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        if self.waited != Some(tick.seq) {
            self.waited = Some(tick.seq);
            let timeout = if self.shot.is_some() { spec.interval() } else { self.first_frame };
            if let Some(shot) = self.screen.newer(self.shot.as_ref().map(|s| s.seq), timeout)? {
                self.shot = Some(shot);
                self.drawn.clear();
            }
        }
        let Some(shot) = &self.shot else {
            return Err(Error::Capture(format!("no frames from device {} in {} s", self.name, self.first_frame.as_secs())));
        };
        let size = (spec.width, spec.height);
        let bgra = match self.drawn.iter().find(|(s, _)| *s == size) {
            Some((_, bgra)) => bgra.clone(),
            None => {
                let window = window(self.region, (shot.width, shot.height), shot.screen)
                    .map_err(|e| Error::Invalid(format!("device:{}: {e}", self.name)))?;
                let bgra: Arc<[u8]> = crop_fill(&shot.rgba, shot.width, shot.height, window, spec.width, spec.height)?.into();
                if self.drawn.len() >= SIZES {
                    self.drawn.clear();
                }
                self.drawn.push((size, bgra.clone()));
                bgra
            }
        };
        Ok(CameraFrame { width: spec.width, height: spec.height, bgra, tick })
    }

    /// Each subscriber size is drawn from the device frame (one pass: crop, scale, swizzle)
    /// rather than fitted from another size's.
    fn synthetic(&self) -> bool {
        true
    }
}

/// The part of a `frame`-sized picture that `region` covers (the whole picture without one):
/// `region` is in the device's `screen` pixels, clamped to the screen, then mapped to the frame's
/// (a frame smaller than the screen was downscaled by the device).
pub fn window(region: Option<Region>, frame: (u32, u32), screen: (u32, u32)) -> std::result::Result<Region, String> {
    let (fw, fh) = frame;
    let (sw, sh) = if screen.0 == 0 || screen.1 == 0 { frame } else { screen };
    let Some(r) = region else { return Ok(Region { x: 0, y: 0, width: fw, height: fh }) };
    if r.x >= sw || r.y >= sh {
        return Err(format!("region {r} is outside the {sw}x{sh} screen"));
    }
    let to = |v: u32, from: u32, into: u32| (u64::from(v) * u64::from(into) / u64::from(from)) as u32;
    let (x1, y1) = (r.x.saturating_add(r.width).min(sw), r.y.saturating_add(r.height).min(sh));
    let (x, y) = (to(r.x, sw, fw).min(fw.saturating_sub(1)), to(r.y, sh, fh).min(fh.saturating_sub(1)));
    let width = to(x1, sw, fw).saturating_sub(x).max(1);
    let height = to(y1, sh, fh).saturating_sub(y).max(1);
    Ok(Region { x, y, width, height })
}

/// Draws `window` of an RGBA picture into a `dst_w`x`dst_h` BGRA frame the way a camera fills
/// its sensor: aspect kept, the whole frame covered, the overflow cropped equally from both sides
/// (nearest neighbour), every pixel opaque.
pub fn crop_fill(src: &[u8], src_w: u32, src_h: u32, window: Region, dst_w: u32, dst_h: u32) -> Result<Vec<u8>> {
    let (sw, sh) = (src_w as usize, src_h as usize);
    if src.len() < sw * sh * 4
        || window.width == 0
        || window.height == 0
        || window.x.saturating_add(window.width) > src_w
        || window.y.saturating_add(window.height) > src_h
    {
        return Err(Error::Capture(format!("a {src_w}x{src_h} device frame of {} bytes can't be cropped to {window}", src.len())));
    }
    let (ww, wh, dw, dh) = (u64::from(window.width), u64::from(window.height), u64::from(dst_w), u64::from(dst_h));
    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];
    if dw == 0 || dh == 0 {
        return Ok(out);
    }
    let (vw, vh) = if ww * dh > dw * wh { ((wh * dw / dh).max(1), wh) } else { (ww, (ww * dh / dw).max(1)) };
    let (x0, y0) = (u64::from(window.x) + (ww - vw) / 2, u64::from(window.y) + (wh - vh) / 2);
    let columns: Vec<usize> = (0..dw).map(|x| ((x0 + x * vw / dw) * 4) as usize).collect();
    for (y, row) in out.chunks_exact_mut(dst_w as usize * 4).enumerate() {
        let sy = (y0 + y as u64 * vh / dh) as usize;
        let src_row = &src[sy * sw * 4..][..sw * 4];
        for (px, &sx) in row.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            *px = [src_row[sx + 2], src_row[sx + 1], src_row[sx], 255];
        }
    }
    Ok(out)
}

/// Opens `device:` `spec`: an AVD of that name, else (macOS) a simulator by UDID or name.
pub fn open(spec: &DeviceSpec) -> Result<Box<dyn Source>> {
    let screen = Arc::new(Screen::default());
    let reader = start(&spec.device, screen.clone())?;
    Ok(Box::new(DeviceSource::new(&spec.device, spec.region, screen).reading(reader)))
}

#[cfg(all(unix, feature = "device-source"))]
fn start(name: &str, screen: Arc<Screen>) -> Result<Reader> {
    let env = sim_android::Env::from_env();
    if env.has_avd(name) {
        let avd = env.load(name).map_err(|e| Error::Io(format!("device:{name}: {e}")))?;
        return Ok(Box::new(android::Reader::start(&avd.dir, name, screen)?));
    }
    ios(name, screen)
}

#[cfg(all(not(unix), feature = "device-source"))]
fn start(name: &str, _screen: Arc<Screen>) -> Result<Reader> {
    Err(Error::Unsupported(format!("device:{name}: a device's screen as a camera source needs macOS or Linux")))
}

/// Without the `device-source` feature (emu's build): the id parses, and opening it says who serves it.
#[cfg(not(feature = "device-source"))]
fn start(name: &str, _screen: Arc<Screen>) -> Result<Reader> {
    Err(Error::Unsupported(format!("device:{name}: device sources are served by simee, not emu")))
}

#[cfg(all(target_os = "macos", feature = "device-source"))]
fn ios(name: &str, screen: Arc<Screen>) -> Result<Reader> {
    Ok(Box::new(ios::Reader::start(name, screen)?))
}

#[cfg(all(unix, not(target_os = "macos"), feature = "device-source"))]
fn ios(name: &str, _screen: Arc<Screen>) -> Result<Reader> {
    Err(Error::Unsupported(format!(
        "device:{name}: no AVD is named {name:?}, and an iOS simulator's screen as a camera source needs macOS"
    )))
}

/// Android: emu's display socket, frames through its shared ring.
#[cfg(all(unix, feature = "device-source"))]
mod android {
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use sim_android::display::{Display, Frames};
    use sim_core::stream::Input;

    use super::Screen;
    use crate::error::{Error, Result};

    /// The reader thread of one display connection.
    pub(super) struct Reader {
        link: UnixStream,
        thread: Option<JoinHandle<()>>,
    }

    impl Reader {
        /// Connects to the AVD in `avd_dir` and posts its frames, at full size, to `screen`.
        pub(super) fn start(avd_dir: &Path, name: &str, screen: Arc<Screen>) -> Result<Reader> {
            let io = |e: String| Error::Io(format!("device:{name}: {e}"));
            let mut display = Display::connect(avd_dir).map_err(|e| Error::Missing(format!("{name} isn't running under emu: {e}")))?;
            display.request_shared().map_err(io)?;
            // `hello 0` would pause the stream; full size is a width no frame exceeds.
            display.send(&Input::Hello { max_width: u32::MAX }).map_err(io)?;
            let mut frames = Frames::new(display.try_clone().map_err(io)?).with_fallback(display.try_clone().map_err(io)?);
            let link = display.try_clone().map_err(io)?;
            let name = name.to_string();
            let thread = std::thread::Builder::new()
                .name("camera device android".into())
                .spawn(move || {
                    let mut buf = Vec::new();
                    loop {
                        match frames.newest_frame_into(&mut buf, || true) {
                            Ok(h) => buf = screen.post(h.width, h.height, (h.device_width, h.device_height), std::mem::take(&mut buf)),
                            Err(e) => return screen.close(format!("{name}'s display closed (the device shut down?): {e}")),
                        }
                    }
                })
                .map_err(|e| Error::Capture(format!("can't start the device reader: {e}")))?;
            Ok(Reader { link, thread: Some(thread) })
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            // Closing both directions wakes the reader out of its blocking read.
            let _ = self.link.shutdown(std::net::Shutdown::Both);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

/// iOS: the framebuffer surface's frame callback. The service's objects aren't `Send`, so one
/// thread makes the context, finds the device and reads its display for the capture's life.
#[cfg(all(target_os = "macos", feature = "device-source"))]
mod ios {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use sim_core::device::DeviceState;
    use sim_coresim::{Context, Device, Display, FrameWatch};

    use super::Screen;
    use crate::error::{Error, Result};

    /// How often an idle reader checks that the device is still booted (and whether to stop).
    const IDLE: Duration = Duration::from_millis(250);

    /// "The device presented a frame", from the service's callback thread.
    #[derive(Default)]
    struct Presented {
        flag: Mutex<bool>,
        cv: Condvar,
    }

    impl Presented {
        fn post(&self) {
            *self.flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.cv.notify_all();
        }

        /// Waits up to `timeout`: whether a frame was presented.
        fn wait(&self, timeout: Duration) -> bool {
            let flag = self.flag.lock().unwrap_or_else(|p| p.into_inner());
            let (mut flag, _) = self.cv.wait_timeout_while(flag, timeout, |f| !*f).unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *flag)
        }
    }

    pub(super) struct Reader {
        stop: Arc<AtomicBool>,
        presented: Arc<Presented>,
        thread: Option<JoinHandle<()>>,
    }

    impl Reader {
        /// Finds simulator `name` (a UDID or a name), checks it is booted, and posts its main
        /// display's frames to `screen`; fails here when it can't.
        pub(super) fn start(name: &str, screen: Arc<Screen>) -> Result<Reader> {
            let (stop, presented) = (Arc::new(AtomicBool::new(false)), Arc::new(Presented::default()));
            let (ready, opened) = mpsc::channel();
            let (n, s, p) = (name.to_string(), stop.clone(), presented.clone());
            let thread = std::thread::Builder::new()
                .name("camera device ios".into())
                .spawn(move || run(&n, &screen, &s, &p, &ready))
                .map_err(|e| Error::Capture(format!("can't start the device reader: {e}")))?;
            match opened.recv() {
                Ok(Ok(())) => Ok(Reader { stop, presented, thread: Some(thread) }),
                Ok(Err(e)) => {
                    let _ = thread.join();
                    Err(e)
                }
                Err(_) => {
                    let _ = thread.join();
                    Err(Error::Capture(format!("device:{name}: the device reader stopped before it started")))
                }
            }
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.presented.post();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn open(name: &str, presented: &Arc<Presented>) -> Result<(Context, Device, Display, FrameWatch)> {
        let context = Context::new().map_err(|e| Error::Unavailable(format!("device:{name}: the simulator service: {e}")))?;
        let device =
            context.device(name).map_err(|e| Error::Invalid(format!("device:{name}: no AVD or simulator is named {name:?} ({e})")))?;
        if device.state() != DeviceState::Booted {
            return Err(Error::Missing(format!("{name} isn't booted; boot it first (`simee boot {name}`)")));
        }
        let display = device.display().map_err(|e| Error::Missing(format!("{name} has no display yet: {e}")))?;
        let p = presented.clone();
        let watch =
            display.watch_frames(move || p.post()).map_err(|e| Error::Unavailable(format!("device:{name}: frame callback: {e}")))?;
        Ok((context, device, display, watch))
    }

    fn run(name: &str, screen: &Screen, stop: &AtomicBool, presented: &Arc<Presented>, ready: &mpsc::Sender<Result<()>>) {
        let (_context, device, display, _watch) = match sim_coresim::autoreleasepool(|| open(name, presented)) {
            Ok(opened) => opened,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        // The surface already on screen counts as the first frame.
        let mut frame = true;
        let mut seed = None;
        while !stop.load(Ordering::Acquire) {
            if frame {
                let now = display.seed();
                if seed != Some(now)
                    && let Ok(f) = sim_coresim::autoreleasepool(|| display.capture())
                {
                    seed = Some(now);
                    screen.post(f.width, f.height, (f.width, f.height), f.rgba);
                }
            } else if sim_coresim::autoreleasepool(|| device.state()) != DeviceState::Booted {
                return screen.close(format!("{name} shut down"));
            }
            frame = presented.wait(IDLE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotplug::{Event, HotPlug, Signals};
    use crate::hub::Hub;
    use crate::source::Opener;
    use crate::{CameraFrame, HostCamera};
    use std::thread;

    fn spec(id: &str) -> DeviceSpec {
        match SourceSpec::parse(id) {
            Ok(SourceSpec::Device(d)) => d,
            other => panic!("{id}: {other:?}"),
        }
    }

    #[test]
    fn device_ids_parse_with_and_without_a_region_and_round_trip() {
        let r = |x, y, width, height| Some(Region { x, y, width, height });
        let cases = [
            ("device:Pixel_9", "Pixel_9", None),
            ("device:Pixel_9:0,100,540,960", "Pixel_9", r(0, 100, 540, 960)),
            ("device:5A1B2C3D-0000-4000-8000-000000000001:10,20,30,40", "5A1B2C3D-0000-4000-8000-000000000001", r(10, 20, 30, 40)),
            ("device:iPhone 16 Pro", "iPhone 16 Pro", None),
            ("device:Jo's phone: work", "Jo's phone: work", None),
        ];
        for (id, device, region) in cases {
            let d = spec(id);
            assert_eq!((d.device.as_str(), d.region), (device, region), "{id}");
            assert_eq!(SourceSpec::Device(d.clone()).to_string(), id);
            assert_eq!(SourceSpec::parse(&SourceSpec::Device(d).to_string()).map(|s| s.kind()), Ok("device"));
        }
        assert_eq!(spec("DEVICE: Pixel : 1, 2, 3, 4 ").region, r(1, 2, 3, 4));
    }

    #[test]
    fn bad_device_ids_explain_themselves() {
        for (id, want) in [
            ("device:", "needs a device's UDID or name"),
            ("device::0,0,10,10", "needs a device's UDID or name"),
            ("device:Pixel:0,0,10", "not x,y,width,height"),
            ("device:Pixel:0,0,0,10", "is empty"),
            ("device:Pixel:1,2,3,99999999999", "not x,y,width,height"),
        ] {
            let err = SourceSpec::parse(id).expect_err(id).to_string();
            assert!(err.contains(want), "{id}: {err}");
        }
    }

    #[test]
    fn a_device_is_refused_as_its_own_camera() {
        let target = ["5A1B2C3D-0000-4000-8000-000000000001", "iPhone 16"];
        for id in ["device:5a1b2c3d-0000-4000-8000-000000000001", "device:iPhone 16:0,0,10,10", "device:IPHONE 16"] {
            let err = refuse_feedback(&SourceSpec::parse(id).unwrap_or_else(|e| panic!("{e}")), &target).expect_err(id);
            assert!(err.to_string().contains("feedback loop"), "{err}");
        }
        assert_eq!(refuse_feedback(&SourceSpec::parse("device:Pixel_9").unwrap_or_else(|e| panic!("{e}")), &target), Ok(()));
        assert_eq!(refuse_feedback(&SourceSpec::Qr("iPhone 16".into()), &target), Ok(()));
        assert_eq!(refuse_feedback(&SourceSpec::Device(spec("device:x")), &["", " "]), Ok(()), "a blank name names nothing");
    }

    #[test]
    fn a_region_is_clamped_to_the_screen_and_mapped_to_the_frame() {
        let reg = |x, y, width, height| Region { x, y, width, height };
        assert_eq!(window(None, (1080, 1920), (1080, 1920)), Ok(reg(0, 0, 1080, 1920)));
        assert_eq!(window(Some(reg(100, 200, 300, 400)), (1080, 1920), (1080, 1920)), Ok(reg(100, 200, 300, 400)));
        // Past the right and bottom edges: what is on the screen.
        assert_eq!(window(Some(reg(900, 1800, 500, 500)), (1080, 1920), (1080, 1920)), Ok(reg(900, 1800, 180, 120)));
        // A frame the device sent at half size: the region in screen pixels lands on half.
        assert_eq!(window(Some(reg(540, 960, 540, 960)), (540, 960), (1080, 1920)), Ok(reg(270, 480, 270, 480)));
        // No screen size: the frame's own.
        assert_eq!(window(Some(reg(1, 1, 2, 2)), (4, 4), (0, 0)), Ok(reg(1, 1, 2, 2)));
        let err = window(Some(reg(1080, 0, 10, 10)), (1080, 1920), (1080, 1920)).expect_err("outside");
        assert!(err.contains("outside the 1080x1920 screen"), "{err}");
    }

    /// A 4x4 RGBA picture whose pixel (x, y) is (x, y, 7, 9): each output pixel names its source.
    fn grid(w: u32, h: u32) -> Vec<u8> {
        (0..h).flat_map(|y| (0..w).flat_map(move |x| [x as u8, y as u8, 7, 9])).collect()
    }

    /// The source (x, y) of each output pixel, from its BGRA (7, y, x, 255).
    fn sources(bgra: &[u8]) -> Vec<(u8, u8)> {
        bgra.as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                assert_eq!((p[0], p[3]), (7, 255), "swizzled to BGRA, opaque");
                (p[2], p[1])
            })
            .collect()
    }

    #[test]
    fn crop_fill_crops_scales_and_swizzles_in_one_pass() {
        let src = grid(4, 4);
        let whole = Region { x: 0, y: 0, width: 4, height: 4 };
        // Same size: every pixel, in place.
        let same = sources(&crop_fill(&src, 4, 4, whole, 4, 4).unwrap_or_default());
        assert_eq!(same[5], (1, 1));
        assert_eq!(same[15], (3, 3));
        // Down by two: every other pixel.
        assert_eq!(sources(&crop_fill(&src, 4, 4, whole, 2, 2).unwrap_or_default()), [(0, 0), (2, 0), (0, 2), (2, 2)]);
        // A region, scaled up: its pixels repeated.
        let region = Region { x: 2, y: 1, width: 2, height: 2 };
        let up = sources(&crop_fill(&src, 4, 4, region, 4, 4).unwrap_or_default());
        assert_eq!(&up[..4], &[(2, 1), (2, 1), (3, 1), (3, 1)]);
        assert_eq!(up[15], (3, 2));
        // A wide output from a square: the middle rows, cropped equally top and bottom.
        assert_eq!(sources(&crop_fill(&src, 4, 4, whole, 4, 2).unwrap_or_default())[..4], [(0, 1), (1, 1), (2, 1), (3, 1)]);
        // A tall output from a square region: its middle columns.
        assert_eq!(sources(&crop_fill(&src, 4, 4, whole, 2, 4).unwrap_or_default())[..2], [(1, 0), (2, 0)]);
        // A window off the frame, or a short frame, is an error, not garbage.
        assert!(crop_fill(&src, 4, 4, Region { x: 3, y: 0, width: 2, height: 1 }, 2, 2).is_err());
        assert!(crop_fill(&src[..20], 4, 4, whole, 2, 2).is_err());
    }

    fn tick(seq: u64) -> Tick {
        Tick { seq, ..Tick::start() }
    }

    fn fps(width: u32, height: u32, fps: u32) -> FrameSpec {
        FrameSpec::new(width, height, fps).unwrap_or_default()
    }

    fn solid(screen: &Screen, value: u8) {
        screen.post(64, 64, (64, 64), vec![value; 64 * 64 * 4]);
    }

    fn take(source: &mut DeviceSource, spec: FrameSpec, seq: u64) -> CameraFrame {
        source.frame(spec, tick(seq)).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn pacing_follows_the_device_frames_and_repeats_a_still_screen() {
        let screen = Arc::new(Screen::default());
        let mut source = DeviceSource::new("A", None, screen.clone());
        let s = fps(32, 32, 10);
        solid(&screen, 10);
        let first = take(&mut source, s, 0);
        assert_eq!(first.pixel(0, 0), [10, 10, 10, 255]);

        // Nothing new: the tick waits out one interval, then the same frame again, not redrawn.
        let started = Instant::now();
        let again = take(&mut source, s, 1);
        assert!(started.elapsed() >= s.interval() - Duration::from_millis(5), "{:?}", started.elapsed());
        assert!(Arc::ptr_eq(&again.bgra, &first.bgra));

        // A frame the device presents during the wait is taken when it comes, not a tick later.
        let poster = screen.clone();
        let post = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            solid(&poster, 20);
        });
        let started = Instant::now();
        let next = take(&mut source, s, 2);
        let waited = started.elapsed();
        post.join().ok();
        assert_eq!(next.pixel(0, 0), [20, 20, 20, 255]);
        assert!(waited >= Duration::from_millis(25) && waited < s.interval(), "{waited:?}");

        // The tick's other sizes are drawn from the same device frame, without another wait.
        let started = Instant::now();
        let other = take(&mut source, fps(48, 32, 10), 2);
        assert!(started.elapsed() < Duration::from_millis(20));
        assert_eq!((other.width, other.height, other.pixel(47, 31)), (48, 32, [20, 20, 20, 255]));

        // Frames the device presented between ticks: the newest is taken at once.
        solid(&screen, 30);
        solid(&screen, 40);
        let started = Instant::now();
        assert_eq!(take(&mut source, s, 3).pixel(0, 0), [40, 40, 40, 255]);
        assert!(started.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn frames_are_scaled_to_each_subscriber_size_from_the_region() {
        let screen = Arc::new(Screen::default());
        // A device at half size: 2x2 frames of a 4x4 screen; the region is the screen's right half.
        let region = Some(Region { x: 2, y: 0, width: 2, height: 4 });
        let mut source = DeviceSource::new("A", region, screen.clone());
        screen.post(2, 2, (4, 4), [[1, 0, 0, 255], [2, 0, 0, 255], [3, 0, 0, 255], [4, 0, 0, 255]].concat());
        let f = take(&mut source, fps(32, 64, 30), 0);
        assert_eq!((f.width, f.height), (32, 64));
        assert_eq!((f.pixel(0, 0), f.pixel(31, 63)), ([0, 0, 2, 255], [0, 0, 4, 255]), "the right column, top and bottom");
    }

    #[test]
    fn no_first_frame_is_a_capture_error_naming_the_device() {
        let mut source = DeviceSource::new("Pixel_9", None, Arc::new(Screen::default()));
        source.first_frame = Duration::from_millis(20);
        let err = source.frame(fps(32, 32, 30), tick(0)).expect_err("no frame");
        assert!(matches!(&err, Error::Capture(m) if m.contains("Pixel_9")), "{err:?}");
    }

    #[test]
    fn a_region_off_the_screen_fails_when_drawn() {
        let screen = Arc::new(Screen::default());
        let mut source = DeviceSource::new("A", Some(Region { x: 100, y: 0, width: 10, height: 10 }), screen.clone());
        solid(&screen, 1);
        let err = source.frame(fps(32, 32, 30), tick(0)).expect_err("off screen");
        assert!(matches!(&err, Error::Invalid(m) if m.contains("device:A") && m.contains("outside the 64x64")), "{err:?}");
    }

    #[test]
    fn a_frame_buffer_comes_back_once_nobody_holds_it() {
        let screen = Screen::default();
        assert!(screen.post(1, 1, (1, 1), vec![0; 4]).is_empty(), "nothing replaced");
        let spare = screen.post(1, 1, (1, 1), vec![0; 4]);
        assert!(spare.is_empty() && spare.capacity() >= 4, "the replaced frame's buffer, emptied");
        let held = screen.newer(None, Duration::ZERO).ok().flatten();
        assert_eq!(screen.post(1, 1, (1, 1), vec![0; 4]).capacity(), 0, "a source still holds it");
        drop(held);
    }

    /// Opens `device:` sources over screens the test posts to.
    struct Fakes(Arc<Mutex<Vec<Arc<Screen>>>>);

    impl Opener for Fakes {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> Result<Box<dyn Source>> {
            let SourceSpec::Device(d) = source else { return Err(Error::Invalid("not a device".into())) };
            let screen = Arc::new(Screen::default());
            self.0.lock().unwrap_or_else(|p| p.into_inner()).push(screen.clone());
            Ok(Box::new(DeviceSource::new(&d.device, d.region, screen)))
        }
    }

    struct NoCameras;

    struct Quiet;

    impl Signals for Quiet {
        fn wait(&mut self, timeout: Duration) -> Result<bool> {
            thread::sleep(timeout.min(Duration::from_millis(50)));
            Ok(false)
        }
    }

    impl HotPlug for NoCameras {
        fn cameras(&self) -> Result<Vec<HostCamera>> {
            Ok(Vec::new())
        }

        fn watch(&self) -> Result<Box<dyn Signals>> {
            Ok(Box::new(Quiet))
        }
    }

    #[test]
    fn a_source_device_going_away_is_source_missing() {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let hub = Hub::with_hotplug(Fakes(opened.clone()), NoCameras);
        let events = hub.events().unwrap_or_else(|e| panic!("{e}"));
        let source = SourceSpec::parse("device:A:0,0,32,32").unwrap_or_else(|e| panic!("{e}"));
        let sub = hub.subscribe(&source, fps(32, 32, 60)).unwrap_or_else(|e| panic!("{e}"));
        let screen = opened.lock().unwrap_or_else(|p| p.into_inner()).first().cloned().expect("opened");
        solid(&screen, 5);
        let frame = sub.recv_timeout(Duration::from_secs(5)).ok().flatten().expect("a frame");
        assert_eq!(frame.pixel(0, 0), [5, 5, 5, 255]);

        screen.close("A shut down");
        let failure = loop {
            match sub.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        let missing = Error::Missing("A shut down".into());
        assert_eq!(failure, Err(missing.clone()));
        let event = loop {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(e @ Event::SourceMissing { .. })) => break e,
                Ok(Some(_)) => continue,
                other => panic!("no SourceMissing: {other:?}"),
            }
        };
        assert_eq!(event, Event::SourceMissing { source: "device:A:0,0,32,32".into(), error: missing });
    }

    /// The Android reader against a fake display socket: the hello it sends, a frame it posts,
    /// and the socket closing as the device going away.
    #[cfg(all(unix, feature = "device-source"))]
    #[test]
    fn the_android_reader_posts_display_frames_and_reports_the_device_gone() {
        use sim_core::stream::FrameHeader;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        // Under /tmp: the socket path must fit a unix socket address (104 bytes on macOS).
        let dir = std::path::PathBuf::from("/tmp").join(format!("simee-camera-device-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("simee")).unwrap_or_else(|e| panic!("{e}"));
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).unwrap_or_else(|e| panic!("{e}"));
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap_or_else(|e| panic!("{e}"));
            let mut lines = BufReader::new(conn.try_clone().unwrap_or_else(|e| panic!("{e}"))).lines();
            let got: Vec<String> = (0..2).filter_map(|_| lines.next()?.ok()).collect();
            let h = FrameHeader { width: 2, height: 1, device_width: 4, device_height: 2, frame: 1, len: 8 };
            conn.write_all(&h.encode()).and_then(|()| conn.write_all(&[1, 2, 3, 4, 5, 6, 7, 8])).unwrap_or_else(|e| panic!("{e}"));
            // Wait for the client to have read it, then close: the device shut down.
            thread::sleep(Duration::from_millis(100));
            got
        });
        let screen = Arc::new(Screen::default());
        let reader = android::Reader::start(&dir, "A", screen.clone()).unwrap_or_else(|e| panic!("{e}"));
        let shot = screen.newer(None, Duration::from_secs(5)).ok().flatten().expect("a frame");
        assert_eq!((shot.width, shot.height, shot.screen, shot.rgba.as_slice()), (2, 1, (4, 2), &[1, 2, 3, 4, 5, 6, 7, 8][..]));
        assert_eq!(server.join().unwrap_or_default(), ["shm", "hello 4294967295"]);
        let gone = screen.newer(Some(shot.seq), Duration::from_secs(5));
        assert!(matches!(&gone, Err(Error::Missing(m)) if m.contains("A's display closed")), "{gone:?}");
        drop(reader);
        std::fs::remove_dir_all(dir).ok();

        // No display socket: the device isn't running.
        let err = android::Reader::start(std::path::Path::new("/nonexistent-avd"), "B", Arc::new(Screen::default())).err();
        assert!(matches!(&err, Some(Error::Missing(m)) if m.contains("B isn't running")), "{err:?}");
    }

    #[cfg(all(unix, not(target_os = "macos"), feature = "device-source"))]
    #[test]
    fn an_ios_device_source_is_unsupported_off_macos() {
        let err = ios("iPhone 16", Arc::new(Screen::default())).err();
        assert!(matches!(&err, Some(Error::Unsupported(m)) if m.contains("needs macOS")), "{err:?}");
    }

    #[cfg(not(feature = "device-source"))]
    #[test]
    fn a_device_source_parses_but_is_unsupported_without_the_device_source_feature() {
        let d = spec("device:Pixel_9:0,0,10,10");
        assert_eq!(d.device, "Pixel_9");
        let err = open(&d).err();
        assert!(matches!(&err, Some(Error::Unsupported(m)) if m.contains("served by simee, not emu")), "{err:?}");
    }
}
