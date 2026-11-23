//! A client for a running emulator's display socket (`<avd>/emu-rs/display.sock`, protocol in
//! `sim_core::stream`): input lines out, frames in. Frames arrive on the socket, or, for a client
//! that asked ([`Display::request_shared`]), in a shared ring file the emulator writes and this
//! side reads with `pread` ([`Frames`] takes either).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use sim_core::stream::{FrameHeader, HEADER_LEN, Input, Record, ring};

/// The display socket of the AVD in `avd_dir`.
pub fn socket(avd_dir: &Path) -> PathBuf {
    avd_dir.join("emu-rs/display.sock")
}

/// One connection to a display socket.
pub struct Display {
    stream: UnixStream,
}

impl Display {
    pub fn connect(avd_dir: &Path) -> Result<Display, String> {
        let path = socket(avd_dir);
        let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e} (is the device running with a display?)", path.display()))?;
        Ok(Display { stream })
    }

    /// Sends one input line.
    pub fn send(&mut self, input: &Input) -> Result<(), String> {
        let mut line = input.to_line();
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    /// Asks for frames through the shared ring rather than the socket. Send it before `hello`;
    /// an emulator that predates the ring ignores it and keeps sending frames on the socket.
    pub fn request_shared(&mut self) -> Result<(), String> {
        self.send(&Input::Shared)
    }

    /// A second handle on the same connection (for a reader thread).
    pub fn try_clone(&self) -> Result<UnixStream, String> {
        self.stream.try_clone().map_err(|e| e.to_string())
    }
}

/// Reads one frame: its header and RGBA rows. Only frames sent on the socket; [`Frames`] also
/// takes them from the shared ring.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<(FrameHeader, Vec<u8>)> {
    let mut hdr = [0u8; HEADER_LEN];
    r.read_exact(&mut hdr)?;
    let header = FrameHeader::decode(&hdr).map_err(std::io::Error::other)?;
    Ok((header, read_exactly(r, header.len as usize)?))
}

/// `len` bytes, read straight into reserved capacity: no zero-fill of a multi-megabyte frame
/// first.
fn read_exactly(r: &mut impl Read, len: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    r.take(len as u64).read_to_end(&mut out)?;
    if out.len() != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(out)
}

/// The frames of one display connection, wherever the server puts their pixels.
pub struct Frames<R> {
    reader: R,
    ring: Option<Ring>,
    /// Frames announced in the ring but overwritten before they could be read.
    pub torn: u64,
}

impl<R: Read> Frames<R> {
    pub fn new(reader: R) -> Frames<R> {
        Frames { reader, ring: None, torn: 0 }
    }

    /// The next whole frame: its header and RGBA rows. Ring announcements are followed, and a
    /// frame the server overwrote before it was read (or that names a ring this side couldn't
    /// open) is skipped for the next one.
    pub fn next_frame(&mut self) -> std::io::Result<(FrameHeader, Vec<u8>)> {
        loop {
            let mut hdr = [0u8; HEADER_LEN];
            self.reader.read_exact(&mut hdr)?;
            match Record::decode(&hdr).map_err(std::io::Error::other)? {
                Record::Frame(header) => return Ok((header, read_exactly(&mut self.reader, header.len as usize)?)),
                Record::Ring { path_len } => {
                    let path = read_exactly(&mut self.reader, path_len as usize)?;
                    // A ring that can't be opened leaves none: its frames are skipped until the
                    // next announcement.
                    self.ring = String::from_utf8(path).ok().and_then(|p| Ring::open(Path::new(&p)).ok());
                }
                Record::Shared { header, slot } => match self.ring.as_ref().map(|r| r.read(slot, &header)) {
                    Some(Ok(Some(rgba))) => return Ok((header, rgba)),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(None)) | None => self.torn += 1,
                },
            }
        }
    }
}

/// A shared frame ring, open for reading.
struct Ring {
    file: File,
    layout: ring::Layout,
}

impl Ring {
    fn open(path: &Path) -> std::io::Result<Ring> {
        let file = File::open(path)?;
        let mut head = [0u8; ring::FILE_HEADER_LEN];
        file.read_exact_at(&mut head, 0)?;
        let layout = ring::Layout::decode(&head).map_err(std::io::Error::other)?;
        Ok(Ring { file, layout })
    }

    /// Slot `slot`'s pixels if it still holds the frame `header` announced, whole; `None` if it
    /// was being (or has been) rewritten.
    fn read(&self, slot: u32, header: &FrameHeader) -> std::io::Result<Option<Vec<u8>>> {
        if slot >= self.layout.slots || u64::from(header.len) > self.layout.capacity {
            return Err(std::io::Error::other(format!("frame ring slot {slot} out of range")));
        }
        let at = self.layout.slot_header_offset(slot);
        let mut raw = [0u8; ring::SLOT_HEADER_LEN];
        self.file.read_exact_at(&mut raw, at)?;
        let before = ring::Slot::decode(&raw).unwrap_or_default();
        if before.len != header.len || !before.read_is_whole(before.seq, header.frame) {
            return Ok(None);
        }
        let mut f = &self.file;
        f.seek(SeekFrom::Start(self.layout.slot_data_offset(slot)))?;
        let rgba = read_exactly(&mut f, header.len as usize)?;
        let mut seq = [0u8; 8];
        self.file.read_exact_at(&mut seq, at)?;
        Ok(before.read_is_whole(u64::from_le_bytes(seq), header.frame).then_some(rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::gesture::Phase;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    #[test]
    fn sends_input_lines_and_reads_frames() {
        // Under /tmp: the socket path must fit a unix socket address (104 bytes on macOS).
        let dir = PathBuf::from("/tmp").join(format!("sa-display-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("emu-rs")).expect("dir");
        let listener = UnixListener::bind(socket(&dir)).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let got: Vec<String> = (0..3).map(|_| lines.next().expect("line").expect("utf8")).collect();
            let h = FrameHeader { width: 1, height: 1, device_width: 2, device_height: 2, frame: 1, len: 4 };
            conn.write_all(&h.encode()).and_then(|()| conn.write_all(&[1, 2, 3, 4])).expect("frame");
            got
        });
        let mut d = Display::connect(&dir).expect("connect");
        d.request_shared().expect("shm");
        d.send(&Input::Hello { max_width: 720 }).expect("hello");
        d.send(&Input::Touch { x: 5.0, y: 6.0, phase: Phase::Down }).expect("touch");
        let (h, rgba) = read_frame(&mut d.try_clone().expect("clone")).expect("frame");
        // A frame cut short is an error, not a short frame.
        let cut = FrameHeader { width: 1, height: 2, device_width: 2, device_height: 2, frame: 2, len: 8 };
        let mut short = std::io::Cursor::new([cut.encode().to_vec(), vec![1; 5]].concat());
        assert_eq!(read_frame(&mut short).map(|_| ()).map_err(|e| e.kind()), Err(std::io::ErrorKind::UnexpectedEof));
        assert_eq!((h.device_width, rgba), (2, vec![1, 2, 3, 4]));
        assert_eq!(server.join().expect("server"), vec!["shm".to_owned(), "hello 720".to_owned(), "touch 5 6 down".to_owned()]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_missing_socket_says_so() {
        let err = Display::connect(Path::new("/nonexistent-avd")).err().expect("no socket");
        assert!(err.contains("display.sock") && err.contains("is the device running"), "{err}");
    }

    /// Writes a ring file the way the server does, with plain file writes.
    fn write_ring(path: &Path, layout: ring::Layout, slots: &[(u32, ring::Slot, &[u8])]) {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).expect("ring");
        file.set_len(layout.file_len()).expect("len");
        file.write_all_at(&layout.encode(), 0).expect("header");
        for (i, slot, px) in slots {
            file.write_all_at(&slot.encode(), layout.slot_header_offset(*i)).expect("slot");
            file.write_all_at(px, layout.slot_data_offset(*i)).expect("pixels");
        }
    }

    #[test]
    fn frames_come_from_the_socket_or_the_ring() {
        let dir = std::env::temp_dir().join(format!("sa-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("frames.ring");
        let layout = ring::Layout::new(3, 8);
        let px = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let slot = |frame, seq| ring::Slot { seq, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
        write_ring(&path, layout, &[(1, slot(5, 2), &px), (2, slot(6, 3), &px), (0, slot(4, 2), &px)]);
        let header = |frame| FrameHeader::new(2, 1, 4, 2, frame).expect("size");
        let p = path.to_str().expect("utf8").as_bytes();
        let mut stream = Vec::new();
        // A shared frame before any ring is announced: skipped.
        stream.extend(Record::Shared { header: header(3), slot: 0 }.encode());
        stream.extend(Record::Ring { path_len: p.len() as u32 }.encode());
        stream.extend(p);
        stream.extend(Record::Shared { header: header(5), slot: 1 }.encode());
        // Slot 2 is mid-write (odd sequence) and slot 0 holds another frame: both skipped.
        stream.extend(Record::Shared { header: header(6), slot: 2 }.encode());
        stream.extend(Record::Shared { header: header(9), slot: 0 }.encode());
        stream.extend(Record::Frame(header(10)).encode());
        stream.extend([9; 8]);
        stream.extend(Record::Shared { header: header(11), slot: 7 }.encode());
        let mut frames = Frames::new(std::io::Cursor::new(stream));
        assert_eq!(frames.next_frame().expect("ring frame"), (header(5), px.to_vec()));
        assert_eq!(frames.next_frame().expect("socket frame"), (header(10), vec![9; 8]));
        assert_eq!(frames.torn, 3);
        let err = frames.next_frame().expect_err("slot out of range");
        assert!(err.to_string().contains("slot 7 out of range"), "{err}");
        assert!(frames.next_frame().is_err(), "end of stream");
        // A ring that can't be opened (gone, or not a ring) leaves its frames skipped.
        let mut stream = Vec::new();
        let gone = dir.join("gone.ring");
        let g = gone.to_str().expect("utf8").as_bytes();
        stream.extend(Record::Ring { path_len: g.len() as u32 }.encode());
        stream.extend(g);
        stream.extend(Record::Shared { header: header(1), slot: 0 }.encode());
        let mut frames = Frames::new(std::io::Cursor::new(stream));
        assert!(frames.next_frame().is_err());
        assert_eq!(frames.torn, 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
