//! A live Android device: frames from its display socket (`<avd>/simee/display.sock`) stream into
//! a [`Pixels`], which wakes only the window showing it; touches and key taps go back as stream
//! lines. Frames come through the emulator's shared frame ring (only a 32-byte notice per frame
//! crosses the socket; the pixels are read from the ring straight into the buffer handed to
//! `Pixels`), or on the socket from an emulator without one. The emulator runs in its own process,
//! so a crashed guest can't take the window down.
//! While the window is hidden the frame stream pauses: the emulator is told to stop sending
//! (`hello 0`, so it stops scaling frames for us too) and the frame thread reads nothing; shown
//! again, it asks for frames and the emulator sends its newest at once.
//! What the socket doesn't carry (typing with shift, rotation, the hinge) goes over the console
//! through `sim-android`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

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
    max_width: u32,
    shown: Arc<Shown>,
    /// Frames read so far.
    read: Arc<AtomicU64>,
}

/// Whether the window shows the screen; the frame thread waits on it while it doesn't.
struct Shown {
    visible: Mutex<bool>,
    changed: Condvar,
}

impl Shown {
    fn get(&self) -> bool {
        *self.visible.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether it changed.
    fn set(&self, visible: bool) -> bool {
        let mut v = self.visible.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = std::mem::replace(&mut *v, visible) != visible;
        self.changed.notify_all();
        changed
    }

    /// Waits while hidden; false once `alive` says the screen is gone.
    fn wait(&self, alive: &AtomicBool) -> bool {
        let mut v = self.visible.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*v && alive.load(Ordering::Relaxed) {
            v = self.changed.wait(v).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        alive.load(Ordering::Relaxed)
    }
}

impl Screen {
    /// Connects and asks for frames at most `max_width` pixels wide.
    pub fn connect(udid: &str, avd_dir: PathBuf, max_width: u32) -> Result<Screen, String> {
        let mut display = sim_android::display::Display::connect(&avd_dir)?;
        display.request_shared()?;
        display.send(&Input::Hello { max_width })?;
        let mut frames = sim_android::display::Frames::new(display.try_clone()?).with_fallback(display.try_clone()?);
        let (pixels, device, alive) = (Pixels::new(), Arc::new((AtomicU32::new(0), AtomicU32::new(0))), Arc::new(AtomicBool::new(true)));
        let shown = Arc::new(Shown { visible: Mutex::new(true), changed: Condvar::new() });
        let read = Arc::new(AtomicU64::new(0));
        let (p, d, a, sh, r) = (pixels.clone(), device.clone(), alive.clone(), shown.clone(), read.clone());
        std::thread::Builder::new()
            .name("simee-android-frames".into())
            .spawn(move || {
                // A frame announced just before the window hid is passed over unread.
                while sh.wait(&a)
                    && let Ok((header, rgba)) = frames.next_frame_when(|| sh.get())
                {
                    r.fetch_add(1, Ordering::Relaxed);
                    d.0.store(header.device_width, Ordering::Relaxed);
                    d.1.store(header.device_height, Ordering::Relaxed);
                    p.set(header.width, header.height, rgba);
                }
                a.store(false, Ordering::Relaxed);
            })
            .map_err(|e| e.to_string())?;
        Ok(Screen { udid: udid.to_owned(), avd_dir, pixels, link: Arc::new(Mutex::new(display)), device, alive, max_width, shown, read })
    }

    /// Pauses the frame stream while the window is hidden (minimised, or covered where the
    /// system says so) and resumes it with the newest frame when it shows again.
    pub fn set_visible(&self, visible: bool) {
        if self.shown.set(visible) {
            self.send(&Input::Hello { max_width: if visible { self.max_width } else { 0 } });
        }
    }

    /// Frames read from the emulator so far.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn frames_read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
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
            // A frame thread paused for a hidden window finds out too.
            self.shown.changed.notify_all();
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

impl Drop for Screen {
    /// Ends the frame thread: it holds its own handle on the connection, so it would otherwise
    /// keep reading (or, paused, keep waiting) after the screen is gone.
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        self.shown.changed.notify_all();
        let link = self.link.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(s) = link.try_clone() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
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
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let ring_path = dir.join("simee/frames-1-0.ring");
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

    /// What the fake emulator saw: the client's lines, frames it announced while asked for none
    /// (only the one already on its way when the window hid), and the newest frame it made.
    #[derive(Default)]
    struct Seen {
        lines: Mutex<Vec<String>>,
        sent_paused: AtomicU64,
        newest: AtomicU64,
    }

    /// A fake emulator that streams like `emu`'s display server: the guest makes a frame every
    /// 4 ms, written into a two-slot ring and announced while the client's last `hello` asks for
    /// frames (a `hello 0` stops it); one frame is announced right after a `hello 0`, as if it
    /// was already on its way. A frame's pixels are its number.
    fn fake_emulator(listener: UnixListener, ring_path: PathBuf, seen: Arc<Seen>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let (max_width, in_flight, gone) =
                (Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (mw, fl, g, s) = (max_width.clone(), in_flight.clone(), gone.clone(), seen.clone());
            let reader = BufReader::new(conn.try_clone().expect("clone"));
            std::thread::spawn(move || {
                for line in reader.lines().map_while(Result::ok) {
                    if let Some(w) = line.strip_prefix("hello ").and_then(|w| w.parse().ok()) {
                        mw.store(w, Ordering::Relaxed);
                        fl.store(w == 0, Ordering::Relaxed);
                    }
                    s.lines.lock().expect("lines").push(line);
                }
                g.store(true, Ordering::Relaxed);
            });
            let layout = ring::Layout::new(2, 8);
            let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&ring_path).expect("ring");
            file.set_len(layout.file_len()).expect("len");
            file.write_all_at(&layout.encode(), 0).expect("header");
            let mut announced = false;
            let mut frame = 0u64;
            while !gone.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(4));
                frame += 1;
                seen.newest.store(frame, Ordering::Relaxed);
                let asked = max_width.load(Ordering::Relaxed) > 0;
                let on_its_way = in_flight.swap(false, Ordering::Relaxed);
                if !asked && !on_its_way {
                    continue;
                }
                let n = (frame % 2) as u32;
                let slot = ring::Slot { seq: 2 * frame, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
                file.write_all_at(&slot.encode(), layout.slot_header_offset(n)).expect("slot");
                file.write_all_at(&[(frame as u32).to_le_bytes(); 2].concat(), layout.slot_data_offset(n)).expect("pixels");
                let mut msg = Vec::new();
                if !announced {
                    let path = ring_path.to_str().expect("utf8").as_bytes();
                    msg.extend(Record::Ring { path_len: path.len() as u32 }.encode());
                    msg.extend(path);
                    announced = true;
                }
                msg.extend(Record::Shared { header: FrameHeader::new(2, 1, 4, 2, frame).expect("size"), slot: n }.encode());
                if conn.write_all(&msg).is_err() {
                    break;
                }
                if !asked {
                    seen.sent_paused.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The frame number the window's pixels show.
    fn shown(screen: &Screen) -> u64 {
        let rgba = screen.pixels.rgba();
        u64::from(u32::from_le_bytes(rgba[..4].try_into().expect("a frame")))
    }

    /// #21: a hidden window reads no frames, and the emulator stops sending them; shown again,
    /// the newest frame arrives at once.
    #[test]
    fn a_hidden_window_reads_no_frames_and_catches_up_when_shown() {
        let dir = PathBuf::from("/tmp").join(format!("rs-hidden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let seen = Arc::new(Seen::default());
        let server = fake_emulator(listener, dir.join("simee/frames-1-0.ring"), seen.clone());
        let screen = Screen::connect("pixel", dir.clone(), 1080).expect("connect");
        wait_until("frames while shown", || screen.frames_read() >= 3);

        screen.set_visible(false);
        screen.set_visible(false); // no change: nothing sent
        wait_until("the emulator to hear the window hid", || seen.lines.lock().expect("lines").len() == 3);
        // Room for the frame that was on its way to arrive (and be passed over).
        std::thread::sleep(Duration::from_millis(40));
        let (read, newest) = (screen.frames_read(), seen.newest.load(Ordering::Relaxed));
        std::thread::sleep(Duration::from_millis(300));
        let made = seen.newest.load(Ordering::Relaxed) - newest;
        assert!(made >= 30, "the guest kept making frames ({made})");
        assert_eq!(screen.frames_read(), read, "no frame read while hidden");
        assert_eq!(seen.sent_paused.load(Ordering::Relaxed), 1, "only the frame already on its way was announced");
        let last_shown = shown(&screen);

        let before = seen.newest.load(Ordering::Relaxed);
        let start = Instant::now();
        screen.set_visible(true);
        wait_until("a frame after showing", || screen.frames_read() > read);
        println!("shown again: newest frame in {:.1} ms (the fake guest makes one every 4 ms)", start.elapsed().as_secs_f64() * 1000.0);
        wait_until("the newest frame", || shown(&screen) > before);
        assert!(shown(&screen) > last_shown);
        assert_eq!(*seen.lines.lock().expect("lines"), ["shm", "hello 1080", "hello 0", "hello 1080"]);

        drop(screen);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(dir);
    }
}
