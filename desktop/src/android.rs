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
        let mut frames = sim_android::display::Frames::new(display.try_clone()?).with_fallback(display.try_clone()?);
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::FileExt;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    use sim_core::stream::{FrameHeader, Record, ring};

    use super::*;

    /// A fake emulator announces a ring and a frame in it; the window's pixels get that frame.
    #[test]
    fn frames_from_the_shared_ring_reach_the_window() {
        let dir = PathBuf::from("/tmp").join(format!("rs-android-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("emu-rs")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let ring_path = dir.join("emu-rs/frames-1-0.ring");
        let rp = ring_path.clone();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let asked: Vec<String> = (0..2).map(|_| lines.next().expect("line").expect("utf8")).collect();
            // A 2x1 frame of a 4x2 device in slot 1 of a 3-slot ring.
            let layout = ring::Layout::new(3, 8);
            let file = std::fs::File::create(&rp).expect("ring");
            file.set_len(layout.file_len()).expect("len");
            file.write_all_at(&layout.encode(), 0).expect("header");
            let slot = ring::Slot { seq: 2, frame: 7, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
            file.write_all_at(&slot.encode(), layout.slot_header_offset(1)).expect("slot");
            file.write_all_at(&[1, 2, 3, 4, 5, 6, 7, 8], layout.slot_data_offset(1)).expect("pixels");
            let path = rp.to_str().expect("utf8").as_bytes();
            let mut msg = Record::Ring { path_len: path.len() as u32 }.encode().to_vec();
            msg.extend(path);
            msg.extend(Record::Shared { header: FrameHeader::new(2, 1, 4, 2, 7).expect("size"), slot: 1 }.encode());
            conn.write_all(&msg).expect("send");
            // Hold the connection open until the test has looked.
            let _ = finished.recv();
            asked
        });
        let screen = Screen::connect("pixel", dir.clone(), 1080).expect("connect");
        let start = Instant::now();
        while screen.pixels.size() != (2, 1) {
            assert!(start.elapsed() < Duration::from_secs(5), "no frame");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(screen.pixels.rgba(), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(screen.device_size(), Some((4, 2)));
        assert!(screen.alive());
        done.send(()).expect("done");
        assert_eq!(server.join().expect("server"), ["shm", "hello 1080"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
