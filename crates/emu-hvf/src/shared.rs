//! A file mapped shared and writable: memory this process writes and other processes read through
//! the file (`pread`), with no copy on this side. The display server's frame ring lives in one.
//! Builds on any unix host.
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

/// `len` bytes of a file, mapped `MAP_SHARED` read-write. Other processes see writes through the
/// page cache as soon as they're made (reads of the file, or their own mappings). The file must
/// not be truncated below `len` while mapped (a process that does so gets `SIGBUS` here); the
/// owner creates it privately (`O_EXCL`, mode 0600) so only the same user could.
pub struct SharedMap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain bytes with no interior references; `&mut self` gates every write
// but the atomic stores, which are safe to share.
unsafe impl Send for SharedMap {}
// SAFETY: as above.
unsafe impl Sync for SharedMap {}

impl SharedMap {
    /// Sets `file`'s length to `len` and maps all of it. The file stays open through the mapping.
    pub fn new(file: &File, len: usize) -> io::Result<SharedMap> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty shared mapping"));
        }
        file.set_len(len as u64)?;
        // SAFETY: a fresh shared mapping of the file's first `len` bytes (the file now has exactly
        // that length) with no address hint; the result is checked before use.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast::<u8>()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(SharedMap { ptr, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The mapped bytes, for writing (a frame read back or downscaled straight into its slot).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr..ptr + len` is the live mapping; `&mut self` makes this the only view in
        // this process. Other processes only read it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: as above, shared; no `&mut` view can coexist with `&self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Stores `value` at `offset` (8-byte aligned, in bounds) with release ordering: every
    /// write made before it is visible to a reader that sees it. A seqlock's sequence number.
    pub fn store_u64(&self, offset: usize, value: u64) {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.len, "u64 at {offset} outside the mapping");
        // SAFETY: in bounds and 8-byte aligned (the mapping is page-aligned); the location is only
        // ever accessed atomically while a `&self` exists, since writes through `as_mut_slice`
        // need `&mut self`.
        unsafe { AtomicU64::from_ptr(self.ptr.as_ptr().add(offset).cast::<u64>()) }.store(value, Ordering::Release);
    }

    /// Loads the u64 at `offset` (8-byte aligned, in bounds) with acquire ordering.
    pub fn load_u64(&self, offset: usize) -> u64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.len, "u64 at {offset} outside the mapping");
        // SAFETY: as in `store_u64`.
        unsafe { AtomicU64::from_ptr(self.ptr.as_ptr().add(offset).cast::<u64>()) }.load(Ordering::Acquire)
    }
}

impl Drop for SharedMap {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what mmap returned, and no slice borrowed from the
        // mapping outlives `self`.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use super::*;

    #[test]
    fn writes_are_visible_through_the_file() {
        let path = std::env::temp_dir().join(format!("emu-hvf-shared-{}", std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("file");
        let mut map = SharedMap::new(&file, 3 * 4096).expect("map");
        assert_eq!((map.len(), map.is_empty()), (3 * 4096, false));
        assert_eq!(file.metadata().expect("meta").len(), 3 * 4096, "sized to the mapping");
        map.as_mut_slice()[5000..5004].copy_from_slice(&[1, 2, 3, 4]);
        map.store_u64(8, 0xdead_beef);
        assert_eq!(map.load_u64(8), 0xdead_beef);
        // Another reader of the file (a client's pread) sees both, with no msync.
        let reader = std::fs::File::open(&path).expect("reopen");
        let mut buf = [0u8; 4];
        reader.read_exact_at(&mut buf, 5000).expect("pread");
        assert_eq!(buf, [1, 2, 3, 4]);
        let mut seq = [0u8; 8];
        reader.read_exact_at(&mut seq, 8).expect("pread seq");
        assert_eq!(u64::from_ne_bytes(seq), 0xdead_beef);
        assert_eq!(&map.as_slice()[5000..5004], &[1, 2, 3, 4]);
        drop(map);
        assert!(SharedMap::new(&file, 0).is_err());
        std::fs::remove_file(path).ok();
    }

    #[test]
    #[should_panic(expected = "outside the mapping")]
    fn atomic_stores_are_bounds_checked() {
        let path = std::env::temp_dir().join(format!("emu-hvf-shared-oob-{}", std::process::id()));
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("file");
        std::fs::remove_file(&path).ok();
        let map = SharedMap::new(&file, 4096).expect("map");
        map.store_u64(4096, 1);
    }
}
