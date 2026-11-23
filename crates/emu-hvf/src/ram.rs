//! Guest RAM: an anonymous host mapping addressed by guest physical address.
use crate::{Error, Result};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("guest memory access out of range: {len} bytes at {addr:#x}")]
pub struct MemError {
    pub addr: u64,
    pub len: u64,
}

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
    fn round_trips_and_bounds_checks() {
        let ram = GuestRam::new(0x4000_0000, 0x8000).expect("alloc");
        ram.write(0x4000_0010, &[1, 2, 3, 4]).expect("write");
        assert_eq!(ram.read_u32(0x4000_0010), Ok(0x0403_0201));
        assert!(ram.read_u32(0x3fff_fffe).is_err(), "below base");
        assert!(ram.read_u32(0x4000_7ffe).is_err(), "straddles the end");
        assert!(ram.write(u64::MAX, &[0]).is_err(), "overflow");
    }
}
