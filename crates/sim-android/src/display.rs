//! A client for a running emulator's display socket (`<avd>/emu-rs/display.sock`, protocol in
//! `sim_core::stream`): input lines out, frames in.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use sim_core::stream::{FrameHeader, HEADER_LEN, Input};

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

    /// A second handle on the same connection (for a reader thread).
    pub fn try_clone(&self) -> Result<UnixStream, String> {
        self.stream.try_clone().map_err(|e| e.to_string())
    }
}

/// Reads one frame: its header and RGBA rows.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<(FrameHeader, Vec<u8>)> {
    let mut hdr = [0u8; HEADER_LEN];
    r.read_exact(&mut hdr)?;
    let header = FrameHeader::decode(&hdr).map_err(std::io::Error::other)?;
    let mut rgba = vec![0u8; header.len as usize];
    r.read_exact(&mut rgba)?;
    Ok((header, rgba))
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
            let got: Vec<String> = (0..2).map(|_| lines.next().expect("line").expect("utf8")).collect();
            let h = FrameHeader { width: 1, height: 1, device_width: 2, device_height: 2, frame: 1, len: 4 };
            conn.write_all(&h.encode()).and_then(|()| conn.write_all(&[1, 2, 3, 4])).expect("frame");
            got
        });
        let mut d = Display::connect(&dir).expect("connect");
        d.send(&Input::Hello { max_width: 720 }).expect("hello");
        d.send(&Input::Touch { x: 5.0, y: 6.0, phase: Phase::Down }).expect("touch");
        let (h, rgba) = read_frame(&mut d.try_clone().expect("clone")).expect("frame");
        assert_eq!((h.device_width, rgba), (2, vec![1, 2, 3, 4]));
        assert_eq!(server.join().expect("server"), vec!["hello 720".to_owned(), "touch 5 6 down".to_owned()]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_missing_socket_says_so() {
        let err = Display::connect(Path::new("/nonexistent-avd")).err().expect("no socket");
        assert!(err.contains("display.sock") && err.contains("is the device running"), "{err}");
    }
}
