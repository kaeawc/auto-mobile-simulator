//! Copy-on-write file clones (#203): the one way emu-avd copies a disk image.
//!
//! Every disk an AVD is made from (a golden's user data, cache and key, `-clone-avd`'s copies of
//! a stopped AVD's, `boot.prop`) goes through [`clone_file`]: `clonefile(2)` on APFS and a
//! `FICLONE` reflink on Linux file systems that have them (Btrfs, XFS, bcachefs), so a new device
//! takes milliseconds and no disk until either side writes. Elsewhere (HFS+, ext4, another
//! volume: `ENOTSUP`, `EOPNOTSUPP`, `EXDEV`) it copies only the source's data extents, so a
//! sparse image (a user data is mostly holes) stays sparse.
//!
//! Metadata, whichever way the file was made: the source's permission bits and extended
//! attributes (what `clonefile` copies, and what the `std::fs::copy` this replaces copied on
//! macOS). An attribute the destination refuses (a `security.*` one without privilege) is
//! skipped, and so is an attribute list that keeps growing while it is read. Ownership and
//! timestamps are the new file's own.
//!
//! `clonefile` also carries ACLs and BSD file flags, but the sparse fallback does not: the
//! `fcopyfile` the old `std::fs::copy` used on macOS copied them, this does not. Nothing needs
//! them on an AVD's disk images (plain files only the emulator opens), so they are dropped.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::{Error, Result};

/// What a data copy is reported as: it keeps holes where the platform can find them.
const COPY_KIND: CloneKind = if cfg!(any(target_vendor = "apple", target_os = "linux")) { CloneKind::Sparse } else { CloneKind::Copy };

/// How a file was copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneKind {
    /// A copy-on-write clone (`clonefile` on APFS, `FICLONE` on Linux): no data copied, blocks
    /// shared until either side writes.
    Reflink,
    /// Only the data extents copied: the file stays as sparse as the original.
    Sparse,
    /// The whole file copied, holes written out: on platforms with neither clones nor
    /// `SEEK_DATA` (neither macOS nor Linux). `golden` ranks it between the other two.
    Copy,
}

impl CloneKind {
    pub fn label(self) -> &'static str {
        match self {
            CloneKind::Reflink => "reflink",
            CloneKind::Sparse => "sparse copy",
            CloneKind::Copy => "copy",
        }
    }
}

/// Copies `from` to the new file `to`: a copy-on-write clone where the file system has them,
/// else a copy that keeps the file's holes.
///
/// `to` must not exist. A failed clone leaves no half-written file behind, and never removes a
/// `to` this call did not create (one another process made in the meantime).
pub fn clone_file(from: &Path, to: &Path) -> Result<CloneKind> {
    clone_with(from, to, reflink, copy_data)
}

/// The clone attempt: makes `to` from `src`, or fails leaving no file this attempt made at `to`.
type Reflink = fn(&File, &Path) -> io::Result<()>;
/// The data copy into the empty `dst` (`src` is `from`, `dst` is `to`).
type CopyData = fn(&File, &File, &Path, &Path) -> Result<()>;

/// [`clone_file`] with the clone attempt and the data copy given, so tests can make them fail
/// as an unsupported file system or a full disk does.
fn clone_with(from: &Path, to: &Path, reflink: Reflink, copy: CopyData) -> Result<CloneKind> {
    if to.symlink_metadata().is_ok() {
        return Err(Error::Io(to.to_path_buf(), io::ErrorKind::AlreadyExists.into()));
    }
    clone_new_file(from, to, reflink, copy)
}

/// A file this call made at `to`, removed again unless [`Created::keep`] is called. It exists
/// only once the clone or `create_new` succeeded, so a `to` somebody else made (an `AlreadyExists`
/// from the race after the check above) is never ours to remove.
struct Created<'a>(Option<&'a Path>);

impl<'a> Created<'a> {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Created<'_> {
    fn drop(&mut self) {
        if let Some(to) = self.0 {
            let _ = std::fs::remove_file(to);
        }
    }
}

fn clone_new_file(from: &Path, to: &Path, reflink: Reflink, copy: CopyData) -> Result<CloneKind> {
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let src = File::open(from).map_err(|e| io(from, e))?;
    // Any failure falls back to the copy: a cause other than the file system (a missing
    // directory, no space) fails the copy too, with its own error. A failed clone leaves no file.
    let (created, dst, kind) = match reflink(&src, to) {
        Ok(()) => {
            let created = Created(Some(to));
            if cfg!(target_vendor = "apple") {
                created.keep();
                return Ok(CloneKind::Reflink);
            }
            let dst = File::options().write(true).open(to).map_err(|e| io(to, e))?;
            (created, dst, CloneKind::Reflink)
        }
        Err(_) => {
            let dst = File::options().write(true).create_new(true).open(to).map_err(|e| io(to, e))?;
            let created = Created(Some(to));
            copy(&src, &dst, from, to)?;
            (created, dst, COPY_KIND)
        }
    };
    copy_metadata(&src, &dst).map_err(|e| io(to, e))?;
    created.keep();
    Ok(kind)
}

/// `clonefile(2)`: `to` shares `src`'s blocks and carries its mode, ACL and extended attributes.
#[cfg(target_vendor = "apple")]
fn reflink(src: &File, to: &Path) -> io::Result<()> {
    rustix::fs::fclonefileat(src, rustix::fs::CWD, to, rustix::fs::CloneFlags::empty()).map_err(io::Error::from)
}

/// `FICLONE` into a new file, removed again if the file system can't.
#[cfg(target_os = "linux")]
fn reflink(src: &File, to: &Path) -> io::Result<()> {
    let dst = File::options().write(true).create_new(true).open(to)?;
    let cloned = rustix::fs::ioctl_ficlone(&dst, src);
    drop(dst);
    if cloned.is_err() {
        let _ = std::fs::remove_file(to);
    }
    cloned.map_err(io::Error::from)
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn reflink(_: &File, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Copies `src`'s data extents into the empty `dst`, leaving its holes as holes. A file system
/// that can't report extents has its file copied whole.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn copy_data(src: &File, dst: &File, from: &Path, to: &Path) -> Result<()> {
    copy_extents(src, dst, from, to, &|f, whence| rustix::fs::seek(f, whence))
}

/// [`copy_data`] with `seek` given, so a test can make `SEEK_DATA`/`SEEK_HOLE` fail as a file
/// system without them does.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn copy_extents(
    src: &File,
    dst: &File,
    from: &Path,
    to: &Path,
    seek: &dyn Fn(&File, rustix::fs::SeekFrom) -> rustix::io::Result<u64>,
) -> Result<()> {
    use rustix::fs::SeekFrom;
    use std::os::unix::fs::FileExt;
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let len = src.metadata().map_err(|e| io(from, e))?.len();
    dst.set_len(len).map_err(|e| io(to, e))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < len {
        let (data, end) = match seek(src, SeekFrom::Data(pos)) {
            Ok(d) => (d, seek(src, SeekFrom::Hole(d)).map_or(len, |h| h.min(len))),
            // No data from `pos` on: the rest is a hole.
            Err(rustix::io::Errno::NXIO) => break,
            Err(_) => (pos, len),
        };
        let mut at = data;
        while at < end {
            let n = ((end - at) as usize).min(buf.len());
            src.read_exact_at(&mut buf[..n], at).map_err(|e| io(from, e))?;
            dst.write_all_at(&buf[..n], at).map_err(|e| io(to, e))?;
            at += n as u64;
        }
        pos = end;
    }
    Ok(())
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn copy_data(src: &File, dst: &File, from: &Path, _: &Path) -> Result<()> {
    io::copy(&mut &*src, &mut &*dst).map(drop).map_err(|e| Error::Io(from.to_path_buf(), e))
}

/// The source's permission bits and, best effort, its extended attributes.
fn copy_metadata(src: &File, dst: &File) -> io::Result<()> {
    dst.set_permissions(src.metadata()?.permissions())?;
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    {
        use rustix::fs::{XattrFlags, fgetxattr, flistxattr, fsetxattr};
        let names = xattr_names(&mut |buf| flistxattr(src, buf), &mut || rustix::fs::flistxattr_query(src))?;
        let mut value = Vec::new();
        for name in names.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            let Ok(size) = rustix::fs::fgetxattr_query(src, name) else { continue };
            value.resize(size, 0);
            if let Ok(got) = fgetxattr(src, name, &mut value[..]) {
                let _ = fsetxattr(dst, name, &value[..got], XattrFlags::empty());
            }
        }
    }
    Ok(())
}

/// The NUL-separated names of the source's extended attributes, best effort: a file system
/// without them, or a list that keeps changing, gives none.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn xattr_names(
    list: &mut dyn FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    query: &mut dyn FnMut() -> rustix::io::Result<usize>,
) -> io::Result<Vec<u8>> {
    let mut names = vec![0u8; 4096];
    // The list can grow between the size query and the read (ERANGE again): ask again a few
    // times, then go without rather than fail the clone for a best-effort copy.
    for _ in 0..4 {
        match list(&mut names[..]) {
            Ok(n) => {
                names.truncate(n);
                return Ok(names);
            }
            Err(rustix::io::Errno::RANGE) => match query() {
                // Room for a little growth since the query.
                Ok(size) => names = vec![0u8; size + 256],
                Err(_) => break,
            },
            // No extended attributes here (a file system without them).
            Err(_) => break,
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("emu-avd-clone-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn unsupported(_: &File, _: &Path) -> io::Result<()> {
        Err(rustix::io::Errno::NOTSUP.into())
    }

    fn cross_volume(_: &File, _: &Path) -> io::Result<()> {
        Err(rustix::io::Errno::XDEV.into())
    }

    /// A 256 MiB image that is almost all hole, with data at its start, middle and end.
    fn sparse_image(path: &Path) {
        let f = File::create(path).expect("src");
        f.set_len(256 << 20).expect("len");
        f.write_all_at(b"head", 0).expect("write");
        f.write_all_at(b"middle", 100 << 20).expect("write");
        f.write_all_at(b"tail", (256 << 20) - 4).expect("write");
    }

    fn write_at(path: &Path, at: u64, b: &[u8]) {
        File::options().write(true).open(path).expect("open").write_all_at(b, at).expect("write");
    }

    fn read_at(path: &Path, at: u64, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        File::open(path).expect("open").read_exact_at(&mut v, at).expect("read");
        v
    }

    /// Writes to either side after a clone never show on the other, whichever way it was made.
    fn assert_independent(src: &Path, dst: &Path) {
        write_at(dst, 0, b"DST!");
        assert_eq!(read_at(src, 0, 4), b"head", "the source is unchanged by a write to its clone");
        write_at(src, 100 << 20, b"SOURCE");
        assert_eq!(read_at(dst, 100 << 20, 6), b"middle", "the clone is unchanged by a write to its source");
        assert_eq!(read_at(dst, 0, 4), b"DST!");
        assert_eq!(read_at(src, 100 << 20, 6), b"SOURCE");
    }

    fn fs_type(dir: &Path) -> String {
        #[cfg(target_vendor = "apple")]
        {
            let s = rustix::fs::statfs(dir).expect("statfs");
            s.f_fstypename.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect()
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = dir;
            String::new()
        }
    }

    #[test]
    fn a_clone_is_independent_of_its_source_after_writes() {
        let dir = tmp("independent");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        sparse_image(&src);
        let kind = clone_file(&src, &dst).expect("clone");
        if fs_type(&dir) == "apfs" {
            assert_eq!(kind, CloneKind::Reflink, "APFS clones with clonefile");
        }
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        assert_independent(&src, &dst);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unsupported_clone_falls_back_to_a_sparse_copy_with_mode_and_xattrs() {
        for (name, attempt) in [("enotsup", unsupported as fn(&File, &Path) -> io::Result<()>), ("exdev", cross_volume)] {
            let dir = tmp(name);
            let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
            sparse_image(&src);
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).expect("chmod");
            let has_xattr = rustix::fs::setxattr(src.as_path(), "user.simee.test", b"kept", rustix::fs::XattrFlags::empty()).is_ok();
            assert_eq!(clone_with(&src, &dst, attempt, copy_data).expect("copy"), CloneKind::Sparse, "{name}");
            let meta = std::fs::metadata(&dst).expect("meta");
            assert_eq!(meta.len(), 256 << 20, "{name}");
            assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"), "{name}");
            assert!(meta.blocks() * 512 < 8 << 20, "{name}: the copy took {} bytes of a 256 MiB hole", meta.blocks() * 512);
            assert_eq!(meta.permissions().mode() & 0o7777, 0o640, "{name}: mode kept");
            if has_xattr {
                let mut v = [0u8; 16];
                let n = rustix::fs::getxattr(dst.as_path(), "user.simee.test", &mut v[..]).expect("xattr kept");
                assert_eq!(&v[..n], b"kept", "{name}");
            }
            assert_independent(&src, &dst);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_clone_never_overwrites_and_a_failed_one_leaves_nothing() {
        let dir = tmp("refuse");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        std::fs::write(&dst, b"mine").expect("dst");
        assert!(clone_file(&src, &dst).is_err());
        assert!(clone_with(&src, &dst, unsupported, copy_data).is_err());
        assert_eq!(std::fs::read(&dst).expect("dst"), b"mine", "an existing file is never replaced");
        let missing = dir.join("out.img");
        assert!(clone_file(&dir.join("missing.img"), &missing).is_err());
        assert!(clone_with(&src, &dir.join("no-such-dir/out.img"), unsupported, copy_data).is_err());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Another process makes `to` after our existence check, then the clone fails with EEXIST.
    fn raced(_: &File, to: &Path) -> io::Result<()> {
        std::fs::write(to, b"theirs")?;
        Err(rustix::io::Errno::EXIST.into())
    }

    /// A copy that dies mid-way (a full disk): some bytes written, then an error.
    fn dies_mid_copy(_: &File, dst: &File, _: &Path, to: &Path) -> Result<()> {
        dst.write_all_at(b"partial", 0).expect("write");
        Err(Error::Io(to.to_path_buf(), rustix::io::Errno::NOSPC.into()))
    }

    #[test]
    fn a_file_another_process_created_first_is_never_removed() {
        let dir = tmp("raced");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        assert!(clone_with(&src, &dst, raced, copy_data).is_err());
        assert_eq!(std::fs::read(&dst).expect("theirs survives"), b"theirs");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pre_existing_destination_survives_every_failure() {
        let dir = tmp("preexisting");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        std::fs::write(&dst, b"mine").expect("dst");
        assert!(clone_with(&src, &dst, unsupported, copy_data).is_err());
        assert!(clone_with(&src, &dst, unsupported, dies_mid_copy).is_err());
        assert_eq!(std::fs::read(&dst).expect("dst"), b"mine");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failure_mid_copy_leaves_nothing_behind() {
        let dir = tmp("mid-copy");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        assert!(clone_with(&src, &dst, unsupported, dies_mid_copy).is_err());
        assert!(dst.symlink_metadata().is_err(), "the half-written copy is removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A sparse file whose last extent is a hole: data at the start and at 1 MiB, then nothing.
    fn trailing_hole_image(path: &Path, len: u64) {
        let f = File::create(path).expect("src");
        f.set_len(len).expect("len");
        f.write_all_at(b"head", 0).expect("write");
        f.write_all_at(b"mid", 1 << 20).expect("write");
    }

    #[test]
    fn a_trailing_hole_keeps_the_size_and_stays_a_hole() {
        let dir = tmp("trailing-hole");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        trailing_hole_image(&src, 256 << 20);
        let s = File::open(&src).expect("src");
        let d = File::create_new(&dst).expect("dst");
        copy_data(&s, &d, &src, &dst).expect("copy");
        let meta = std::fs::metadata(&dst).expect("meta");
        assert_eq!(meta.len(), 256 << 20, "the size includes the trailing hole");
        assert!(meta.blocks() * 512 < 8 << 20, "the hole took {} bytes", meta.blocks() * 512);
        assert_eq!(read_at(&dst, 0, 4), b"head");
        assert_eq!(read_at(&dst, 1 << 20, 3), b"mid");
        assert_eq!(read_at(&dst, (256 << 20) - 8, 8), [0u8; 8]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_system_without_seek_data_has_the_whole_file_copied() {
        let dir = tmp("no-seek-data");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        trailing_hole_image(&src, 3 << 20);
        let s = File::open(&src).expect("src");
        let d = File::create_new(&dst).expect("dst");
        let no_seek = |_: &File, _: rustix::fs::SeekFrom| Err(rustix::io::Errno::INVAL);
        copy_extents(&s, &d, &src, &dst, &no_seek).expect("whole copy");
        assert_eq!(std::fs::metadata(&dst).expect("meta").len(), 3 << 20);
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_growing_xattr_list_is_retried_then_skipped_not_fatal() {
        let list = |calls: &mut u32, buf: &mut [u8], settle_at: u32| {
            *calls += 1;
            if *calls < settle_at {
                return Err(rustix::io::Errno::RANGE);
            }
            buf[..7].copy_from_slice(b"user.a\0");
            Ok(7)
        };
        // The list grew again between the size query and the read, once.
        let mut calls = 0;
        let names = xattr_names(&mut |b| list(&mut calls, b, 3), &mut || Ok(16)).expect("retried");
        assert_eq!(names, b"user.a\0");
        // It never settles: no attributes, and the clone goes on.
        let mut calls = 0;
        let names = xattr_names(&mut |b| list(&mut calls, b, u32::MAX), &mut || Ok(16)).expect("skipped");
        assert!(names.is_empty());
        // A failing size query is skipped too.
        let mut calls = 0;
        let names = xattr_names(&mut |b| list(&mut calls, b, u32::MAX), &mut || Err(rustix::io::Errno::RANGE)).expect("skipped");
        assert!(names.is_empty());
    }
}
