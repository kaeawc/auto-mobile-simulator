//! Guest RAM: an anonymous host mapping addressed by guest physical address.
use crate::{Error, Result};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("guest memory access out of range: {len} bytes at {addr:#x}")]
pub struct MemError {
    pub addr: u64,
    pub len: u64,
}

/// Most iovecs one `preadv`/`pwritev` takes (macOS and Linux both define `IOV_MAX` as 1024).
const IOV_MAX: usize = 1024;

/// A host address range backing guest-visible memory, handed to native libraries (the GPU
/// renderer) that read and write guest buffers directly. Constructed only from live mappings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRange {
    pub(crate) ptr: *mut u8,
    pub(crate) len: usize,
}

// SAFETY: plain address + length; whoever dereferences it upholds the mapping's lifetime.
unsafe impl Send for HostRange {}
// SAFETY: as above.
unsafe impl Sync for HostRange {}

impl HostRange {
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Wraps memory returned by a native library (e.g. a renderer's mapped blob).
    ///
    /// # Safety
    /// `ptr..ptr+len` must stay a valid mapping for as long as the range is used.
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> HostRange {
        HostRange { ptr, len }
    }
}

/// A contiguous block of guest RAM.
///
/// The guest reads and writes this memory while device threads do the same through the
/// accessors here, exactly like DMA on real hardware. Accessors copy bytes in and out and never
/// hand out references into the mapping, so a racing guest can only produce stale or torn
/// *values* — which virtio devices must already tolerate — never undefined behaviour in safe
/// callers.
pub struct GuestRam {
    ptr: *mut u8,
    len: u64,
    base: u64,
}

// SAFETY: the mapping is plain bytes with no interior references; all access goes through
// copying accessors (see the type docs).
unsafe impl Send for GuestRam {}
// SAFETY: as above.
unsafe impl Sync for GuestRam {}

impl GuestRam {
    /// Allocates `len` bytes of zeroed RAM that will appear at guest physical address `base`.
    /// Pages are committed lazily by the host as the guest touches them.
    pub fn new(base: u64, len: u64) -> Result<GuestRam> {
        if len == 0 || !len.is_multiple_of(0x4000) || !base.is_multiple_of(0x4000) {
            return Err(Error::Invalid("guest RAM base and size must be non-zero multiples of 16 KiB"));
        }
        // SAFETY: anonymous private mapping with no address hint; the result is checked.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(Error::Mmap(len as usize));
        }
        Ok(GuestRam { ptr: ptr.cast(), len, base })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Guest physical address one past the end.
    pub fn end(&self) -> u64 {
        self.base + self.len
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn host_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// The host address range behind guest `[addr, addr + len)`, for native code that DMAs
    /// into guest buffers. Valid while this `GuestRam` lives.
    pub fn host_range(&self, addr: u64, len: usize) -> Option<HostRange> {
        let off = self.offset(addr, len).ok()?;
        // SAFETY: `off + len` is inside the mapping (checked above).
        Some(HostRange { ptr: unsafe { self.ptr.add(off) }, len })
    }

    fn offset(&self, addr: u64, len: usize) -> std::result::Result<usize, MemError> {
        let err = MemError { addr, len: len as u64 };
        let off = addr.checked_sub(self.base).ok_or(err.clone_err())?;
        let end = off.checked_add(len as u64).ok_or(err.clone_err())?;
        if end > self.len {
            return Err(err);
        }
        Ok(off as usize)
    }

    /// Copies guest memory at `addr` into `buf`.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, buf.len())?;
        // SAFETY: `off + buf.len()` is within the mapping (checked above) and `buf` is a
        // distinct host allocation, so the ranges cannot overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(off), buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    /// Appends guest `[addr, addr + len)` to `out` without zero-filling it first (device paths
    /// copy whole packets and commands out of the guest this way).
    pub fn read_append(&self, addr: u64, len: usize, out: &mut Vec<u8>) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, len)?;
        out.reserve(len);
        let start = out.len();
        // SAFETY: `off + len` is within the mapping (checked above); `reserve` made room for `len`
        // bytes after `start` in `out`'s own allocation, which cannot overlap guest memory, and
        // every one of them is initialized by the copy before `set_len` exposes them.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr.add(off), out.as_mut_ptr().add(start), len);
            out.set_len(start + len);
        }
        Ok(())
    }

    /// Copies `buf` into guest memory at `addr`.
    pub fn write(&self, addr: u64, buf: &[u8]) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, buf.len())?;
        // SAFETY: as in `read`, with the direction reversed.
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), self.ptr.add(off), buf.len()) };
        Ok(())
    }

    /// Reads `len` bytes from `file` at `offset` straight into guest memory at `addr` (no
    /// intermediate buffer). Short reads at end of file zero-fill the rest.
    pub fn read_from_file(&self, addr: u64, len: usize, file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let off = self.offset(addr, len).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut done = 0usize;
        while done < len {
            // SAFETY: the destination range lies inside the mapping (checked above); pread writes
            // at most `len - done` bytes there.
            let n = unsafe {
                libc::pread(file.as_raw_fd(), self.ptr.add(off + done).cast(), len - done, (offset + done as u64) as libc::off_t)
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                // Past end of file: the rest reads as zeroes, like a sparse disk.
                // SAFETY: same range as above.
                unsafe { std::ptr::write_bytes(self.ptr.add(off + done), 0, len - done) };
                break;
            }
            done += n as usize;
        }
        Ok(())
    }

    /// Writes `len` bytes of guest memory at `addr` to `file` at `offset` (no intermediate buffer).
    pub fn write_to_file(&self, addr: u64, len: usize, file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let off = self.offset(addr, len).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut done = 0usize;
        while done < len {
            // SAFETY: the source range lies inside the mapping (checked above).
            let n = unsafe {
                libc::pwrite(file.as_raw_fd(), self.ptr.add(off + done).cast(), len - done, (offset + done as u64) as libc::off_t)
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "pwrite wrote nothing"));
            }
            done += n as usize;
        }
        Ok(())
    }

    /// Reads `file` from `offset` into the guest ranges `segs` (`(addr, len)`, in order) with
    /// as few `preadv` calls as possible: guest-adjacent ranges merge into one iovec and at
    /// most `IOV_MAX` (1024) go in one call. Short reads continue where they stopped; past end of
    /// file the rest reads as zeroes, as in [`GuestRam::read_from_file`]. Every range is
    /// checked before anything is read.
    pub fn read_vectored_from_file(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        self.vectored(segs, file, offset, false)
    }

    /// Writes the guest ranges `segs` (`(addr, len)`, in order) to `file` from `offset` with
    /// as few `pwritev` calls as possible (see [`GuestRam::read_vectored_from_file`]).
    pub fn write_vectored_to_file(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64) -> std::io::Result<()> {
        self.vectored(segs, file, offset, true)
    }

    fn vectored(&self, segs: &[(u64, usize)], file: &std::fs::File, offset: u64, write: bool) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let invalid = |e: MemError| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
        let mut iov: Vec<libc::iovec> = Vec::with_capacity(segs.len().min(IOV_MAX));
        for &(addr, len) in segs {
            let off = self.offset(addr, len).map_err(invalid)?;
            if len == 0 {
                continue;
            }
            // SAFETY: `off + len` is inside the mapping (checked above).
            let base = unsafe { self.ptr.add(off) };
            match iov.last_mut() {
                // SAFETY: `last.iov_base + iov_len` is the end of a range checked to lie inside the mapping.
                Some(last) if unsafe { last.iov_base.cast::<u8>().add(last.iov_len) } == base => last.iov_len += len,
                _ => iov.push(libc::iovec { iov_base: base.cast(), iov_len: len }),
            }
        }
        let fd = file.as_raw_fd();
        let (mut first, mut pos) = (0usize, offset);
        while first < iov.len() {
            let batch = &iov[first..iov.len().min(first + IOV_MAX)];
            // SAFETY: every iovec covers a range inside the mapping (checked above); preadv
            // writes and pwritev reads only within them, and `batch.len() <= IOV_MAX` fits a c_int.
            let n = unsafe {
                if write {
                    libc::pwritev(fd, batch.as_ptr(), batch.len() as libc::c_int, pos as libc::off_t)
                } else {
                    libc::preadv(fd, batch.as_ptr(), batch.len() as libc::c_int, pos as libc::off_t)
                }
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                if write {
                    return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "pwritev wrote nothing"));
                }
                // Past end of file: the rest reads as zeroes, like a sparse disk.
                for v in &iov[first..] {
                    // SAFETY: each iovec lies inside the mapping (checked above).
                    unsafe { std::ptr::write_bytes(v.iov_base.cast::<u8>(), 0, v.iov_len) };
                }
                return Ok(());
            }
            // Skip what was transferred; a partly done iovec continues where it stopped.
            let mut n = n as usize;
            pos += n as u64;
            while n > 0 && first < iov.len() {
                let v = &mut iov[first];
                if n >= v.iov_len {
                    n -= v.iov_len;
                    first += 1;
                } else {
                    // SAFETY: `n < iov_len`, so the advanced base stays inside the same range.
                    v.iov_base = unsafe { v.iov_base.cast::<u8>().add(n) }.cast();
                    v.iov_len -= n;
                    n = 0;
                }
            }
        }
        Ok(())
    }

    /// Fills `len` bytes at `addr` with zeroes.
    pub fn zero(&self, addr: u64, len: usize) -> std::result::Result<(), MemError> {
        let off = self.offset(addr, len)?;
        // SAFETY: range checked above.
        unsafe { std::ptr::write_bytes(self.ptr.add(off), 0, len) };
        Ok(())
    }

    pub fn read_u16(&self, addr: u64) -> std::result::Result<u16, MemError> {
        let mut b = [0; 2];
        self.read(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn read_u32(&self, addr: u64) -> std::result::Result<u32, MemError> {
        let mut b = [0; 4];
        self.read(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn read_u64(&self, addr: u64) -> std::result::Result<u64, MemError> {
        let mut b = [0; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn write_u16(&self, addr: u64, v: u16) -> std::result::Result<(), MemError> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn write_u32(&self, addr: u64, v: u32) -> std::result::Result<(), MemError> {
        self.write(addr, &v.to_le_bytes())
    }
}

impl MemError {
    fn clone_err(&self) -> MemError {
        MemError { addr: self.addr, len: self.len }
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what mmap returned.
        unsafe { libc::munmap(self.ptr.cast(), self.len as usize) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_append_extends_without_touching_what_is_there() {
        let ram = GuestRam::new(0x4000_0000, 0x8000).expect("alloc");
        ram.write(0x4000_0100, b"guest").expect("write");
        let mut out = b"host:".to_vec();
        ram.read_append(0x4000_0100, 5, &mut out).expect("append");
        assert_eq!(out, b"host:guest");
        assert!(ram.read_append(0x4000_7ffe, 4, &mut out).is_err(), "past the end");
        assert_eq!(out, b"host:guest", "unchanged on error");
        ram.read_append(0x4000_0000, 0, &mut out).expect("empty");
    }

    fn tmp_file(name: &str, data: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let path = std::env::temp_dir().join(format!("emu-hvf-ram-{}-{name}", std::process::id()));
        std::fs::write(&path, data).expect("write");
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path).expect("open");
        (path, file)
    }

    #[test]
    fn vectored_reads_scatter_merge_and_zero_fill_past_eof() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x10_0000).expect("alloc");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (path, file) = tmp_file("readv", &data);
        ram.write(B, &vec![0xee; 0x10_0000]).expect("fill");
        // Adjacent pair (merged), a gap, an empty range, then a range running past EOF.
        let segs = [(B + 0x100, 1000), (B + 0x100 + 1000, 24), (B + 0x8000, 0), (B + 0x9000, 3000), (B + 0x20000, 8000)];
        ram.read_vectored_from_file(&segs, &file, 100).expect("readv");
        let mut off = 100usize;
        for &(addr, len) in &segs {
            let mut got = vec![0u8; len];
            ram.read(addr, &mut got).expect("r");
            let want: Vec<u8> = (off..off + len).map(|i| data.get(i).copied().unwrap_or(0)).collect();
            assert_eq!(got, want, "segment at {addr:#x}");
            off += len;
        }
        let mut gap = [0u8; 4];
        ram.read(B + 0x100 + 1024, &mut gap).expect("r");
        assert_eq!(gap, [0xee; 4], "bytes between segments untouched");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn vectored_io_spans_more_than_iov_max_ranges() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x40_0000).expect("alloc");
        // 2500 non-adjacent 16-byte ranges: three preadv/pwritev batches.
        let segs: Vec<(u64, usize)> = (0..2500u64).map(|i| (B + i * 32, 16)).collect();
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 253) as u8).collect();
        let (path, file) = tmp_file("iovmax", &data);
        ram.read_vectored_from_file(&segs, &file, 0).expect("readv");
        for (i, &(addr, _)) in segs.iter().enumerate() {
            let mut got = [0u8; 16];
            ram.read(addr, &mut got).expect("r");
            assert_eq!(&got[..], &data[i * 16..i * 16 + 16], "segment {i}");
        }
        // Write them back shifted by one byte of file offset.
        ram.write_vectored_to_file(&segs, &file, 1).expect("writev");
        let back = std::fs::read(&path).expect("read");
        assert_eq!(&back[1..40_001], &data[..]);
        assert_eq!(back[0], data[0]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn vectored_io_checks_every_range_first() {
        const B: u64 = 0x4000_0000;
        let ram = GuestRam::new(B, 0x8000).expect("alloc");
        let (path, file) = tmp_file("bounds", &[9u8; 64]);
        let segs = [(B, 16), (B + 0x7ff8, 16)];
        assert!(ram.read_vectored_from_file(&segs, &file, 0).is_err());
        assert_eq!(ram.read_u32(B), Ok(0), "nothing read when a later range is bad");
        ram.write(B, &[1u8; 16]).expect("w");
        assert!(ram.write_vectored_to_file(&segs, &file, 0).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), [9u8; 64], "nothing written");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn round_trips_and_bounds_checks() {
        let ram = GuestRam::new(0x4000_0000, 0x8000).expect("alloc");
        ram.write(0x4000_0010, &[1, 2, 3, 4]).expect("write");
        assert_eq!(ram.read_u32(0x4000_0010), Ok(0x0403_0201));
        assert!(ram.read_u32(0x3fff_fffe).is_err(), "below base");
        assert!(ram.read_u32(0x4000_7ffe).is_err(), "straddles the end");
        assert!(ram.write(u64::MAX, &[0]).is_err(), "overflow");
    }
}
