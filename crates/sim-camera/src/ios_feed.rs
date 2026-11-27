//! The iOS camera feed: one simulator's linked camera source, served to the capture shim injected
//! into its apps (`shims/ios-camera`) over a Unix socket.
//!
//! A [`Feed`] listens on a socket path (by default in the device's data directory, see
//! [`socket_path`]) and serves every reader that connects: each says the size and rate it wants,
//! gets its own hub subscription (readers of one source share one capture) and is sent frames as
//! the source produces them. [`Feed::set_source`] relinks the feed: every reader moves to the new
//! source within [`POLL`] plus a frame, without reconnecting.
//!
//! The wire format, little-endian:
//!
//! - reader to feed, once after connecting: a [`Hello`], 20 bytes: `SIMC`, the version (1), and the
//!   width, height and frame rate it wants (0 for the feed's default).
//! - feed to reader, per frame: a [`FrameHeader`], 32 bytes: `SIMF`, width, height, the link's
//!   generation (bumped by every relink), the source's frame number and its time since the capture
//!   started in nanoseconds; then `width * height * 4` bytes of BGRA, top row first.
//!
//! A reader that stops reading for [`WRITE_TIMEOUT`] is dropped; the shim reconnects. A reader
//! that goes away while its source is failing to open or silent is noticed within [`POLL`].
//!
//! Access: the socket is mode 0600 and a fallback path lives in a private (0700, ours) directory,
//! but any process running as the same user can connect, and so read a linked camera without a
//! camera permission of its own.

use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec};
use crate::hub::Hub;
use crate::id::SourceSpec;

/// The environment variable that names the feed's socket to the shim.
pub const SOCKET_ENV: &str = "SIMEE_CAMERA_SOCKET";

/// The protocol version a [`Hello`] carries.
pub const VERSION: u32 = 1;

/// How often a reader's thread checks for a relink or a stop while it waits for a frame.
pub const POLL: Duration = Duration::from_millis(50);

/// How long a reader may stall a frame write before it is dropped.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a new reader has to send its [`Hello`].
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a reader waits before subscribing again after its source failed to open or stopped.
const RETRY: Duration = Duration::from_millis(250);

/// The longest socket path `bind` takes (`sun_path` is 104 bytes on macOS, NUL included).
const MAX_SOCKET_PATH: usize = 103;

/// Where a device's feed listens: `cam.sock` in its data directory, or, when that path is too long
/// for a Unix socket (a long home directory), a name derived from it in a private per-user
/// directory of the host's temp directory (created 0700; refused if it exists and isn't ours and
/// 0700, so another user can't plant the socket).
pub fn socket_path(data_dir: &Path) -> Result<PathBuf> {
    let path = data_dir.join("cam.sock");
    if path.as_os_str().len() <= MAX_SOCKET_PATH {
        return Ok(path);
    }
    let hash = data_dir
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3));
    let user: String =
        std::env::var("USER").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(24).collect();
    let dir = private_dir_in(&std::env::temp_dir(), &format!("simee-cam-{}", if user.is_empty() { "user" } else { &user }))?;
    let path = dir.join(format!("{hash:016x}.sock"));
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(Error::Invalid(format!("camera feed: socket path {} is longer than {MAX_SOCKET_PATH} bytes", path.display())));
    }
    Ok(path)
}

/// `base/name` as a directory only this user can use: created 0700, or, if it exists, accepted
/// only when it is a real directory (not a symlink) owned by this user with mode exactly 0700.
fn private_dir_in(base: &Path, name: &str) -> Result<PathBuf> {
    let dir = base.join(name);
    let io = |what: &str, e: io::Error| Error::Io(format!("camera feed: {what} {}: {e}", dir.display()));
    match DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).map_err(|e| io("securing", e))?,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(&dir).map_err(|e| io("inspecting", e))?;
            let mine = own_uid(base).map_err(|e| io("finding the user for", e))?;
            if !meta.file_type().is_dir() || meta.uid() != mine || meta.mode() & 0o7777 != 0o700 {
                return Err(Error::Invalid(format!(
                    "camera feed: {} exists and is not a private (0700) directory of this user; refusing to use it",
                    dir.display()
                )));
            }
        }
        Err(e) => return Err(io("creating", e)),
    }
    Ok(dir)
}

/// This process's user id, read off a file created (exclusively) in `dir`.
fn own_uid(dir: &Path) -> io::Result<u32> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let probe = dir.join(format!(".simee-uid-{}-{nanos}", std::process::id()));
    let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&probe)?;
    let uid = file.metadata().map(|m| m.uid());
    drop(file);
    let _ = std::fs::remove_file(&probe);
    uid
}

/// The launch environment that gives an app the camera: the shim inserted, and the feed's socket.
pub fn launch_env(shim: &Path, socket: &Path) -> Vec<(String, String)> {
    vec![("DYLD_INSERT_LIBRARIES".into(), shim.display().to_string()), (SOCKET_ENV.into(), socket.display().to_string())]
}

/// What a reader asks for when it connects. Zero fields take the feed's default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Hello {
    pub const LEN: usize = 20;
    const MAGIC: &'static [u8; 4] = b"SIMC";

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(Self::MAGIC);
        b[4..8].copy_from_slice(&VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&self.width.to_le_bytes());
        b[12..16].copy_from_slice(&self.height.to_le_bytes());
        b[16..20].copy_from_slice(&self.fps.to_le_bytes());
        b
    }

    pub fn parse(b: &[u8; Self::LEN]) -> Result<Hello> {
        if &b[0..4] != Self::MAGIC {
            return Err(Error::Invalid("camera feed: not a hello (bad magic)".into()));
        }
        let version = u32_at(b, 4);
        if version != VERSION {
            return Err(Error::Invalid(format!("camera feed: protocol version {version}, this feed speaks {VERSION}")));
        }
        Ok(Hello { width: u32_at(b, 8), height: u32_at(b, 12), fps: u32_at(b, 16) })
    }

    /// The spec this hello asks for: `default`'s fields where it says 0, and `default` itself when
    /// the result is out of range.
    pub fn spec(self, default: FrameSpec) -> FrameSpec {
        let pick = |v: u32, d: u32| if v == 0 { d } else { v };
        FrameSpec::new(pick(self.width, default.width), pick(self.height, default.height), pick(self.fps, default.fps)).unwrap_or(default)
    }
}

/// The header before each frame's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    /// The link's generation: 0 for the source the feed started with, one more per relink.
    pub generation: u32,
    /// The source's frame number ([`crate::Tick::seq`]; the test card draws it).
    pub seq: u64,
    /// The frame's time since its capture started, in nanoseconds.
    pub pts_ns: u64,
}

impl FrameHeader {
    pub const LEN: usize = 32;
    const MAGIC: &'static [u8; 4] = b"SIMF";

    pub fn of(frame: &CameraFrame, generation: u32) -> FrameHeader {
        let pts_ns = u64::try_from(frame.tick.pts.as_nanos()).unwrap_or(u64::MAX);
        FrameHeader { width: frame.width, height: frame.height, generation, seq: frame.tick.seq, pts_ns }
    }

    /// Bytes of pixels after the header.
    pub fn pixel_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn to_bytes(self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(Self::MAGIC);
        b[4..8].copy_from_slice(&self.width.to_le_bytes());
        b[8..12].copy_from_slice(&self.height.to_le_bytes());
        b[12..16].copy_from_slice(&self.generation.to_le_bytes());
        b[16..24].copy_from_slice(&self.seq.to_le_bytes());
        b[24..32].copy_from_slice(&self.pts_ns.to_le_bytes());
        b
    }

    /// A header, checked: the magic, and a size each side of which a frame can have.
    pub fn parse(b: &[u8; Self::LEN]) -> Result<FrameHeader> {
        if &b[0..4] != Self::MAGIC {
            return Err(Error::Invalid("camera feed: not a frame header (bad magic)".into()));
        }
        let h =
            FrameHeader { width: u32_at(b, 4), height: u32_at(b, 8), generation: u32_at(b, 12), seq: u64_at(b, 16), pts_ns: u64_at(b, 24) };
        let side = FrameSpec::MIN_SIDE..=FrameSpec::MAX_SIDE;
        if !side.contains(&h.width) || !side.contains(&h.height) {
            return Err(Error::Invalid(format!("camera feed: frame size {}x{} is out of range", h.width, h.height)));
        }
        Ok(h)
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut v = [0u8; 4];
    v.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(v)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// The source the feed is linked to, and how many times it has been relinked.
struct Link {
    source: SourceSpec,
    generation: u32,
}

struct Shared {
    hub: Arc<Hub>,
    default_spec: FrameSpec,
    link: Mutex<Link>,
    stop: AtomicBool,
    readers: Mutex<Vec<Reader>>,
}

/// A connected reader: a handle on its socket (to cut it off when the feed stops) and its thread.
struct Reader {
    stream: UnixStream,
    thread: JoinHandle<()>,
}

/// A running feed: listening on its socket until dropped.
pub struct Feed {
    shared: Arc<Shared>,
    path: PathBuf,
    accept: Option<JoinHandle<()>>,
}

impl Feed {
    /// Listens on `path` and serves `source` from `hub` to every reader that connects, at the
    /// size and rate each asks for (`default_spec` where it doesn't say). A stale socket file at
    /// `path` (nothing accepting) is replaced; a live one is an error.
    pub fn serve(hub: Arc<Hub>, path: impl Into<PathBuf>, source: SourceSpec, default_spec: FrameSpec) -> Result<Feed> {
        let path = path.into();
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(Error::Invalid(format!("camera feed: socket path {} is longer than {MAX_SOCKET_PATH} bytes", path.display())));
        }
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(Error::Busy(format!("camera feed: {} is already served", path.display())));
            }
            std::fs::remove_file(&path).map_err(|e| Error::Io(format!("camera feed: removing stale {}: {e}", path.display())))?;
        }
        let listener = UnixListener::bind(&path).map_err(|e| Error::Io(format!("camera feed: binding {}: {e}", path.display())))?;
        // Owner-only before anything can connect; the accept loop polls so a stop never depends on the file.
        let secured = std::fs::set_permissions(&path, Permissions::from_mode(0o600));
        if let Err(e) = secured.and_then(|()| listener.set_nonblocking(true)) {
            let _ = std::fs::remove_file(&path);
            return Err(Error::Io(format!("camera feed: securing {}: {e}", path.display())));
        }
        let shared = Arc::new(Shared {
            hub,
            default_spec,
            link: Mutex::new(Link { source, generation: 0 }),
            stop: AtomicBool::new(false),
            readers: Mutex::new(Vec::new()),
        });
        let run = shared.clone();
        let accept = thread::Builder::new()
            .name("camera feed".into())
            .spawn(move || accept_loop(&run, &listener))
            .map_err(|e| Error::Capture(format!("camera feed: can't start the accept thread: {e}")))?;
        Ok(Feed { shared, path, accept: Some(accept) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The source the feed is linked to now.
    pub fn source(&self) -> SourceSpec {
        lock(&self.shared.link).source.clone()
    }

    /// The link's generation: 0 at the start, one more per [`Feed::set_source`].
    pub fn generation(&self) -> u32 {
        lock(&self.shared.link).generation
    }

    /// Relinks the feed: every reader moves to `source` (within [`POLL`] and its first frame).
    /// Returns the new generation, which the frames of `source` carry.
    pub fn set_source(&self, source: SourceSpec) -> u32 {
        let mut link = lock(&self.shared.link);
        link.source = source;
        link.generation = link.generation.wrapping_add(1);
        link.generation
    }

    /// Readers connected now.
    pub fn readers(&self) -> usize {
        let mut readers = lock(&self.shared.readers);
        readers.retain(|r| !r.thread.is_finished());
        readers.len()
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // The accept loop polls `stop`, so this needs neither the socket file nor a connection.
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let readers = std::mem::take(&mut *lock(&self.shared.readers));
        for reader in &readers {
            let _ = reader.stream.shutdown(std::net::Shutdown::Both);
        }
        for reader in readers {
            let _ = reader.thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn accept_loop(shared: &Arc<Shared>, listener: &UnixListener) {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => {
                thread::sleep(POLL);
                continue;
            }
        };
        // Some platforms hand the accepted socket the listener's nonblocking flag.
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        let Ok(handle) = stream.try_clone() else { continue };
        let run = shared.clone();
        let spawned = thread::Builder::new().name("camera feed reader".into()).spawn(move || serve_reader(&run, stream));
        if let Ok(thread) = spawned {
            let mut readers = lock(&shared.readers);
            readers.retain(|r| !r.thread.is_finished());
            readers.push(Reader { stream: handle, thread });
        }
    }
}

/// One reader: its hello, then frames from the linked source until it goes away or the feed stops.
/// Then the socket is shut down, so the reader sees the end although the feed keeps a handle.
fn serve_reader(shared: &Shared, mut stream: UnixStream) {
    serve(shared, &mut stream);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn serve(shared: &Shared, stream: &mut UnixStream) {
    let mut hello = [0u8; Hello::LEN];
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    if stream.read_exact(&mut hello).is_err() {
        return;
    }
    let Ok(hello) = Hello::parse(&hello) else { return };
    let spec = hello.spec(shared.default_spec);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    while !shared.stop.load(Ordering::SeqCst) {
        let (source, generation) = {
            let link = lock(&shared.link);
            (link.source.clone(), link.generation)
        };
        match stream_source(shared, stream, &source, generation, spec) {
            // Relinked: subscribe to the new source.
            Ok(true) => {}
            // The source didn't open, or its capture ended: try again shortly (or on a relink).
            Ok(false) => {
                if !wait_for_retry(shared, stream, generation) {
                    return;
                }
            }
            // The reader went away.
            Err(_) => return,
        }
    }
}

/// Waits [`RETRY`] before the source is opened again, watching the reader meanwhile: false when it
/// went away or the feed stopped, so a reader of a source that won't open doesn't leave a thread
/// behind. A relink ends the wait early.
fn wait_for_retry(shared: &Shared, stream: &UnixStream, seen: u32) -> bool {
    let until = Instant::now() + RETRY;
    while Instant::now() < until {
        if shared.stop.load(Ordering::SeqCst) || peer_gone(stream) {
            return false;
        }
        if lock(&shared.link).generation != seen {
            return true;
        }
        thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
    }
    true
}

/// Whether the reader closed its end: a nonblocking read that finds the end of the stream. (The
/// reader sends nothing after its hello, so any byte it did send is discarded, not a close.)
fn peer_gone(stream: &UnixStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    let gone = match (&*stream).read(&mut byte) {
        Ok(n) => n == 0,
        Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
    };
    let _ = stream.set_nonblocking(false);
    gone
}

/// Streams `source` to the reader until the link changes (`Ok(true)`), the source fails or the
/// feed stops (`Ok(false)`), or a write fails (`Err`).
fn stream_source(shared: &Shared, stream: &mut UnixStream, source: &SourceSpec, generation: u32, spec: FrameSpec) -> io::Result<bool> {
    let Ok(sub) = shared.hub.subscribe(source, spec) else { return Ok(false) };
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if lock(&shared.link).generation != generation {
            return Ok(true);
        }
        match sub.recv_timeout(POLL) {
            Ok(Some(frame)) => write_frame(stream, &frame, generation)?,
            Ok(None) => {
                if peer_gone(stream) {
                    return Err(io::Error::new(ErrorKind::BrokenPipe, "reader went away"));
                }
            }
            Err(_) => return Ok(false),
        }
    }
}

/// Writes one frame: its header, then its pixels.
pub fn write_frame(w: &mut impl Write, frame: &CameraFrame, generation: u32) -> io::Result<()> {
    w.write_all(&FrameHeader::of(frame, generation).to_bytes())?;
    w.write_all(&frame.bgra)
}

/// A reader of a feed, as the shim reads it: for tests, probes and host-side consumers.
pub struct FeedReader {
    stream: UnixStream,
}

impl FeedReader {
    /// Connects to the feed at `path` and asks for `hello`.
    pub fn connect(path: &Path, hello: Hello) -> Result<FeedReader> {
        let mut stream = UnixStream::connect(path).map_err(|e| Error::Io(format!("camera feed: connecting to {}: {e}", path.display())))?;
        stream.write_all(&hello.to_bytes()).map_err(|e| Error::Io(format!("camera feed: hello: {e}")))?;
        Ok(FeedReader { stream })
    }

    /// The next frame, waiting up to `timeout` for it to start.
    pub fn next(&mut self, timeout: Duration) -> Result<(FrameHeader, Vec<u8>)> {
        self.stream.set_read_timeout(Some(timeout)).map_err(|e| Error::Io(e.to_string()))?;
        read_frame(&mut self.stream)
    }
}

/// Reads one frame (header and pixels) from `r`.
pub fn read_frame(r: &mut impl Read) -> Result<(FrameHeader, Vec<u8>)> {
    let mut head = [0u8; FrameHeader::LEN];
    r.read_exact(&mut head).map_err(|e| Error::Io(format!("camera feed: reading a frame header: {e}")))?;
    let header = FrameHeader::parse(&head)?;
    let mut bgra = vec![0u8; header.pixel_len()];
    r.read_exact(&mut bgra).map_err(|e| Error::Io(format!("camera feed: reading {} bytes of pixels: {e}", bgra.len())))?;
    Ok((header, bgra))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::SystemTime;

    use super::*;
    use crate::frame::Tick;
    use crate::source::{Opener, Source};

    /// Frames filled with one byte: the first byte of the source's id after `fake:`, so a reader
    /// can tell which source a frame came from. Counts opens.
    struct Fake {
        opened: Arc<AtomicUsize>,
    }

    struct Fill(u8);

    impl Source for Fill {
        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let bgra = vec![self.0; spec.width as usize * spec.height as usize * 4];
            Ok(CameraFrame { width: spec.width, height: spec.height, bgra: bgra.into(), tick })
        }

        fn synthetic(&self) -> bool {
            true
        }
    }

    impl Opener for Fake {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> Result<Box<dyn Source>> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            match source {
                SourceSpec::Text(t) => Ok(Box::new(Fill(t.bytes().next().unwrap_or(0)))),
                other => Err(Error::Unsupported(format!("fake opener: {other}"))),
            }
        }
    }

    fn text(t: &str) -> SourceSpec {
        SourceSpec::Text(t.into())
    }

    fn sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("simee-feed-{tag}-{}.sock", std::process::id()))
    }

    fn feed(tag: &str, source: SourceSpec, fps: u32) -> (Feed, Arc<AtomicUsize>) {
        let opened = Arc::new(AtomicUsize::new(0));
        let hub = Arc::new(Hub::new(Fake { opened: opened.clone() }));
        let spec = FrameSpec::new(64, 48, fps).unwrap_or_default();
        let feed = Feed::serve(hub, sock(tag), source, spec).unwrap_or_else(|e| panic!("{e}"));
        (feed, opened)
    }

    fn reader(feed: &Feed, hello: Hello) -> FeedReader {
        FeedReader::connect(feed.path(), hello).unwrap_or_else(|e| panic!("{e}"))
    }

    fn next(r: &mut FeedReader) -> (FrameHeader, Vec<u8>) {
        r.next(Duration::from_secs(5)).unwrap_or_else(|e| panic!("{e}"))
    }

    const DEFAULT: Hello = Hello { width: 0, height: 0, fps: 0 };

    #[test]
    fn headers_and_hellos_round_trip() {
        let h = FrameHeader { width: 640, height: 480, generation: 3, seq: 0x1_0000_0002, pts_ns: 33_333_333 };
        assert_eq!(FrameHeader::parse(&h.to_bytes()), Ok(h));
        assert_eq!(h.pixel_len(), 640 * 480 * 4);
        let mut bad = h.to_bytes();
        bad[0] = b'X';
        assert!(FrameHeader::parse(&bad).is_err());
        let huge = FrameHeader { width: 9000, ..h };
        assert!(FrameHeader::parse(&huge.to_bytes()).is_err(), "a size no frame has is refused before allocating");

        let hello = Hello { width: 720, height: 960, fps: 30 };
        assert_eq!(Hello::parse(&hello.to_bytes()), Ok(hello));
        let mut old = hello.to_bytes();
        old[4] = 9;
        assert!(Hello::parse(&old).is_err_and(|e| e.to_string().contains("version 9")));
        // Zero fields take the default; an out-of-range request gets the default whole.
        let d = FrameSpec::default();
        assert_eq!(DEFAULT.spec(d), d);
        assert_eq!(Hello { width: 0, height: 0, fps: 15 }.spec(d), FrameSpec { fps: 15, ..d });
        assert_eq!(Hello { width: 10, height: 10, fps: 30 }.spec(d), d);
    }

    #[test]
    fn frames_are_framed_as_header_then_pixels() {
        let tick = Tick { seq: 7, pts: Duration::from_millis(250), wall: SystemTime::now() };
        let frame = CameraFrame { width: 32, height: 32, bgra: vec![9u8; 32 * 32 * 4].into(), tick };
        let mut wire = Vec::new();
        write_frame(&mut wire, &frame, 2).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(wire.len(), FrameHeader::LEN + 32 * 32 * 4);
        let (h, px) = read_frame(&mut wire.as_slice()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(h, FrameHeader { width: 32, height: 32, generation: 2, seq: 7, pts_ns: 250_000_000 });
        assert_eq!(&*px, &*frame.bgra);
        // A stream cut mid-frame is an error, not a short frame.
        assert!(read_frame(&mut &wire[..wire.len() - 1]).is_err());
    }

    #[test]
    fn a_reader_gets_frames_at_its_size_paced_by_the_source() {
        let (feed, _) = feed("pace", text("a"), 50);
        let mut r = reader(&feed, Hello { width: 40, height: 32, fps: 25 });
        let (first, px) = next(&mut r);
        assert_eq!((first.width, first.height, first.generation), (40, 32, 0));
        assert!(px.iter().all(|&b| b == b'a'));
        let started = Instant::now();
        let mut last = first.seq;
        for _ in 0..10 {
            let (h, _) = next(&mut r);
            assert!(h.seq > last, "frame numbers advance: {} after {last}", h.seq);
            last = h.seq;
        }
        // Ten frames at 25 fps take 400 ms; allow scheduling slack either way.
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(300), "paced by the source, not as fast as possible: {took:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn readers_share_one_capture() {
        let (feed, opened) = feed("many", text("b"), 30);
        let mut a = reader(&feed, DEFAULT);
        let mut b = reader(&feed, DEFAULT);
        let ((ha, pa), (hb, pb)) = (next(&mut a), next(&mut b));
        assert_eq!((ha.width, ha.height), (64, 48), "the feed's default size");
        assert_eq!((hb.width, hb.height), (64, 48));
        assert_eq!(pa, pb);
        assert_eq!(opened.load(Ordering::SeqCst), 1, "one capture for both readers");
        assert_eq!(feed.readers(), 2);
        drop(a);
        // The dropped reader's thread ends at its next write.
        let deadline = Instant::now() + Duration::from_secs(5);
        while feed.readers() != 1 {
            assert!(Instant::now() < deadline, "the dropped reader is still counted");
            thread::sleep(Duration::from_millis(10));
        }
        let _ = next(&mut b);
    }

    #[test]
    fn relinking_moves_every_reader_to_the_new_source() {
        let (feed, _) = feed("relink", text("a"), 30);
        let mut r = reader(&feed, DEFAULT);
        assert_eq!(next(&mut r).0.generation, 0);
        let switched = Instant::now();
        assert_eq!(feed.set_source(text("z")), 1);
        assert_eq!(feed.source(), text("z"));
        loop {
            let (h, px) = next(&mut r);
            if h.generation == 1 {
                assert!(px.iter().all(|&b| b == b'z'), "a new-generation frame is the new source's");
                break;
            }
            assert!(px.iter().all(|&b| b == b'a'), "an old-generation frame is the old source's");
        }
        assert!(switched.elapsed() < Duration::from_secs(1), "relinked in {:?}", switched.elapsed());
    }

    #[test]
    fn a_source_that_fails_to_open_is_retried_until_relinked() {
        let (feed, opened) = feed("retry", SourceSpec::Solid("red".into()), 30);
        let mut r = reader(&feed, DEFAULT);
        assert!(r.next(Duration::from_millis(400)).is_err(), "no frames from a source that doesn't open");
        assert!(opened.load(Ordering::SeqCst) >= 1);
        feed.set_source(text("c"));
        let (h, px) = next(&mut r);
        assert_eq!((h.generation, px[0]), (1, b'c'));
    }

    #[test]
    fn a_bad_hello_is_dropped_and_the_feed_keeps_serving() {
        let (feed, _) = feed("hello", text("d"), 30);
        let mut bad = UnixStream::connect(feed.path()).unwrap_or_else(|e| panic!("{e}"));
        bad.write_all(&[0u8; Hello::LEN]).unwrap_or_else(|e| panic!("{e}"));
        let _ = bad.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 1];
        assert_eq!(bad.read(&mut buf).ok(), Some(0), "closed without a frame");
        let mut good = reader(&feed, DEFAULT);
        assert_eq!(next(&mut good).1[0], b'd');
    }

    #[test]
    fn stopping_removes_the_socket_and_a_stale_one_is_replaced() {
        let path = sock("stale");
        // A leftover socket file nobody accepts on.
        drop(UnixListener::bind(&path));
        assert!(path.exists());
        let hub = Arc::new(Hub::new(Fake { opened: Arc::default() }));
        let feed = Feed::serve(hub.clone(), &path, text("e"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(Feed::serve(hub, &path, text("e"), FrameSpec::default()), Err(Error::Busy(_))), "a live feed isn't replaced");
        let mut r = reader(&feed, DEFAULT);
        let _ = next(&mut r);
        drop(feed);
        assert!(!path.exists());
        assert!(r.next(Duration::from_secs(2)).is_err(), "readers are cut off");
    }

    #[test]
    fn socket_paths_fit_a_unix_socket() {
        let short = Path::new("/Users/a/Library/Developer/CoreSimulator/Devices/0053A2F9-1FD0-4409-BCF1-6D32F934C172/data");
        assert_eq!(socket_path(short).ok(), Some(short.join("cam.sock")));
        let long =
            Path::new("/Users/someone-with-a-long-name/Library/Developer/CoreSimulator/Devices/0053A2F9-1FD0-4409-BCF1-6D32F934C172/data");
        let fallback = socket_path(long).unwrap_or_else(|e| panic!("{e}"));
        assert!(fallback.as_os_str().len() <= MAX_SOCKET_PATH || std::env::temp_dir().as_os_str().len() > 70, "{}", fallback.display());
        assert_ne!(Some(fallback.clone()), socket_path(short).ok());
        assert_eq!(Some(fallback.clone()), socket_path(long).ok(), "stable for one device");
        let env = launch_env(Path::new("/s/libsimee-camera.dylib"), &fallback);
        assert_eq!(env[0], ("DYLD_INSERT_LIBRARIES".into(), "/s/libsimee-camera.dylib".into()));
        assert_eq!(env[1].0, SOCKET_ENV);
    }

    #[test]
    fn a_reader_of_a_source_that_wont_open_is_let_go_when_it_leaves() {
        let (feed, opened) = feed("leaver", SourceSpec::Solid("red".into()), 30);
        let r = reader(&feed, DEFAULT);
        let deadline = Instant::now() + Duration::from_secs(5);
        while opened.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline, "the failing source is retried");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(feed.readers(), 1);
        drop(r);
        while feed.readers() != 0 {
            assert!(Instant::now() < deadline, "the reader's thread outlived its client");
            thread::sleep(Duration::from_millis(10));
        }
        let settled = opened.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(opened.load(Ordering::SeqCst), settled, "no more opens for a reader that left");
    }

    #[test]
    fn dropping_the_feed_does_not_need_its_socket_file() {
        let (feed, _) = feed("unlinked", text("f"), 30);
        let mut r = reader(&feed, DEFAULT);
        let _ = next(&mut r);
        std::fs::remove_file(feed.path()).unwrap_or_else(|e| panic!("{e}"));
        let started = Instant::now();
        drop(feed);
        assert!(started.elapsed() < Duration::from_secs(3), "drop took {:?}", started.elapsed());
    }

    #[test]
    fn the_socket_is_owner_only_and_the_fallback_directory_is_private() {
        let (feed, _) = feed("perms", text("g"), 30);
        let mode = std::fs::metadata(feed.path()).unwrap_or_else(|e| panic!("{e}")).mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(feed);

        let base = std::env::temp_dir().join(format!("simee-feed-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        DirBuilder::new().mode(0o700).create(&base).unwrap_or_else(|e| panic!("{e}"));
        let dir = private_dir_in(&base, "d").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::metadata(&dir).unwrap_or_else(|e| panic!("{e}")).mode() & 0o7777, 0o700);
        assert_eq!(private_dir_in(&base, "d").ok(), Some(dir.clone()), "an existing private directory is reused");
        std::fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap_or_else(|e| panic!("{e}"));
        assert!(private_dir_in(&base, "d").is_err(), "a directory others can enter is refused");
        std::os::unix::fs::symlink(&base, base.join("link")).unwrap_or_else(|e| panic!("{e}"));
        assert!(private_dir_in(&base, "link").is_err(), "a symlink is refused");
        let _ = std::fs::remove_dir_all(&base);
    }
}
