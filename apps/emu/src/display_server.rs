//! Live display and input for local front ends (the desktop app) over a unix socket at
//! `<avd>/emu-rs/display.sock`. The protocol (frame headers one way, input lines the other) is
//! `sim_core::stream`, shared with the desktop window that reads it. A frame goes out when the
//! guest posted a new one, at most every `MIN_INTERVAL`.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use emu_vmm::display::FrameStore;
use sim_core::gesture::Phase;
use sim_core::pixels::{downscale, downscale_factor};
use sim_core::stream::{FrameHeader, Input};

const MIN_INTERVAL: Duration = Duration::from_millis(33);

pub struct DisplayServer {
    path: PathBuf,
}

impl Drop for DisplayServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Starts serving; the socket is removed when the returned guard drops.
pub fn start(
    avd_dir: &Path,
    display: Arc<FrameStore>,
    inputs: Arc<Mutex<Option<emu_vmm::Inputs>>>,
    stop: Arc<AtomicBool>,
) -> Option<DisplayServer> {
    let path = avd_dir.join("emu-rs/display.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    std::thread::Builder::new()
        .name("display-server".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (d, i, s) = (display.clone(), inputs.clone(), stop.clone());
                let _ = std::thread::Builder::new().name("display-client".into()).spawn(move || client(conn, d, i, s));
            }
        })
        .ok()?;
    Some(DisplayServer { path })
}

fn client(conn: UnixStream, display: Arc<FrameStore>, inputs: Arc<Mutex<Option<emu_vmm::Inputs>>>, stop: Arc<AtomicBool>) {
    let max_width = Arc::new(AtomicU32::new(0));
    let done = Arc::new(AtomicBool::new(false));
    // Frames go only to clients that ask (`hello`): input-only connections (one per API touch)
    // get no frame thread.
    let mut writer = conn.try_clone().ok();
    let mut finger_down = false;
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        let Some(input) = Input::parse(&line) else { continue };
        let g = inputs.lock().unwrap_or_else(|e| e.into_inner());
        match (input, g.as_ref()) {
            (Input::Hello { max_width: mw }, _) => {
                max_width.store(mw, Ordering::Relaxed);
                if let Some(writer) = writer.take() {
                    let (display, max_width, done, stop) = (display.clone(), max_width.clone(), done.clone(), stop.clone());
                    let _ = std::thread::Builder::new()
                        .name("display-frames".into())
                        .spawn(move || frames(writer, display, max_width, done, stop));
                }
            }
            (Input::Touch { x, y, phase }, Some(emu_vmm::Inputs { touch: Some(touch), .. })) => match phase {
                Phase::Down | Phase::Move => {
                    finger_down = true;
                    touch.touch(x as i32, y as i32, true);
                }
                Phase::Up => {
                    if finger_down {
                        touch.touch(x as i32, y as i32, false);
                    }
                    finger_down = false;
                }
            },
            // A console key name, or a raw Linux keycode (as the console's `event send` takes).
            (Input::Key(name), Some(i)) => {
                if let Some(code) = sim_core::goldfish::keycode(&name) {
                    i.keyboard.tap_key(code);
                }
            }
            // Clicks become the rotary's units here, once; the console's `event send
            // EV_REL:REL_WHEEL:<n>` stays raw.
            (Input::Wheel(clicks), Some(i)) => {
                i.rotary.send(vec![
                    emu_vmm::virtio::input::InputEvent::new(
                        emu_vmm::virtio::input::EV_REL,
                        emu_vmm::virtio::input::REL_WHEEL,
                        sim_core::stream::wheel_units(clicks),
                    ),
                    emu_vmm::virtio::input::InputEvent::syn(),
                ]);
            }
            _ => {}
        }
    }
    done.store(true, Ordering::Relaxed);
}

fn frames(mut out: UnixStream, display: Arc<FrameStore>, max_width: Arc<AtomicU32>, done: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let mut sent = 0u64;
    let mut last = Instant::now() - MIN_INTERVAL;
    // One readback buffer for the stream's life.
    let mut rgba = Vec::new();
    while !done.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        // At most one frame per MIN_INTERVAL; then wait for a new one (woken by the frame, or
        // every 100 ms to notice the client or the emulator going away).
        let since = last.elapsed();
        if since < MIN_INTERVAL {
            std::thread::sleep(MIN_INTERVAL - since);
        }
        let n = display.wait_newer(sent, Duration::from_millis(100));
        let mw = max_width.load(Ordering::Relaxed);
        if mw == 0 || n == sent {
            continue;
        }
        // Only the size: the pixels come from the renderer's own capture.
        let (width, height, _) = display.geometry();
        if width == 0 || !emu_vmm::renderer_screenshot_into(width, height, &mut rgba) {
            continue;
        }
        let (w, h, px) = downscale(width, height, &rgba, downscale_factor(width, mw));
        let hdr = FrameHeader { width: w, height: h, device_width: width, device_height: height, frame: n, len: px.len() as u32 };
        if out.write_all(&hdr.encode()).and_then(|()| out.write_all(&px)).is_err() {
            break;
        }
        sent = n;
        last = Instant::now();
    }
}
