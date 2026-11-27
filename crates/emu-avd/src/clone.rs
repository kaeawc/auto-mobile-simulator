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
//! skipped. Ownership and timestamps are the new file's own.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::{Error, Result};

/// How a file was copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneKind {
    /// A copy-on-write clone (`clonefile` on APFS, `FICLONE` on Linux): no data copied, blocks
    /// shared until either side writes.
    Reflink,
    /// Only the data extents copied: the file stays as sparse as the original.
    Sparse,
    /// `std::fs::copy`, on platforms with neither clones nor `SEEK_DATA`.
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
/// `to` must not exist, and a failed clone leaves no half-written file behind.
pub fn clone_file(from: &Path, to: &Path) -> Result<CloneKind> {
    clone_with(from, to, reflink)
}

/// [`clone_file`] with the clone attempt given, so tests can make it fail as an unsupported file
/// system does. `reflink` makes `to` from `src` or fails leaving no file at `to`.
fn clone_with(from: &Path, to: &Path, reflink: fn(&File, &Path) -> io::Result<()>) -> Result<CloneKind> {
    if to.symlink_metadata().is_ok() {
        return Err(Error::Io(to.to_path_buf(), io::ErrorKind::AlreadyExists.into()));
    }
    clone_new_file(from, to, reflink).inspect_err(|_| {
        let _ = std::fs::remove_file(to);
    })
}

fn clone_new_file(from: &Path, to: &Path, reflink: fn(&File, &Path) -> io::Result<()>) -> Result<CloneKind> {
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let src = File::open(from).map_err(|e| io(from, e))?;
    // Any failure falls back to the copy: a cause other than the file system (a missing
    // directory, no space) fails the copy too, with its own error.
    let (dst, kind) = match reflink(&src, to) {
        Ok(()) if cfg!(target_vendor = "apple") => return Ok(CloneKind::Reflink),
        Ok(()) => (File::options().write(true).open(to).map_err(|e| io(to, e))?, CloneKind::Reflink),
        Err(_) => {
            let dst = File::options().write(true).create_new(true).open(to).map_err(|e| io(to, e))?;
            copy_data(&src, &dst, from, to)?;
            (dst, CloneKind::Sparse)
        }
    };
    copy_metadata(&src, &dst).map_err(|e| io(to, e))?;
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
    use rustix::fs::{SeekFrom, seek};
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
        let mut names = vec![0u8; 4096];
        let n = match flistxattr(src, &mut names[..]) {
            Ok(n) => n,
            Err(rustix::io::Errno::RANGE) => {
                names = vec![0u8; rustix::fs::flistxattr_query(src)?];
                flistxattr(src, &mut names[..])?
            }
            // No extended attributes here (a file system without them).
            Err(_) => 0,
        };
        let mut value = Vec::new();
        for name in names[..n].split(|&b| b == 0).filter(|s| !s.is_empty()) {
            let Ok(size) = rustix::fs::fgetxattr_query(src, name) else { continue };
            value.resize(size, 0);
            if let Ok(got) = fgetxattr(src, name, &mut value[..]) {
                let _ = fsetxattr(dst, name, &value[..got], XattrFlags::empty());
            }
        }
    }
    Ok(())
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
            assert_eq!(clone_with(&src, &dst, attempt).expect("copy"), CloneKind::Sparse, "{name}");
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
        assert!(clone_with(&src, &dst, unsupported).is_err());
        assert_eq!(std::fs::read(&dst).expect("dst"), b"mine", "an existing file is never replaced");
        let missing = dir.join("out.img");
        assert!(clone_file(&dir.join("missing.img"), &missing).is_err());
        assert!(clone_with(&src, &dir.join("no-such-dir/out.img"), unsupported).is_err());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
