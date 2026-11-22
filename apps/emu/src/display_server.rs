//! Live display and input for local front ends (the desktop app) over a unix socket at
//! `<avd>/emu-rs/display.sock`.
//!
//! Client → server, text lines:
//! - `hello <max_width>`: start streaming, downscaled by a whole factor to at most that width;
//! - `touch <x> <y> down|move|up` in guest pixels;
//! - `key <name>` (`HOME`, `BACK`, `POWER`, `A`, ... as the console's `event` names them);
//! - `wheel <ticks>` on the rotary/scroll device.
//!
//! Server → client, per frame: `b"EMUF"`, then little-endian u32 width, u32 height, u32 guest
//! width, u32 guest height, u64 frame number, u32 byte count, then RGBA rows. A frame goes out
//! when the guest posted a new one, at most every `MIN_INTERVAL`.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use emu_vmm::virtio::gpu::FrameStore;

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
    let Ok(writer) = conn.try_clone() else { return };
    let max_width = Arc::new(AtomicU32::new(0));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (display, max_width, done, stop) = (display.clone(), max_width.clone(), done.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("display-frames".into()).spawn(move || frames(writer, display, max_width, done, stop));
    }
    let mut finger_down = false;
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        let w: Vec<&str> = line.split_whitespace().collect();
        let g = inputs.lock().unwrap_or_else(|e| e.into_inner());
        match (w.as_slice(), g.as_ref()) {
            (["hello", mw], _) => max_width.store(mw.parse().unwrap_or(0), Ordering::Relaxed),
            (["touch", x, y, phase], Some(i)) => {
                let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) else { continue };
                match *phase {
                    "down" | "move" => {
                        finger_down = true;
                        i.touch.touch(x as i32, y as i32, true);
                    }
                    _ => {
                        if finger_down {
                            i.touch.touch(x as i32, y as i32, false);
                        }
                        finger_down = false;
                    }
                }
            }
            (["key", name], Some(i)) => {
                if let Some(code) = emu_vmm::virtio::input::keycode(name) {
                    i.keyboard.tap_key(code);
                }
            }
            (["wheel", ticks], Some(i)) => {
                if let Ok(t) = ticks.parse::<i32>() {
                    i.rotary.send(vec![
                        emu_vmm::virtio::input::InputEvent::new(emu_vmm::virtio::input::EV_REL, emu_vmm::virtio::input::REL_WHEEL, t),
                        emu_vmm::virtio::input::InputEvent::syn(),
                    ]);
                }
            }
            _ => {}
        }
    }
    done.store(true, Ordering::Relaxed);
}

/// Averages `factor` x `factor` blocks.
fn downscale(w: u32, h: u32, rgba: &[u8], factor: u32) -> (u32, u32, Vec<u8>) {
    if factor <= 1 {
        return (w, h, rgba.to_vec());
    }
    let (ow, oh) = (w / factor, h / factor);
    let mut out = Vec::with_capacity(ow as usize * oh as usize * 4);
    let n = factor * factor;
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0u32; 4];
            for dy in 0..factor {
                let row = ((oy * factor + dy) * w + ox * factor) as usize * 4;
                for dx in 0..factor as usize {
                    for c in 0..4 {
                        acc[c] += u32::from(rgba[row + dx * 4 + c]);
                    }
                }
            }
            out.extend(acc.map(|v| (v / n) as u8));
        }
    }
    (ow, oh, out)
}

fn frames(mut out: UnixStream, display: Arc<FrameStore>, max_width: Arc<AtomicU32>, done: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let mut sent = 0u64;
    let mut last = Instant::now() - MIN_INTERVAL;
    while !done.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(8));
        let mw = max_width.load(Ordering::Relaxed);
        let n = display.frames();
        if mw == 0 || n == sent || last.elapsed() < MIN_INTERVAL {
            continue;
        }
        let f = display.snapshot();
        if f.width == 0 {
            continue;
        }
        let Some(rgba) = emu_vmm::renderer_screenshot(f.width, f.height) else { continue };
        let factor = f.width.div_ceil(mw.max(1)).max(1);
        let (w, h, px) = downscale(f.width, f.height, &rgba, factor);
        let mut hdr = Vec::with_capacity(32);
        hdr.extend_from_slice(b"EMUF");
        for v in [w, h, f.width, f.height] {
            hdr.extend_from_slice(&v.to_le_bytes());
        }
        hdr.extend_from_slice(&n.to_le_bytes());
        hdr.extend_from_slice(&(px.len() as u32).to_le_bytes());
        if out.write_all(&hdr).and_then(|()| out.write_all(&px)).is_err() {
            break;
        }
        sent = n;
        last = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_averages_blocks() {
        // 2x2 image: one white pixel, three black; factor 2 → one grey pixel.
        let px = [255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        assert_eq!(downscale(2, 2, &px, 2), (1, 1, vec![63, 63, 63, 255]));
        assert_eq!(downscale(2, 2, &px, 1).2, px.to_vec());
    }
}
