//! Host file operations that std and rustix don't wrap.
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// Deallocates `len` bytes of `file` at `offset` with `fcntl(F_PUNCHHOLE)`. The range reads back
/// as zeroes and the file's length is unchanged. `offset` and `len` must be multiples of the
/// filesystem block size (4096 on APFS); filesystems without hole support fail with `ENOTSUP`.
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
