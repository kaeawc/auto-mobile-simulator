//! Live display and input for local front ends (the desktop app) over a unix socket at
//! `<avd>/emu-rs/display.sock`. The protocol (frame records one way, input lines the other) is
//! `sim_core::stream`, shared with the clients that read it. A frame goes out when the guest posted
//! a new one, at most every `MIN_INTERVAL`, read back once however many clients and screenshots
//! want it ([`FrameStore::latest`]).
//!
//! A client that sends `shm` gets its frames through a shared ring: a file in `<avd>/emu-rs/`
//! this process maps, writes each frame into (downscaled straight into its slot), and announces
//! with a 32-byte record; only that record crosses the socket. Other clients get the RGBA rows on
//! the socket, as before.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use emu_hvf::SharedMap;
use emu_vmm::display::FrameStore;
use sim_core::gesture::Phase;
use sim_core::pixels::{downscale_factor, downscale_into, downscaled_size};
use sim_core::stream::{FrameHeader, Input, Record, ring};

const MIN_INTERVAL: Duration = Duration::from_millis(33);
/// Slots per ring: a client reading a frame within two frame intervals never races the writer.
const RING_SLOTS: u32 = 3;

pub struct DisplayServer {
    path: PathBuf,
}

impl Drop for DisplayServer {
    /// Removes the socket and this process's rings: streams still running when the emulator
    /// exits never drop theirs.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent()
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let prefix = ring_prefix();
            for e in entries.flatten() {
                if e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".ring")) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Starts serving; the socket is removed when the returned guard drops.
pub fn start(
    avd_dir: &Path,
    display: Arc<FrameStore>,
    inputs: Arc<Mutex<Option<emu_vmm::Inputs>>>,
    stop: Arc<AtomicBool>,
) -> Option<DisplayServer> {
    let dir = avd_dir.join("emu-rs");
    remove_stale_rings(&dir);
    let path = dir.join("display.sock");
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
                let (d, i, s, dir) = (display.clone(), inputs.clone(), stop.clone(), dir.clone());
                let _ = std::thread::Builder::new().name("display-client".into()).spawn(move || client(conn, d, i, s, dir));
            }
        })
        .ok()?;
    Some(DisplayServer { path })
}

/// What a client asked for.
#[derive(Default)]
struct Wants {
    max_width: AtomicU32,
    shared: AtomicBool,
    done: AtomicBool,
}

fn client(conn: UnixStream, display: Arc<FrameStore>, inputs: Arc<Mutex<Option<emu_vmm::Inputs>>>, stop: Arc<AtomicBool>, dir: PathBuf) {
    let wants = Arc::new(Wants::default());
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
                wants.max_width.store(mw, Ordering::Relaxed);
                if let Some(writer) = writer.take() {
                    let (display, wants, stop, dir) = (display.clone(), wants.clone(), stop.clone(), dir.clone());
                    let _ = std::thread::Builder::new()
                        .name("display-frames".into())
                        .spawn(move || frames(writer, &display, &wants, &stop, &dir));
                }
            }
            (Input::Shared, _) => wants.shared.store(true, Ordering::Relaxed),
            (Input::Socket, _) => wants.shared.store(false, Ordering::Relaxed),
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
    wants.done.store(true, Ordering::Relaxed);
}

/// One frame, ready to send once the capture is released.
enum Out {
    /// RGBA rows in the stream's buffer, for the socket.
    Socket(FrameHeader),
    /// In the ring's `slot`; `announce` when the ring is new.
    Shared { header: FrameHeader, slot: u32, announce: Option<PathBuf> },
}

fn frames(mut out: UnixStream, display: &FrameStore, wants: &Wants, stop: &AtomicBool, dir: &Path) {
    let mut sent = 0u64;
    let mut last = Instant::now() - MIN_INTERVAL;
    // For socket frames: one buffer for the stream's life.
    let mut rows = Vec::new();
    let mut ring: Option<Ring> = None;
    let mut ring_failed = false;
    let mut stats = std::env::var_os("EMU_FRAME_STATS").is_some().then(Stats::default);
    while !wants.done.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        // At most one frame per MIN_INTERVAL; then wait for a new one (woken by the frame, or
        // every 100 ms to notice the client or the emulator going away).
        let since = last.elapsed();
        if since < MIN_INTERVAL {
            std::thread::sleep(MIN_INTERVAL - since);
        }
        let n = display.wait_newer(sent, Duration::from_millis(100));
        let mw = wants.max_width.load(Ordering::Relaxed);
        if mw == 0 || n == sent {
            continue;
        }
        let started = Instant::now();
        let frame = if wants.shared.load(Ordering::Relaxed) && !ring_failed {
            shared_frame(display, mw, n, &mut ring, dir).unwrap_or_else(|e| {
                // No ring: this client gets its frames on the socket from the next one.
                eprintln!("emu: display: no shared frame ring ({e}); sending frames on the socket");
                ring_failed = true;
                None
            })
        } else {
            socket_frame(display, mw, &mut rows)
        };
        let Some(frame) = frame else {
            // Nothing readable: try again after the interval, not in a spin.
            last = Instant::now();
            continue;
        };
        let captured = started.elapsed();
        let result = match &frame {
            Out::Socket(header) => {
                sent = header.frame;
                out.write_all(&header.encode()).and_then(|()| out.write_all(&rows))
            }
            Out::Shared { header, slot, announce } => {
                sent = header.frame;
                let mut msg = Vec::with_capacity(128);
                if let Some(path) = announce {
                    let p = path.as_os_str().as_encoded_bytes();
                    msg.extend(Record::Ring { path_len: p.len() as u32 }.encode());
                    msg.extend_from_slice(p);
                }
                msg.extend(Record::Shared { header: *header, slot: *slot }.encode());
                out.write_all(&msg)
            }
        };
        if result.is_err() {
            break;
        }
        last = Instant::now();
        if let Some(line) = stats.as_mut().and_then(|s| s.add(&frame, captured, last - started - captured)) {
            eprintln!("emu: display: {line}");
        }
    }
}

/// `EMU_FRAME_STATS=1`: each stream reports, every [`Stats::EVERY`] frames, how long getting a
/// frame (readback when it's new, then scaling it into the ring or the socket buffer) and sending
/// it took, for measuring on a running guest.
#[derive(Default)]
struct Stats {
    capture: Vec<f64>,
    send: Vec<f64>,
}

impl Stats {
    const EVERY: usize = 100;

    fn add(&mut self, frame: &Out, capture: Duration, send: Duration) -> Option<String> {
        self.capture.push(capture.as_secs_f64() * 1000.0);
        self.send.push(send.as_secs_f64() * 1000.0);
        if self.capture.len() < Self::EVERY {
            return None;
        }
        let (header, how) = match frame {
            Out::Socket(h) => (h, "socket"),
            Out::Shared { header, .. } => (header, "ring"),
        };
        let summary = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            let s = format!("median {:.2} ms, max {:.2} ms", v[v.len() / 2], v[v.len() - 1]);
            v.clear();
            s
        };
        Some(format!(
            "{} frames of {}x{} (device {}x{}) by {how}: capture+scale {}; send {}",
            Self::EVERY,
            header.width,
            header.height,
            header.device_width,
            header.device_height,
            summary(&mut self.capture),
            summary(&mut self.send)
        ))
    }
}

/// The latest frame, scaled into the socket buffer `rows` while the capture is held (sent after,
/// so a slow client never holds up other streams or screenshots).
fn socket_frame(display: &FrameStore, max_width: u32, rows: &mut Vec<u8>) -> Option<Out> {
    display
        .latest(|cap| {
            let factor = downscale_factor(cap.width, max_width);
            let (w, h) = downscaled_size(cap.width, cap.height, factor);
            let header = FrameHeader::new(w, h, cap.width, cap.height, cap.frame)?;
            rows.resize(header.len as usize, 0);
            downscale_into(cap.width, cap.height, &cap.rgba, factor, rows);
            Some(Out::Socket(header))
        })
        .flatten()
}

/// Frame `frame` written into the next slot of `ring` (created, or replaced by a larger one, as
/// needed, and then announced): downscaled from the shared capture, or, at full size, read back
/// from the renderer straight into the slot. `Ok(None)` when the frame can't be read; `Err` when
/// no ring can be made.
fn shared_frame(display: &FrameStore, max_width: u32, frame: u64, ring: &mut Option<Ring>, dir: &Path) -> std::io::Result<Option<Out>> {
    let (width, height, _) = display.geometry();
    let factor = downscale_factor(width, max_width);
    let (w, h) = downscaled_size(width, height, factor);
    let Some(header) = FrameHeader::new(w, h, width, height, frame) else { return Ok(None) };
    let ring = match ring {
        Some(r) if r.fits(header.len) => r,
        _ => {
            // A new size (or the first frame): a new ring.
            *ring = None;
            ring.insert(Ring::create(dir, u64::from(header.len))?)
        }
    };
    let mut ok = false;
    let slot = if factor == 1 {
        ring.publish(&header, |dst| ok = display.read_into(width, height, frame, dst))
    } else {
        ring.publish(&header, |dst| {
            // The capture may hold a newer frame than `frame` by now: it's at least as new.
            let scaled = display.latest(|cap| {
                let same = (cap.width, cap.height) == (width, height);
                if same {
                    downscale_into(width, height, &cap.rgba, factor, dst);
                }
                same
            });
            ok = scaled == Some(true);
        })
    };
    // A slot that wasn't filled is never announced; a client still reading an older frame there
    // sees its sequence number move and skips it. The ring itself is announced with the first
    // frame that is sent from it, however many tries that takes.
    if !ok {
        return Ok(None);
    }
    let announce = (!ring.announced).then(|| ring.path.clone());
    ring.announced = true;
    Ok(Some(Out::Shared { header, slot, announce }))
}

/// A frame ring this process writes (layout in `sim_core::stream::ring`): a private file in the
/// AVD's `emu-rs/`, removed when the ring is dropped.
struct Ring {
    path: PathBuf,
    map: SharedMap,
    layout: ring::Layout,
    next: u32,
    /// Its path has gone to the client.
    announced: bool,
}

static RING_IDS: AtomicU64 = AtomicU64::new(0);

/// This process's ring names start with this.
fn ring_prefix() -> String {
    format!("frames-{}-", std::process::id())
}

impl Ring {
    /// A new ring for frames of up to `frame_bytes`, named for this process
    /// (`frames-<pid>-<n>.ring`) so a crashed emulator's rings can be found and removed.
    ///
    /// The file is written out in full before it's mapped: its blocks are allocated (a full disk
    /// is an error here, not a `SIGBUS` when a frame lands in a hole) and its pages are resident
    /// before the first frame.
    fn create(dir: &Path, frame_bytes: u64) -> std::io::Result<Ring> {
        let layout = ring::Layout::new(RING_SLOTS, frame_bytes);
        let (path, file) = Ring::open_new(dir)?;
        let ring = Ring::fill_zero(&file, layout.file_len()).and_then(|()| SharedMap::new(&file, layout.file_len() as usize));
        match ring {
            Ok(mut map) => {
                map.as_mut_slice()[..ring::FILE_HEADER_LEN].copy_from_slice(&layout.encode());
                Ok(Ring { path, map, layout, next: 0, announced: false })
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// A fresh private file `frames-<pid>-<n>.ring`, skipping names a previous process with this
    /// PID left behind.
    fn open_new(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
        let mut tries = 0;
        loop {
            let path = dir.join(format!("{}{}.ring", ring_prefix(), RING_IDS.fetch_add(1, Ordering::Relaxed)));
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
                other => return other.map(|f| (path, f)),
            }
        }
    }

    fn fill_zero(file: &std::fs::File, len: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let zeros = vec![0u8; 1 << 20];
        let mut at = 0;
        while at < len {
            let n = (len - at).min(zeros.len() as u64) as usize;
            file.write_all_at(&zeros[..n], at)?;
            at += n as u64;
        }
        Ok(())
    }

    fn fits(&self, len: u32) -> bool {
        u64::from(len) <= self.layout.capacity
    }

    /// Writes the next slot (`fill` gets exactly the frame's bytes) under its seqlock: the
    /// sequence number goes odd before the first byte and even after the last.
    fn publish(&mut self, header: &FrameHeader, fill: impl FnOnce(&mut [u8])) -> u32 {
        let slot = self.next;
        self.next = (slot + 1) % self.layout.slots;
        let at = self.layout.slot_header_offset(slot) as usize;
        let seq = self.map.load_u64(at) | 1;
        self.map.store_u64(at, seq);
        // The odd sequence number is visible before any byte of the new frame.
        std::sync::atomic::fence(Ordering::SeqCst);
        let data = self.layout.slot_data_offset(slot) as usize;
        let bytes = self.map.as_mut_slice();
        fill(&mut bytes[data..data + header.len as usize]);
        let meta = ring::Slot {
            seq,
            frame: header.frame,
            width: header.width,
            height: header.height,
            device_width: header.device_width,
            device_height: header.device_height,
            len: header.len,
        };
        bytes[at + 8..at + ring::SLOT_HEADER_LEN].copy_from_slice(&meta.encode()[8..]);
        // Release: the pixels and the header before the even sequence number.
        self.map.store_u64(at, seq + 1);
        slot
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Removes rings left by emulators that are gone (a crash skips their cleanup).
fn remove_stale_rings(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str().filter(|n| n.ends_with(".ring")) else { continue };
        let Some(pid) = name.strip_prefix("frames-").and_then(|n| n.split('-').next()?.parse::<i32>().ok()) else { continue };
        // Gone only if no such process (another user's live process answers EPERM).
        let gone = rustix::process::Pid::from_raw(pid)
            .is_none_or(|p| rustix::process::test_kill_process(p).is_err_and(|e| e == rustix::io::Errno::SRCH));
        if gone {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use emu_vmm::display::{DisplaySink, Readback};
    use emu_vmm::virtio::input::{EV_KEY, EV_REL, InputEvent, Kind, REL_WHEEL};
    use sim_android::display::{Display, Frames, read_frame};
    use sim_core::pixels::downscale;

    use super::*;

    /// A frame's pixels, distinct per frame number.
    fn pixels(w: u32, h: u32, frame: u64) -> Vec<u8> {
        (0..w * h * 4).map(|i| (u64::from(i).wrapping_mul(2_654_435_761) >> 7) as u8 ^ frame as u8).collect()
    }

    /// A short directory (a unix socket address is at most 104 bytes on macOS), in memory where
    /// there's a tmpfs, so rings aren't timed against disk writeback.
    fn scratch(tag: &str) -> PathBuf {
        let root = ["/dev/shm", "/tmp"].into_iter().map(PathBuf::from).find(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!("emu-ds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("emu-rs")).expect("dir");
        dir
    }

    struct Rig {
        dir: PathBuf,
        store: Arc<FrameStore>,
        reads: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        server: Option<DisplayServer>,
    }

    impl Rig {
        /// A display server over a store whose readback produces [`pixels`] for the latest frame.
        fn new(tag: &str) -> Rig {
            Rig::with(tag, None, None)
        }

        /// With these input devices, and this readback instead of [`pixels`].
        fn with(tag: &str, inputs: Option<emu_vmm::Inputs>, readback: Option<Readback>) -> Rig {
            let dir = scratch(tag);
            let store = Arc::new(FrameStore::default());
            let reads = Arc::new(AtomicUsize::new(0));
            let (counter, frames) = (reads.clone(), Arc::downgrade(&store));
            store.set_readback(readback.unwrap_or_else(|| {
                Box::new(move |w, h, buf| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    let n = frames.upgrade().map_or(0, |s| s.frames());
                    buf.copy_from_slice(&pixels(w, h, n));
                    true
                })
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let server = start(&dir, store.clone(), Arc::new(Mutex::new(inputs)), stop.clone());
            assert!(server.is_some(), "serving");
            Rig { dir, store, reads, stop, server }
        }

        fn connect(&self, shared: bool, max_width: u32) -> (Display, UnixStream) {
            let mut d = Display::connect(&self.dir).expect("connect");
            if shared {
                d.request_shared().expect("shm");
            }
            d.send(&Input::Hello { max_width }).expect("hello");
            let r = d.try_clone().expect("clone");
            r.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            (d, r)
        }

        fn rings(&self) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(self.dir.join("emu-rs")) else { return Vec::new() };
            let mut names: Vec<String> =
                entries.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".ring")).collect();
            names.sort();
            names
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            drop(self.server.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn shared_frames_arrive_through_the_ring_and_follow_size_changes() {
        let rig = Rig::new("ring");
        let (_d, reader) = rig.connect(true, 720);
        let mut frames = Frames::new(reader);
        rig.store.posted(1440, 200);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, h.device_width, h.device_height, h.frame), (720, 100, 1440, 200, 1));
        assert_eq!(rgba, downscale(1440, 200, &pixels(1440, 200, 1), 2).2, "downscaled by 2 into the ring");
        let first = rig.rings();
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].starts_with(&format!("frames-{}-", std::process::id())), "{first:?}");
        // The same size reuses the ring, going round its slots.
        for n in 2..=4u64 {
            rig.store.posted(1440, 200);
            let (h, rgba) = frames.next_frame().expect("frame");
            assert_eq!(h.frame, n);
            assert_eq!(rgba, downscale(1440, 200, &pixels(1440, 200, n), 2).2);
        }
        assert_eq!(rig.rings(), first);
        // A larger frame needs a larger ring: a new one is announced and the old one removed.
        rig.store.posted(700, 1600);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, h.frame), (700, 1600, 5), "no downscale under the width");
        assert_eq!(rgba, pixels(700, 1600, 5));
        let second = rig.rings();
        assert_eq!(second.len(), 1);
        assert_ne!(second, first);
        assert_eq!(frames.torn, 0);
        // The ring goes when the client does.
        drop((frames, _d));
        wait_until("the ring's removal", || rig.rings().is_empty());
    }

    #[test]
    fn socket_clients_get_the_same_frames_and_share_one_readback() {
        let rig = Rig::new("sock");
        // An old client: frames on the socket, read with the header-only decoder.
        let (_old, mut old) = rig.connect(false, 720);
        let (_new, shared) = rig.connect(true, 720);
        let mut shared = Frames::new(shared);
        std::thread::sleep(Duration::from_millis(50));
        rig.store.posted(1440, 200);
        let (h_old, px_old) = read_frame(&mut old).expect("socket frame");
        let (h_new, px_new) = shared.next_frame().expect("ring frame");
        assert_eq!((h_old, &px_old), (h_new, &px_new));
        // A screenshot of the same frame reads nothing back either.
        assert_eq!(rig.store.latest(|c| c.frame), Some(1));
        assert_eq!(rig.reads.load(Ordering::Relaxed), 1, "two streams and a capture, one readback");
        assert!(rig.rings().len() == 1, "only the shared client has a ring");
    }

    #[test]
    fn a_ring_is_announced_with_the_first_frame_sent_from_it() {
        // The first readback fails after the ring is made: the announcement waits for a frame.
        let tries = Arc::new(AtomicUsize::new(0));
        let t = tries.clone();
        let rig = Rig::with(
            "announce",
            None,
            Some(Box::new(move |w, h, buf| {
                if t.fetch_add(1, Ordering::Relaxed) == 0 {
                    return false;
                }
                buf.copy_from_slice(&pixels(w, h, 9));
                true
            })),
        );
        let (_d, reader) = rig.connect(true, 4096);
        let mut frames = Frames::new(reader);
        std::thread::sleep(Duration::from_millis(50));
        rig.store.posted(5, 3);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, rgba), (5, 3, pixels(5, 3, 9)));
        assert!(tries.load(Ordering::Relaxed) >= 2, "the first try failed");
        assert_eq!(frames.torn, 0);
    }

    #[test]
    fn rings_skip_taken_names_and_the_server_removes_its_own() {
        let rig = Rig::new("names");
        let dir = rig.dir.join("emu-rs");
        // Names a previous process with this PID left behind.
        let next = RING_IDS.load(Ordering::Relaxed);
        for id in next..next + 3 {
            std::fs::write(dir.join(format!("{}{id}.ring", ring_prefix())), b"old").expect("write");
        }
        let ring = Ring::create(&dir, 100).expect("ring");
        assert!(std::fs::read(&ring.path).expect("ring").len() as u64 == ring.layout.file_len(), "written out in full");
        let other = dir.join("frames-1-0.ring");
        std::fs::write(&other, b"").expect("write");
        std::mem::forget(ring); // a stream still running at exit never drops its ring
        let mut rig = rig;
        drop(rig.server.take());
        assert!(rig.rings() == ["frames-1-0.ring"], "{:?}", rig.rings());
    }

    #[test]
    fn a_client_can_go_back_to_socket_frames() {
        let rig = Rig::new("back");
        let (mut d, mut reader) = rig.connect(true, 4096);
        d.send(&Input::Socket).expect("socket");
        std::thread::sleep(Duration::from_millis(50));
        rig.store.posted(3, 2);
        // The header-only decoder: an EMUR or EMUS here would be an error.
        let (h, rgba) = read_frame(&mut reader).expect("socket frame");
        assert_eq!((h.width, rgba), (3, pixels(3, 2, 1)));
        assert!(rig.rings().is_empty());
    }

    #[test]
    fn without_a_ring_shared_clients_get_frames_on_the_socket() {
        let rig = Rig::new("noring");
        let (_d, reader) = rig.connect(true, 4096);
        let mut frames = Frames::new(reader);
        std::thread::sleep(Duration::from_millis(50));
        // Nowhere to make a ring: the stream says so and falls back to the socket.
        std::fs::remove_dir_all(rig.dir.join("emu-rs")).expect("rm");
        rig.store.posted(3, 2);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, h.frame, rgba), (3, 2, 1, pixels(3, 2, 1)));
        assert!(rig.rings().is_empty());
    }

    #[test]
    fn input_lines_reach_the_devices() {
        let (touch, th) = emu_vmm::virtio::input::Input::new(Kind::Touch { width: 720, height: 1280 }, "touch");
        let (keyboard, kh) = emu_vmm::virtio::input::Input::new(Kind::Keyboard, "keyboard");
        let (rotary, rh) = emu_vmm::virtio::input::Input::new(Kind::Rotary, "rotary");
        let inputs = emu_vmm::Inputs { screen: (720, 1280), touch: Some(th), keyboard: kh, rotary: rh, mouse: None, touchpad: None };
        let rig = Rig::with("input", Some(inputs), None);
        let mut d = Display::connect(&rig.dir).expect("connect");
        for input in [
            Input::Touch { x: 1.0, y: 2.0, phase: Phase::Up }, // no finger down: dropped
            Input::Touch { x: 10.0, y: 20.0, phase: Phase::Down },
            Input::Touch { x: 11.0, y: 21.0, phase: Phase::Move },
            Input::Touch { x: 12.0, y: 22.0, phase: Phase::Up },
            Input::Key("HOME".into()),
            Input::Key("NOT_A_KEY".into()),
            Input::Wheel(2),
        ] {
            d.send(&input).expect("send");
        }
        // Lines are handled in order: once the wheel is in, everything before it is too.
        let start = Instant::now();
        let wheel = loop {
            let got = rotary.take_queued();
            if !got.is_empty() {
                break got;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no wheel");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(wheel, [vec![InputEvent::new(EV_REL, REL_WHEEL, 30), InputEvent::syn()]], "2 clicks, 15 units each");
        assert_eq!(touch.take_queued().len(), 3, "down, move, up; the stray up dropped");
        let home = sim_core::goldfish::keycode("HOME").expect("HOME");
        let keys: Vec<InputEvent> = keyboard.take_queued().into_iter().flatten().collect();
        assert!(keys.iter().any(|e| e.typ == EV_KEY && e.code == home), "{keys:?}");
        assert!(keys.iter().all(|e| e.typ != EV_KEY || e.code == home), "the unknown key sent nothing: {keys:?}");
        // No frames without hello.
        let mut r = d.try_clone().expect("clone");
        r.set_read_timeout(Some(Duration::from_millis(100))).expect("timeout");
        rig.store.posted(4, 4);
        assert!(std::io::Read::read(&mut r, &mut [0u8; 1]).is_err());
        assert_eq!(rig.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn frame_stats_report_every_hundred_frames() {
        let mut stats = Stats::default();
        let header = FrameHeader::new(2, 1, 4, 2, 1).expect("size");
        for i in 1..Stats::EVERY {
            assert_eq!(stats.add(&Out::Socket(header), Duration::from_millis(i as u64), Duration::ZERO), None);
        }
        let line = stats.add(&Out::Shared { header, slot: 0, announce: None }, Duration::from_millis(100), Duration::ZERO).expect("report");
        assert_eq!(
            line,
            "100 frames of 2x1 (device 4x2) by ring: capture+scale median 51.00 ms, max 100.00 ms; send median 0.00 ms, max 0.00 ms"
        );
        assert!(stats.capture.is_empty() && stats.send.is_empty(), "starts over");
    }

    #[test]
    fn stale_rings_go() {
        let dir = scratch("stale");
        // PIDs never reach this (Linux caps them at 2^22, macOS at 99999); our own ring stays.
        let mine = format!("frames-{}-0.ring", std::process::id());
        for name in ["frames-2147483646-0.ring", mine.as_str(), "frames-x-1.ring", "frames-2147483646-0.png"] {
            std::fs::write(dir.join("emu-rs").join(name), b"").expect("write");
        }
        remove_stale_rings(&dir.join("emu-rs"));
        let mut left: Vec<String> =
            std::fs::read_dir(dir.join("emu-rs")).expect("dir").flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        left.sort();
        assert_eq!(left, ["frames-2147483646-0.png", mine.as_str(), "frames-x-1.ring"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    /// Moves `runs` frames of each size from this side to a client, on the socket (as before)
    /// and through a ring in each of `roots`, `pause` apart; one line per root and size.
    fn transport_bench(roots: &[PathBuf], sizes: &[(u32, u32)], runs: usize, pause: Duration) -> Vec<String> {
        let mut lines = Vec::new();
        for root in roots {
            let dir = root.join(format!("emu-transport-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            for &(w, h) in sizes {
                let px = pixels(w, h, 1);
                let header = FrameHeader::new(w, h, w, h, 1).expect("size");
                // Socket: header and rows written, read into a fresh Vec.
                let (mut tx, mut rx) = UnixStream::pair().expect("pair");
                let reader = std::thread::spawn(move || {
                    (0..runs).map(|_| (read_frame(&mut rx).expect("frame").1.len(), Instant::now())).collect::<Vec<_>>()
                });
                let (mut write, mut starts) = (Vec::new(), Vec::new());
                for _ in 0..runs {
                    std::thread::sleep(pause);
                    let t = Instant::now();
                    starts.push(t);
                    tx.write_all(&header.encode()).and_then(|()| tx.write_all(&px)).expect("write");
                    write.push(ms(t.elapsed()));
                }
                let got = reader.join().expect("reader");
                assert!(got.iter().all(|(n, _)| *n == px.len()));
                let socket = median(got.iter().zip(&starts).map(|((_, d), s)| ms(d.duration_since(*s))).collect());
                // Ring: rows copied into a slot (what a full-size readback writes there; a
                // downscale writes its output there), a 32-byte notice, the client's pread.
                let mut ring = Ring::create(&dir, u64::from(header.len)).expect("ring");
                let (mut tx, rx) = UnixStream::pair().expect("pair");
                let path = ring.path.as_os_str().as_encoded_bytes().to_vec();
                tx.write_all(&[Record::Ring { path_len: path.len() as u32 }.encode().as_slice(), &path].concat()).expect("announce");
                let reader = std::thread::spawn(move || {
                    let mut frames = Frames::new(rx);
                    (0..runs).map(|_| (frames.next_frame().expect("frame").1.len(), Instant::now())).collect::<Vec<_>>()
                });
                let (mut publish, mut starts) = (Vec::new(), Vec::new());
                for n in 0..runs {
                    std::thread::sleep(pause);
                    let t = Instant::now();
                    starts.push(t);
                    let header = FrameHeader { frame: n as u64 + 1, ..header };
                    let slot = ring.publish(&header, |dst| dst.copy_from_slice(&px));
                    tx.write_all(&Record::Shared { header, slot }.encode()).expect("notice");
                    publish.push(ms(t.elapsed()));
                }
                let got = reader.join().expect("reader");
                assert!(got.iter().all(|(n, _)| *n == px.len()));
                let ring_total = median(got.iter().zip(&starts).map(|((_, d), s)| ms(d.duration_since(*s))).collect());
                lines.push(format!(
                    "ring in {}: {w}x{h} ({:.1} MB): socket write {:.2} ms, write->read {socket:.2} ms | ring copy+notice {:.2} ms, ->client has it {ring_total:.2} ms",
                    root.display(),
                    f64::from(header.len) / 1e6,
                    median(write),
                    median(publish),
                ));
            }
            std::fs::remove_dir_all(dir).ok();
        }
        lines
    }

    /// Post to client-has-the-pixels through the display server, `runs` frames `pause` apart, for
    /// each (frame width, frame height, max width, shared) case. The readback is a memcpy of the
    /// frame (the last step of the renderer's own; its GPU copy isn't modelled). One line each,
    /// with every sample when `samples`.
    fn latency_bench(cases: &[(u32, u32, u32, bool)], runs: usize, pause: Duration, samples: bool) -> Vec<String> {
        let mut lines = Vec::new();
        for &(w, h, max_width, shared) in cases {
            let src = pixels(w, h, 0);
            let rig = Rig::with(
                &format!("lat{w}{max_width}{shared}"),
                None,
                Some(Box::new(move |_, _, buf| {
                    buf.copy_from_slice(&src);
                    true
                })),
            );
            let (_d, reader) = rig.connect(shared, max_width);
            let mut frames = Frames::new(reader);
            std::thread::sleep(Duration::from_millis(50));
            let mut lat = Vec::new();
            for _ in 0..runs {
                std::thread::sleep(pause);
                let t = Instant::now();
                rig.store.posted(w, h);
                std::hint::black_box(frames.next_frame().expect("frame"));
                lat.push(ms(t.elapsed()));
            }
            let all = if samples { format!(" {lat:.1?}") } else { String::new() };
            let way = if shared { "ring  " } else { "socket" };
            lines.push(format!("{w}x{h} hello {max_width} {way}: post -> client has pixels {:.2} ms (median of {runs}){all}", median(lat)));
        }
        lines
    }

    #[test]
    fn the_benchmarks_run() {
        let lines = transport_bench(&[scratch("bench")], &[(8, 4)], 3, Duration::ZERO);
        assert!(lines.len() == 1 && lines[0].contains("8x4"), "{lines:?}");
        let lines = latency_bench(&[(16, 8, 8, true), (16, 8, 8, false)], 3, MIN_INTERVAL, true);
        assert!(lines.len() == 2 && lines.iter().all(|l| l.contains("median of 3")), "{lines:?}");
    }

    /// Moving one frame's RGBA rows from the emulator to a client, on the socket and through the
    /// ring, on disk and (where there's one) on a tmpfs.
    ///     cargo test --release -p emu transport_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn transport_cost() {
        let roots: Vec<PathBuf> = [std::env::temp_dir(), PathBuf::from("/dev/shm")].into_iter().filter(|d| d.is_dir()).collect();
        for l in transport_bench(&roots, &[(2480, 1740), (1080, 2400), (826, 580), (360, 800)], 11, Duration::from_millis(20)) {
            println!("{l}");
        }
    }

    /// Post to client-has-the-pixels for the desktop's request (`hello 1080`) and a full-size
    /// one, on the socket and through the ring. `CASE=<w>,<max_width>,<shared>` runs one case
    /// alone (a long run of many rigs drifts); `SAMPLES=1` prints every sample (each ring's first
    /// three frames include its slots' first page faults).
    ///     cargo test --release -p emu frame_path_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn frame_path_latency() {
        let case = std::env::var("CASE").ok();
        let mut cases = Vec::new();
        for (w, h) in [(2480u32, 1740u32), (1080, 2400)] {
            for max_width in [1080, 4096] {
                for shared in [false, true] {
                    if case.as_ref().is_none_or(|c| *c == format!("{w},{max_width},{shared}")) {
                        cases.push((w, h, max_width, shared));
                    }
                }
            }
        }
        for l in latency_bench(&cases, 11, Duration::from_millis(40), std::env::var_os("SAMPLES").is_some()) {
            println!("{l}");
        }
    }
}
