//! Host file operations that std and rustix don't wrap.
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// Deallocates `len` bytes of `file` at `offset` with `fcntl(F_PUNCHHOLE)`. The range reads back
/// as zeroes and the file's length is unchanged. `offset` and `len` must be multiples of the
/// filesystem block size (4096 on APFS); filesystems without hole support fail with `ENOTSUP`.
#[cfg(target_os = "macos")]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let too_big = |_| io::Error::new(io::ErrorKind::InvalidInput, "punch range exceeds off_t");
    let arg = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: libc::off_t::try_from(offset).map_err(too_big)?,
        fp_length: libc::off_t::try_from(len).map_err(too_big)?,
    };
    loop {
        // SAFETY: F_PUNCHHOLE reads one fpunchhole_t through the pointer, which references a live
        // local for the duration of the call; the descriptor is owned by `file`, which outlives it.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &arg as *const libc::fpunchhole_t) };
        if rc != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Deallocates `len` bytes of `file` at `offset` with `fallocate(PUNCH_HOLE | KEEP_SIZE)`: the
/// range reads back as zeroes and the file's length is unchanged. Filesystems without hole
/// support fail with `EOPNOTSUPP`.
#[cfg(target_os = "linux")]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let too_big = |_| io::Error::new(io::ErrorKind::InvalidInput, "punch range exceeds off_t");
    let offset = libc::off_t::try_from(offset).map_err(too_big)?;
    let len = libc::off_t::try_from(len).map_err(too_big)?;
    loop {
        // SAFETY: plain integer arguments; the descriptor is owned by `file`, which outlives the call.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, offset, len) };
        if rc != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use super::*;

    #[test]
    fn punched_range_reads_zero_and_keeps_length() {
        let path = std::env::temp_dir().join(format!("emu-hvf-punch-{}", std::process::id()));
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("create");
        f.write_all(&vec![0xab; 3 * 4096]).expect("fill");
        let punched = punch_hole(&f, 4096, 4096);
        let mut back = Vec::new();
        f.seek(SeekFrom::Start(0)).expect("seek");
        f.read_to_end(&mut back).expect("read");
        let _ = std::fs::remove_file(&path);
        if let Err(e) = punched {
            // tmpfs and some overlay filesystems have no holes; nothing to check there.
            assert_eq!(e.raw_os_error(), Some(libc::EOPNOTSUPP), "{e}");
            return;
        }
        assert_eq!(back.len(), 3 * 4096, "length unchanged");
        assert!(back[..4096].iter().all(|&b| b == 0xab));
        assert!(back[4096..8192].iter().all(|&b| b == 0), "hole reads as zeroes");
        assert!(back[8192..].iter().all(|&b| b == 0xab));
    }
}
