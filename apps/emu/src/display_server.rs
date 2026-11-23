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
        let result = match frame {
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
                msg.extend(Record::Shared { header, slot }.encode());
                out.write_all(&msg)
            }
        };
        if result.is_err() {
            break;
        }
        last = Instant::now();
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
    let mut announce = None;
    let ring = match ring {
        Some(r) if r.fits(header.len) => r,
        _ => {
            // A new size (or the first frame): a new ring, announced with this frame.
            *ring = None;
            let r = ring.insert(Ring::create(dir, u64::from(header.len))?);
            announce = Some(r.path.clone());
            r
        }
    };
    let mut ok = false;
    let slot = if factor == 1 {
        ring.publish(&header, |dst| ok = display.read_into(width, height, frame, dst))
    } else {
        ring.publish(&header, |dst| {
            // The capture may hold a newer frame than `frame` by now: it's at least as new.
            ok = display
                .latest(|cap| (cap.width, cap.height) == (width, height) && (downscale_into(width, height, &cap.rgba, factor, dst), true).1)
                .unwrap_or(false);
        })
    };
    // A slot that wasn't filled is never announced; a client still reading an older frame there
    // sees its sequence number move and skips it.
    Ok(ok.then_some(Out::Shared { header, slot, announce }))
}

/// A frame ring this process writes (layout in `sim_core::stream::ring`): a private file in the
/// AVD's `emu-rs/`, removed when the ring is dropped.
struct Ring {
    path: PathBuf,
    map: SharedMap,
    layout: ring::Layout,
    next: u32,
}

static RING_IDS: AtomicU64 = AtomicU64::new(0);

impl Ring {
    /// A new ring for frames of up to `frame_bytes`, named for this process
    /// (`frames-<pid>-<n>.ring`) so a crashed emulator's rings can be found and removed.
    fn create(dir: &Path, frame_bytes: u64) -> std::io::Result<Ring> {
        let name = format!("frames-{}-{}.ring", std::process::id(), RING_IDS.fetch_add(1, Ordering::Relaxed));
        let path = dir.join(name);
        let layout = ring::Layout::new(RING_SLOTS, frame_bytes);
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
        match SharedMap::new(&file, layout.file_len() as usize) {
            Ok(mut map) => {
                map.as_mut_slice()[..ring::FILE_HEADER_LEN].copy_from_slice(&layout.encode());
                Ok(Ring { path, map, layout, next: 0 })
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
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

    use emu_vmm::display::DisplaySink;
    use sim_android::display::{Display, Frames, read_frame};
    use sim_core::pixels::downscale;

    use super::*;

    /// A frame's pixels, distinct per frame number.
    fn pixels(w: u32, h: u32, frame: u64) -> Vec<u8> {
        (0..w * h * 4).map(|i| (u64::from(i).wrapping_mul(2_654_435_761) >> 7) as u8 ^ frame as u8).collect()
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
            // A short path (a unix socket address is at most 104 bytes on macOS), in memory where
            // there's a tmpfs, so the ring isn't timed against disk writeback.
            let root = ["/dev/shm", "/tmp"].into_iter().map(PathBuf::from).find(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
            let dir = root.join(format!("emu-ds-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("emu-rs")).expect("dir");
            let store = Arc::new(FrameStore::default());
            let reads = Arc::new(AtomicUsize::new(0));
            let (counter, frames) = (reads.clone(), Arc::downgrade(&store));
            store.set_readback(Box::new(move |w, h, buf| {
                counter.fetch_add(1, Ordering::Relaxed);
                let n = frames.upgrade().map_or(0, |s| s.frames());
                buf.copy_from_slice(&pixels(w, h, n));
                true
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let server = start(&dir, store.clone(), Arc::new(Mutex::new(None)), stop.clone());
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
            let mut names: Vec<String> = std::fs::read_dir(self.dir.join("emu-rs"))
                .expect("dir")
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".ring"))
                .collect();
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
    fn input_only_connections_get_no_frames_and_stale_rings_go() {
        let dir = PathBuf::from("/tmp").join(format!("emu-ds-stale-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("emu-rs")).expect("dir");
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

        let rig = Rig::new("input");
        let mut d = Display::connect(&rig.dir).expect("connect");
        d.send(&Input::Key("HOME".into())).expect("key");
        d.send(&Input::Touch { x: 1.0, y: 2.0, phase: Phase::Up }).expect("touch");
        rig.store.posted(4, 4);
        let mut r = d.try_clone().expect("clone");
        r.set_read_timeout(Some(Duration::from_millis(200))).expect("timeout");
        let mut buf = [0u8; 1];
        assert!(std::io::Read::read(&mut r, &mut buf).is_err(), "no frames without hello");
        assert_eq!(rig.reads.load(Ordering::Relaxed), 0);
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    /// Moving one frame's RGBA rows from the emulator to a client, on the socket (as before) and
    /// through the ring: the emulator's time, the client's time, and both, per frame.
    ///     cargo test --release -p emu transport_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn transport_cost() {
        // The ring on disk (macOS has no tmpfs; rings live in the AVD directory) and, where
        // there is one, in memory.
        let roots: Vec<PathBuf> = [std::env::temp_dir(), PathBuf::from("/dev/shm")].into_iter().filter(|d| d.is_dir()).collect();
        for root in roots {
            let dir = root.join(format!("emu-transport-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            println!("ring in {}:", root.display());
            for (w, h) in [(2480u32, 1740u32), (1080, 2400), (826, 580), (360, 800)] {
                let px = pixels(w, h, 1);
                let header = FrameHeader::new(w, h, w, h, 1).expect("size");
                let runs = 11;
                // Socket: header and rows written, read into a fresh Vec.
                let (mut tx, rx) = UnixStream::pair().expect("pair");
                let reader = std::thread::spawn(move || {
                    let mut rx = rx;
                    (0..runs)
                        .map(|_| {
                            let (h, v) = read_frame(&mut rx).expect("frame");
                            (Instant::now(), h.len as usize == v.len())
                        })
                        .collect::<Vec<_>>()
                });
                let mut send = Vec::new();
                let mut starts = Vec::new();
                for _ in 0..runs {
                    std::thread::sleep(Duration::from_millis(20));
                    let t = Instant::now();
                    starts.push(t);
                    tx.write_all(&header.encode()).and_then(|()| tx.write_all(&px)).expect("write");
                    send.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                let done = reader.join().expect("reader");
                let socket_total = median(
                    done.iter()
                        .zip(&starts)
                        .map(|((d, ok), s)| {
                            assert!(ok);
                            d.duration_since(*s).as_secs_f64() * 1000.0
                        })
                        .collect(),
                );
                // Ring: rows copied into a slot (a factor-1 frame; downscaled frames are written there
                // by the downscale itself), a 32-byte notice, the client's pread.
                let mut ring = Ring::create(&dir, u64::from(header.len)).expect("ring");
                let (mut tx, rx) = UnixStream::pair().expect("pair");
                let path = ring.path.as_os_str().as_encoded_bytes().to_vec();
                let mut announce = Record::Ring { path_len: path.len() as u32 }.encode().to_vec();
                announce.extend(&path);
                tx.write_all(&announce).expect("announce");
                let reader = std::thread::spawn(move || {
                    let mut frames = Frames::new(rx);
                    (0..runs)
                        .map(|_| {
                            let t = Instant::now();
                            let (h, v) = frames.next_frame().expect("frame");
                            (Instant::now(), t, h.len as usize == v.len())
                        })
                        .collect::<Vec<_>>()
                });
                let (mut publish, mut starts) = (Vec::new(), Vec::new());
                for n in 0..runs {
                    std::thread::sleep(Duration::from_millis(20));
                    let t = Instant::now();
                    starts.push(t);
                    let header = FrameHeader { frame: n as u64 + 1, ..header };
                    let slot = ring.publish(&header, |dst| dst.copy_from_slice(&px));
                    tx.write_all(&Record::Shared { header, slot }.encode()).expect("notice");
                    publish.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                let done = reader.join().expect("reader");
                let ring_total = median(
                    done.iter()
                        .zip(&starts)
                        .map(|((d, _, ok), s)| {
                            assert!(ok);
                            d.duration_since(*s).as_secs_f64() * 1000.0
                        })
                        .collect(),
                );
                let client = median(
                    done.iter()
                        .map(|(d, _, _)| *d)
                        .zip(starts.iter())
                        .map(|(d, s)| d.duration_since(*s).as_secs_f64() * 1000.0)
                        .collect::<Vec<_>>(),
                ) - median(publish.clone());
                println!(
                    "{w}x{h} ({:.1} MB): socket write {:.2} ms, write->read {socket_total:.2} ms | ring publish+notice {:.2} ms, ->client has it {ring_total:.2} ms (client ~{client:.2} ms)",
                    f64::from(header.len) / 1e6,
                    median(send),
                    median(publish),
                );
            }
            std::fs::remove_dir_all(dir).ok();
        }
    }

    /// Post to client-has-the-pixels, through the display server, for the desktop's request
    /// (`hello 1080`) and a full-size one, on the socket and through the ring. The readback is a
    /// memcpy of the frame (the last step of the renderer's own; its GPU copy isn't modelled).
    ///     cargo test --release -p emu frame_path_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn frame_path_latency() {
        for (w, h) in [(2480u32, 1740u32), (1080, 2400)] {
            for max_width in [1080, 4096] {
                for shared in [false, true] {
                    let rig = Rig::new(&format!("lat{w}{max_width}{shared}"));
                    let src = Arc::new(pixels(w, h, 0));
                    let store = Arc::new(FrameStore::default());
                    let s = src.clone();
                    store.set_readback(Box::new(move |_, _, buf| {
                        buf.copy_from_slice(&s);
                        true
                    }));
                    let server = start(&rig.dir.join("x"), store.clone(), Arc::new(Mutex::new(None)), rig.stop.clone());
                    assert!(server.is_none(), "no such dir");
                    std::fs::create_dir_all(rig.dir.join("y/emu-rs")).expect("dir");
                    let _server = start(&rig.dir.join("y"), store.clone(), Arc::new(Mutex::new(None)), rig.stop.clone()).expect("server");
                    let mut d = Display::connect(&rig.dir.join("y")).expect("connect");
                    if shared {
                        d.request_shared().expect("shm");
                    }
                    d.send(&Input::Hello { max_width }).expect("hello");
                    let mut frames = Frames::new(d.try_clone().expect("clone"));
                    std::thread::sleep(Duration::from_millis(50));
                    let mut lat = Vec::new();
                    for _ in 0..11 {
                        std::thread::sleep(Duration::from_millis(40));
                        let t = Instant::now();
                        store.posted(w, h);
                        let (_, rgba) = frames.next_frame().expect("frame");
                        std::hint::black_box(rgba);
                        lat.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    if std::env::var_os("SAMPLES").is_some() {
                        println!("{lat:.1?}");
                    }
                    println!(
                        "{w}x{h} hello {max_width} {}: post -> client has pixels {:.2} ms (median of 11)",
                        if shared { "ring  " } else { "socket" },
                        median(lat)
                    );
                }
            }
        }
    }
}
