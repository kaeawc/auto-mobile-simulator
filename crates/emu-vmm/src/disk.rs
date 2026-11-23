//! Disk images backing virtio-blk devices.
//!
//! - `Persistent`: a raw file the guest reads and writes in place. Sparse on APFS, so an empty
//!   10 GiB userdata costs nothing until written, and guest discards punch holes back into it.
//! - `Ephemeral`: a read-only base (a system image in the SDK) with guest writes kept in
//!   memory for the life of the process — the stock emulator's behaviour for system and vendor
//!   images unless `-writable-system` is given.
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use emu_hvf::GuestRam;

const CHUNK: u64 = 4096;
/// Largest buffer of zeroes written at once where a range can't be punched.
const ZERO_BUF: u64 = 1 << 20;

/// How a disk image is attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskMode {
    /// Guest writes go to the file.
    Persistent,
    /// Guest writes are kept in memory and discarded at exit.
    Ephemeral,
}

/// A disk in a virtio-blk slot of the board.
#[derive(Clone, Debug)]
pub struct DiskSpec {
    pub slot: u32,
    pub path: PathBuf,
    pub mode: DiskMode,
    pub serial: String,
}

pub struct Disk {
    /// fsync on guest FLUSH (see `set_fsync`).
    fsync: bool,
    file: File,
    size: u64,
    /// Granularity `F_PUNCHHOLE` accepts: the host filesystem's block size.
    block: u64,
    /// Cleared once the host filesystem reports it can't punch holes.
    punch: bool,
    overlay: Option<Overlay>,
}

/// An ephemeral disk's in-memory changes on top of its read-only base.
#[derive(Default)]
struct Overlay {
    /// Chunks the guest wrote, keyed by chunk-aligned offset. They take precedence over `zeroed`.
    chunks: BTreeMap<u64, Box<[u8]>>,
    /// Chunk-aligned ranges the guest discarded or zeroed, `start -> end`, disjoint and
    /// non-adjacent: a whole-disk trim costs one entry, not one per chunk.
    zeroed: BTreeMap<u64, u64>,
}

impl Overlay {
    /// End of the zeroed range containing `off`, if any.
    fn zeroed_at(&self, off: u64) -> Option<u64> {
        self.zeroed.range(..=off).next_back().map(|(_, &end)| end).filter(|&end| end > off)
    }

    /// Start of the first zeroed range at or after `off`.
    fn next_zeroed(&self, off: u64) -> Option<u64> {
        self.zeroed.range(off..).next().map(|(&start, _)| start)
    }

    fn touches(&self, offset: u64, len: u64) -> bool {
        self.chunks.range(offset / CHUNK * CHUNK..offset + len).next().is_some()
            || self.zeroed_at(offset).is_some()
            || self.zeroed.range(offset..offset + len).next().is_some()
    }

    /// Marks chunk-aligned `[start, end)` as zeroes, dropping any chunks written there.
    fn add_zeroed(&mut self, start: u64, end: u64) {
        let mut tail = self.chunks.split_off(&start);
        let mut rest = tail.split_off(&end);
        self.chunks.append(&mut rest);
        let (mut s, mut e) = (start, end);
        if let Some((&ps, &pe)) = self.zeroed.range(..s).next_back()
            && pe >= s
        {
            s = ps;
            e = e.max(pe);
        }
        let merged: Vec<u64> = self.zeroed.range(s..=e).map(|(&k, _)| k).collect();
        for k in merged {
            if let Some(ke) = self.zeroed.remove(&k) {
                e = e.max(ke);
            }
        }
        self.zeroed.insert(s, e);
    }

    /// The writable copy of the chunk at `chunk`, created from the base (or zeroes) on first use.
    fn chunk_mut(&mut self, base: &File, chunk: u64) -> io::Result<&mut [u8]> {
        let zeroed = self.zeroed_at(chunk).is_some();
        Ok(match self.chunks.entry(chunk) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let mut data = vec![0u8; CHUNK as usize].into_boxed_slice();
                if !zeroed {
                    read_at(base, &mut data, chunk)?;
                }
                e.insert(data)
            }
        })
    }
}

fn mem_err(e: emu_hvf::MemError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e)
}

impl Disk {
    pub fn persistent(path: &Path) -> io::Result<Disk> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        let block = rustix::fs::fstatvfs(&file).ok().map(|s| s.f_frsize).filter(|b| b.is_power_of_two() && *b >= 512).unwrap_or(CHUNK);
        Ok(Disk { file, size, block, punch: true, overlay: None, fsync: false })
    }

    pub fn ephemeral(path: &Path) -> io::Result<Disk> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Disk { file, size, block: CHUNK, punch: false, overlay: Some(Overlay::default()), fsync: false })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    fn check(&self, offset: u64, len: u64) -> io::Result<()> {
        match offset.checked_add(len) {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "access beyond end of disk")),
        }
    }

    /// Reads `len` bytes at disk `offset` into guest memory at `gpa`.
    pub fn read_to_guest(&self, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        self.check(offset, len)?;
        let ov = match &self.overlay {
            Some(o) if o.touches(offset, len) => o,
            // Fast path: one pread straight into guest memory.
            _ => return mem.read_from_file(gpa, len as usize, &self.file, offset),
        };
        let end = offset + len;
        let mut off = offset;
        while off < end {
            let chunk = off / CHUNK * CHUNK;
            let dst = gpa + (off - offset);
            if let Some(data) = ov.chunks.get(&chunk) {
                let n = (chunk + CHUNK).min(end) - off;
                let i = (off - chunk) as usize;
                mem.write(dst, &data[i..i + n as usize]).map_err(mem_err)?;
                off += n;
                continue;
            }
            // A run of base or zeroed data, up to the next written chunk.
            let stop = ov.chunks.range(chunk + CHUNK..).next().map_or(end, |(&k, _)| k.min(end));
            let n = if let Some(zend) = ov.zeroed_at(off) {
                let n = zend.min(stop) - off;
                mem.zero(dst, n as usize).map_err(mem_err)?;
                n
            } else {
                let n = ov.next_zeroed(off).map_or(stop, |s| s.min(stop)) - off;
                mem.read_from_file(dst, n as usize, &self.file, off)?;
                n
            };
            off += n;
        }
        Ok(())
    }

    /// Writes `len` bytes from guest memory at `gpa` to disk `offset`.
    pub fn write_from_guest(&mut self, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        self.check(offset, len)?;
        let Some(overlay) = self.overlay.as_mut() else {
            return mem.write_to_file(gpa, len as usize, &self.file, offset);
        };
        let mut pos = 0;
        while pos < len {
            let off = offset + pos;
            let chunk = off / CHUNK * CHUNK;
            let in_chunk = (off - chunk) as usize;
            let n = (CHUNK - in_chunk as u64).min(len - pos) as usize;
            let data = overlay.chunk_mut(&self.file, chunk)?;
            mem.read(gpa + pos, &mut data[in_chunk..in_chunk + n]).map_err(mem_err)?;
            pos += n as u64;
        }
        Ok(())
    }

    /// Makes `len` bytes at `offset` read back as zeroes and releases their storage where
    /// possible (virtio-blk DISCARD and WRITE_ZEROES). A persistent image gets a hole punched
    /// over the block-aligned interior and explicit zeroes at unaligned edges; an ephemeral disk
    /// records the range in memory and never touches its base.
    pub fn zero_range(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.check(offset, len)?;
        let end = offset + len;
        if let Some(ov) = self.overlay.as_mut() {
            let first = offset.next_multiple_of(CHUNK);
            let last = end / CHUNK * CHUNK;
            if first < last {
                zero_overlay_bytes(ov, &self.file, offset, first)?;
                zero_overlay_bytes(ov, &self.file, last, end)?;
                ov.add_zeroed(first, last);
            } else {
                zero_overlay_bytes(ov, &self.file, offset, end)?;
            }
            return Ok(());
        }
        let first = offset.next_multiple_of(self.block);
        let last = end / self.block * self.block;
        if !self.punch || first >= last {
            return write_zeroes(&self.file, offset, end);
        }
        write_zeroes(&self.file, offset, first)?;
        write_zeroes(&self.file, last, end)?;
        match emu_hvf::host::punch_hole(&self.file, first, last - first) {
            Ok(()) => Ok(()),
            Err(e) => {
                if [Some(libc::ENOTSUP), Some(libc::EOPNOTSUPP)].contains(&e.raw_os_error()) {
                    self.punch = false;
                }
                write_zeroes(&self.file, first, last)
            }
        }
    }

    /// Whether guest flushes reach the drive (`-disk-cache fsync`). Off by default: writes are
    /// already in the host page cache, which survives an emulator crash; only a host crash or
    /// power loss can lose them.
    pub fn set_fsync(&mut self, on: bool) {
        self.fsync = on;
    }

    pub fn flush(&self) -> io::Result<()> {
        if self.overlay.is_some() || !self.fsync {
            return Ok(());
        }
        // Plain fsync hands dirty pages to the drive. std's sync_data/sync_all use F_FULLFSYNC on
        // macOS, which also drains the drive's cache: it made guest FLUSHes (ext4 commits,
        // fsync-heavy first boot) the dominant cost of booting. Data written before a flush has
        // already reached the host page cache, so an emulator crash loses nothing either way.
        rustix::fs::fsync(&self.file).map_err(io::Error::from)
    }
}

/// Zeroes `[start, end)` of an ephemeral disk chunk by chunk (the partial chunks of a range).
fn zero_overlay_bytes(ov: &mut Overlay, base: &File, start: u64, end: u64) -> io::Result<()> {
    let mut off = start;
    while off < end {
        let chunk = off / CHUNK * CHUNK;
        let n = (chunk + CHUNK).min(end) - off;
        if ov.chunks.contains_key(&chunk) || ov.zeroed_at(chunk).is_none() {
            let i = (off - chunk) as usize;
            ov.chunk_mut(base, chunk)?[i..i + n as usize].fill(0);
        }
        off += n;
    }
    Ok(())
}

/// Writes zeroes over `[start, end)` of `file` in bounded pieces.
fn write_zeroes(file: &File, start: u64, end: u64) -> io::Result<()> {
    if start >= end {
        return Ok(());
    }
    let buf = vec![0u8; (end - start).min(ZERO_BUF) as usize];
    let mut off = start;
    while off < end {
        let n = (end - off).min(ZERO_BUF) as usize;
        file.write_all_at(&buf[..n], off)?;
        off += n as u64;
    }
    Ok(())
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break, // past EOF reads as zeroes
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Creates a sparse raw image of `size` bytes if `path` doesn't exist.
pub fn ensure_sparse(path: &Path, size: u64) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.set_len(size)
}

/// Copies `src` to `dst` if `dst` doesn't exist, as an APFS clone when possible (instant, no
/// extra space until either side is written).
pub fn ensure_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const RAM: u64 = 0x4000_0000;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("emu-disk-{}-{name}", std::process::id()))
    }

    fn read_all(d: &Disk, mem: &GuestRam, len: u64) -> Vec<u8> {
        d.read_to_guest(mem, 0, RAM + 0x10000, len).expect("read");
        let mut got = vec![0u8; len as usize];
        mem.read(RAM + 0x10000, &mut got).expect("r");
        got
    }

    #[test]
    fn ephemeral_keeps_writes_in_memory() {
        let path = tmp("eph");
        std::fs::write(&path, vec![0xaa; 3 * CHUNK as usize]).expect("write");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::ephemeral(&path).expect("open");
        mem.write(RAM, &[1, 2, 3, 4]).expect("w");
        // Straddles chunks 0 and 1.
        d.write_from_guest(&mem, CHUNK - 2, RAM, 4).expect("write");
        let got = read_all(&d, &mem, 3 * CHUNK);
        assert_eq!(&got[CHUNK as usize - 3..CHUNK as usize + 3], &[0xaa, 1, 2, 3, 4, 0xaa]);
        assert!(got[2 * CHUNK as usize..].iter().all(|&b| b == 0xaa));
        assert_eq!(std::fs::read(&path).expect("read")[CHUNK as usize - 2], 0xaa, "base untouched");
        assert!(d.read_to_guest(&mem, 3 * CHUNK - 1, RAM, 2).is_err(), "past the end");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_writes_through() {
        let path = tmp("pers");
        ensure_sparse(&path, 2 * CHUNK).expect("create");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::persistent(&path).expect("open");
        mem.write(RAM, b"data").expect("w");
        d.write_from_guest(&mem, 512, RAM, 4).expect("write");
        d.flush().expect("flush");
        assert_eq!(&std::fs::read(&path).expect("read")[512..516], b"data");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_zero_range_punches_holes() {
        const LEN: u64 = 4 << 20;
        let path = tmp("punch");
        std::fs::write(&path, vec![0x55u8; LEN as usize]).expect("write");
        File::open(&path).and_then(|f| f.sync_all()).expect("sync");
        let before = std::fs::metadata(&path).expect("meta").blocks();
        let mut d = Disk::persistent(&path).expect("open");
        // Unaligned at both ends: 1000 bytes in, ending 3000 bytes past the 3 MiB mark.
        let (start, end) = (1000, (3 << 20) + 3000);
        d.zero_range(start, end - start).expect("zero");
        d.flush().expect("flush");
        let data = std::fs::read(&path).expect("read");
        assert!(data[..start as usize].iter().all(|&b| b == 0x55), "head preserved");
        assert!(data[start as usize..end as usize].iter().all(|&b| b == 0), "range zeroed");
        assert!(data[end as usize..].iter().all(|&b| b == 0x55), "tail preserved");
        assert_eq!(data.len() as u64, LEN, "length unchanged");
        let after = std::fs::metadata(&path).expect("meta").blocks();
        // st_blocks counts 512-byte units; all but the two partial edge blocks are released.
        assert!(before - after >= ((3 << 20) - 2 * 4096) / 512, "blocks {before} -> {after}");
        assert!(d.zero_range(LEN - 10, 11).is_err(), "past the end");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_zero_range_within_one_block() {
        let path = tmp("punch-small");
        std::fs::write(&path, vec![0x55u8; 2 * CHUNK as usize]).expect("write");
        let mut d = Disk::persistent(&path).expect("open");
        d.zero_range(CHUNK - 10, 20).expect("zero");
        let data = std::fs::read(&path).expect("read");
        let z = (CHUNK - 10) as usize;
        assert_eq!(data[z - 1], 0x55);
        assert!(data[z..z + 20].iter().all(|&b| b == 0));
        assert_eq!(data[z + 20], 0x55);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ephemeral_zero_range_shadows_base() {
        let path = tmp("eph-zero");
        std::fs::write(&path, vec![0xaa; 4 * CHUNK as usize]).expect("write");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::ephemeral(&path).expect("open");
        mem.write(RAM, &[7; 8]).expect("w");
        d.write_from_guest(&mem, CHUNK + 100, RAM, 8).expect("write");
        // Tail of chunk 0, all of chunks 1-2, head of chunk 3.
        let (start, end) = (CHUNK - 10, 3 * CHUNK + 20);
        d.zero_range(start, end - start).expect("zero");
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert!(got[..start as usize].iter().all(|&b| b == 0xaa));
        assert!(got[start as usize..end as usize].iter().all(|&b| b == 0));
        assert!(got[end as usize..].iter().all(|&b| b == 0xaa));
        let ov = d.overlay.as_ref().expect("overlay");
        assert_eq!(ov.chunks.keys().copied().collect::<Vec<_>>(), [0, 3 * CHUNK], "full chunks cost nothing");
        assert_eq!(ov.zeroed.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(), [(CHUNK, 3 * CHUNK)]);

        // A write into a zeroed chunk starts from zeroes, not the base.
        d.write_from_guest(&mem, 2 * CHUNK + 5, RAM, 1).expect("write");
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert_eq!(&got[2 * CHUNK as usize + 4..2 * CHUNK as usize + 7], &[0, 7, 0]);

        // Zeroing chunk 3 merges with the existing range; the newer chunk-2 write survives.
        d.zero_range(3 * CHUNK, CHUNK).expect("zero");
        let ov = d.overlay.as_ref().expect("overlay");
        assert_eq!(ov.zeroed.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(), [(CHUNK, 4 * CHUNK)]);
        assert_eq!(ov.chunks.keys().copied().collect::<Vec<_>>(), [0, 2 * CHUNK]);
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert_eq!(got[2 * CHUNK as usize + 5], 7);
        assert!(got[3 * CHUNK as usize..].iter().all(|&b| b == 0));
        assert!(std::fs::read(&path).expect("read").iter().all(|&b| b == 0xaa), "base untouched");
        let _ = std::fs::remove_file(path);
    }
}
