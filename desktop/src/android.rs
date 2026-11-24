//! A live Android device: frames from its display socket (`<avd>/simee/display.sock`) stream into
//! a [`Pixels`], which wakes only the window showing it; touches and key taps go back as stream
//! lines. Frames come through the emulator's shared frame ring (only a 32-byte notice per frame
//! crosses the socket; the pixels are read from the ring straight into the buffer handed to
//! `Pixels`), or on the socket from an emulator without one. The emulator runs in its own process,
//! so a crashed guest can't take the window down.
//! What the socket doesn't carry (typing with shift, rotation, the hinge, screenshots) goes over
//! the console through `sim-android`, on the screen's console worker: a console session can take
//! seconds on a busy emulator, and the window never waits for one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rune::Pixels;
use sim_core::gesture::Phase;
use sim_core::hid::{KeyStroke, linux_keycode};
use sim_core::stream::Input;

use crate::work::{Outcome, Worker};

/// How the console worker finds its device: looked up again for every job (on the worker's
/// thread), so an AVD started or restarted elsewhere since the screen connected is the one the
/// job drives, never a copy that still says "not running".
pub type Lookup = Box<dyn Fn() -> Result<sim_android::Device, String> + Send>;

/// The lookup of AVD `udid`: an exact name loads just that AVD and the running-instance files.
pub fn lookup(backend: sim_android::Android, udid: String) -> Lookup {
    Box::new(move || backend.device(&udid))
}

/// Strokes queued for the console at most; more typed while a stalled emulator holds the queue
/// are dropped.
pub const STROKE_LIMIT: usize = 256;

/// The strokes of the newest console stroke job, until the worker starts it (`None` then).
type QueuedStrokes = Arc<Mutex<Option<Vec<KeyStroke>>>>;

pub struct Screen {
    pub udid: String,
    pub avd_dir: PathBuf,
    pub pixels: Pixels,
    link: Arc<Mutex<sim_android::display::Display>>,
    device: Arc<(AtomicU32, AtomicU32)>,
    alive: Arc<AtomicBool>,
    console: Worker<Lookup>,
    /// The last job queued on `console`, when it is a stroke job: strokes typed behind it join
    /// it (one console session) instead of queueing a session each.
    tail: Mutex<Option<QueuedStrokes>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The display socket's `key` lines for `strokes`, when it can carry all of them: it taps one key
/// at a time, so a stroke with shift (or a key without a Linux keycode) needs the console.
pub fn display_keys(strokes: &[KeyStroke]) -> Option<Vec<String>> {
    strokes.iter().map(|s| if s.shift { None } else { linux_keycode(s.usage).map(|c| c.to_string()) }).collect()
}

impl Screen {
    /// Connects and asks for frames at most `max_width` pixels wide. Console work for the
    /// device goes to `console`.
    pub fn connect(udid: &str, avd_dir: PathBuf, max_width: u32, console: Worker<Lookup>) -> Result<Screen, String> {
        let mut display = sim_android::display::Display::connect(&avd_dir)?;
        display.request_shared()?;
        display.send(&Input::Hello { max_width })?;
        let mut frames = sim_android::display::Frames::new(display.try_clone()?).with_fallback(display.try_clone()?);
        let (pixels, device, alive) = (Pixels::new(), Arc::new((AtomicU32::new(0), AtomicU32::new(0))), Arc::new(AtomicBool::new(true)));
        let (p, d, a) = (pixels.clone(), device.clone(), alive.clone());
        std::thread::Builder::new()
            .name("simee-android-frames".into())
            .spawn(move || {
                // Only the newest announced frame: the window shows nothing older.
                while let Ok((header, rgba)) = frames.newest_frame() {
                    d.0.store(header.device_width, Ordering::Relaxed);
                    d.1.store(header.device_height, Ordering::Relaxed);
                    p.set(header.width, header.height, rgba);
                }
                a.store(false, Ordering::Relaxed);
            })
            .map_err(|e| e.to_string())?;
        let tail = Mutex::new(None);
        Ok(Screen { udid: udid.to_owned(), avd_dir, pixels, link: Arc::new(Mutex::new(display)), device, alive, console, tail })
    }

    /// Runs a console job for the device (looked up when the job starts), after the ones already
    /// queued. `false` when the queue is full (a stalled emulator) and the job was dropped.
    pub fn console(&self, job: impl FnOnce(&sim_android::Device) -> Outcome + Send + 'static) -> bool {
        let mut tail = lock(&self.tail);
        // Strokes typed after this job go behind it, not into a stroke job ahead of it.
        *tail = None;
        self.console.run(move |find: &Lookup| job(&find()?))
    }

    /// Types key strokes: as display socket taps when it can carry them and no console work is
    /// queued ahead (which would otherwise land after them), else on the console worker, joined
    /// to the stroke job queued last if it hasn't started (key repeat on a stalled emulator is
    /// one queued session, not one per key).
    pub fn strokes(&self, strokes: Vec<KeyStroke>) {
        let mut tail = lock(&self.tail);
        if let Some(keys) = display_keys(&strokes)
            && self.console.idle()
        {
            drop(tail);
            keys.into_iter().for_each(|k| self.send(&Input::Key(k)));
            return;
        }
        if let Some(queued) = tail.as_ref()
            && let Some(waiting) = lock(queued).as_mut()
        {
            let room = STROKE_LIMIT.saturating_sub(waiting.len());
            waiting.extend(strokes.into_iter().take(room));
            return;
        }
        let mut strokes = strokes;
        strokes.truncate(STROKE_LIMIT);
        let queued: QueuedStrokes = Arc::new(Mutex::new(Some(strokes)));
        let job = queued.clone();
        if self.console.run(move |find: &Lookup| {
            // Taken (and the lock let go) before the session: typing joins a later job meanwhile.
            let strokes = lock(&job).take();
            match strokes {
                Some(strokes) => find()?.strokes(&strokes).map(|()| None),
                None => Ok(None),
            }
        }) {
            *tail = Some(queued);
        }
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
    use crate::work::Outbox;

    /// A console worker for an AVD that isn't running: its jobs fail at once.
    fn console(outbox: Outbox) -> Worker<Lookup> {
        counted_console(outbox, Arc::default())
    }

    /// The same, counting the lookups (one per job that reaches the device).
    fn counted_console(outbox: Outbox, lookups: Arc<std::sync::atomic::AtomicUsize>) -> Worker<Lookup> {
        let find: Lookup = Box::new(move || {
            lookups.fetch_add(1, Ordering::Relaxed);
            let info = sim_core::device::DeviceInfo {
                udid: "pixel".into(),
                name: "pixel".into(),
                state: sim_core::device::DeviceState::Booted,
                runtime_id: String::new(),
                device_type_id: "custom".into(),
                available: true,
                platform: sim_core::platform::Platform::Android,
                form_factor: None,
                sub_platform: None,
            };
            Ok(sim_android::Device { info, avd: None, running: None })
        });
        Worker::start("test-console", find, outbox)
    }

    fn stroke(c: char) -> KeyStroke {
        sim_core::hid::keystroke(c).expect("US key")
    }

    /// A display socket that accepts one client and reads whatever it sends until the test ends.
    fn quiet_display(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp").join(format!("rs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            BufReader::new(conn).lines().map_while(Result::ok).for_each(drop);
        });
        dir
    }

    fn wait_idle(screen: &Screen) {
        let start = Instant::now();
        while !screen.console.idle() {
            assert!(start.elapsed() < Duration::from_secs(5), "console idle");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Key repeat on a stalled emulator: the strokes typed while a console session holds the
    /// queue join one queued session, in order, instead of one session each; a console action
    /// typed between them keeps its place.
    #[test]
    fn strokes_behind_a_stalled_console_join_one_session() {
        let dir = quiet_display("coalesce");
        let (outbox, lookups) = (Outbox::default(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let screen = Screen::connect("pixel", dir.clone(), 1080, counted_console(outbox.clone(), lookups.clone())).expect("connect");
        let (release, gate) = std::sync::mpsc::channel::<()>();
        assert!(screen.console(move |_| {
            gate.recv().ok();
            Ok(None)
        }));
        for c in "ABCDEFGHIJ".repeat(10).chars() {
            screen.strokes(vec![stroke(c)]);
        }
        assert!(screen.console(|_| Ok(Some("rotated".into()))));
        screen.strokes(vec![stroke('Z')]);
        screen.strokes(vec![stroke('z')]);
        release.send(()).expect("release");
        wait_idle(&screen);
        // The gate, the 100 strokes, the action, then 'Z' and 'z': four sessions, not 103.
        assert_eq!(lookups.load(Ordering::Relaxed), 4);
        let lines = outbox.take();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("isn't running") && lines[1] == "rotated" && lines[2].contains("isn't running"), "{lines:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The console worker looks the device up for every job, so an AVD started elsewhere after
    /// the screen connected (while the window held a "not running" copy) gets the next action.
    #[test]
    fn console_jobs_look_the_device_up_when_they_run() {
        let root = std::env::temp_dir().join(format!("rs-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (avds, running) = (root.join("avd"), root.join("running"));
        std::fs::create_dir_all(&avds).expect("avd home");
        std::fs::create_dir_all(&running).expect("running dir");
        std::fs::write(avds.join("pixel.ini"), "").expect("ini");
        let mut backend = sim_android::Android::from_env();
        (backend.env.avd_home, backend.env.sdk_root, backend.running_dir) = (avds, None, Some(running.clone()));
        let dir = quiet_display("lookup");
        let outbox = Outbox::default();
        let worker = Worker::start("test-lookup", lookup(backend, "pixel".into()), outbox.clone());
        let screen = Screen::connect("pixel", dir.clone(), 1080, worker).expect("connect");
        let probe = |d: &sim_android::Device| Ok(Some(format!("running: {}", d.running.is_some())));
        assert!(screen.console(probe));
        wait_idle(&screen);
        // The emulator advertises itself (as this process, which is alive).
        std::fs::write(running.join(format!("pid_{}.ini", std::process::id())), "avd.name=pixel\nport.serial=5554\n").expect("advertise");
        assert!(screen.console(probe));
        wait_idle(&screen);
        assert_eq!(outbox.take(), ["running: false", "running: true"]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn plain_keys_go_over_the_display_socket_and_shifted_ones_need_the_console() {
        assert_eq!(display_keys(&[stroke('a'), stroke('b')]), Some(vec!["30".to_owned(), "48".to_owned()]));
        assert_eq!(display_keys(&[stroke('a'), stroke('B')]), None, "shift needs the console");
        assert_eq!(display_keys(&[]), Some(vec![]));
    }

    /// Typing never waits on the console: plain keys tap over the display connection at once,
    /// shifted ones queue on the console worker, and a plain key typed while console work is
    /// queued waits behind it, so keys arrive in the order they were typed.
    #[test]
    fn keystrokes_keep_their_order_across_the_socket_and_the_console() {
        let dir = PathBuf::from("/tmp").join(format!("rs-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn).lines();
            (0..5).map(|_| lines.next().expect("line").expect("utf8")).collect::<Vec<_>>()
        });
        let outbox = Outbox::default();
        let screen = Screen::connect("pixel", dir.clone(), 1080, console(outbox.clone())).expect("connect");
        screen.strokes(vec![stroke('a'), stroke('b')]);
        let (release, gate) = std::sync::mpsc::channel::<()>();
        screen.console(move |_| {
            gate.recv().ok();
            Ok(None)
        });
        // Behind the slow job: neither may reach the socket before it.
        screen.strokes(vec![stroke('c')]);
        screen.strokes(vec![stroke('D')]);
        release.send(()).expect("release");
        let start = Instant::now();
        while outbox.take().is_empty() {
            assert!(start.elapsed() < Duration::from_secs(5), "console jobs ran");
            std::thread::sleep(Duration::from_millis(2));
        }
        while !screen.console.idle() {
            assert!(start.elapsed() < Duration::from_secs(5), "console idle");
            std::thread::sleep(Duration::from_millis(2));
        }
        screen.strokes(vec![stroke('e')]);
        assert_eq!(server.join().expect("server"), ["shm", "hello 1080", "key 30", "key 48", "key 18"]);
        let _ = std::fs::remove_dir_all(dir);
    }

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
        let screen = Screen::connect("pixel", dir.clone(), 1080, console(Outbox::default())).expect("connect");
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
