//! Host file changes through kqueue: a device's preference files and the directories that hold
//! them, so a feed wakes when cfprefsd writes one instead of polling. A directory is told about a
//! file renamed into it (how cfprefsd replaces a plist), a file about a write in place.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

/// A kqueue watching a set of paths for changes.
pub struct FileWatch {
    kq: OwnedFd,
    /// One descriptor per watched path; closing one removes its event.
    fds: Vec<OwnedFd>,
}

impl FileWatch {
    pub fn new() -> io::Result<FileWatch> {
        // SAFETY: kqueue takes no arguments and returns a new descriptor or -1.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a descriptor kqueue just returned, owned by nothing else.
        Ok(FileWatch { kq: unsafe { OwnedFd::from_raw_fd(fd) }, fds: Vec::new() })
    }

    /// Watches exactly `paths` from now on (files or directories): written, extended, renamed,
    /// deleted or their attributes changed. A path that doesn't exist (yet) is skipped; how many
    /// were watched is returned.
    pub fn set(&mut self, paths: &[PathBuf]) -> io::Result<usize> {
        self.fds.clear();
        for path in paths {
            let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { continue };
            // SAFETY: `c` is a NUL-terminated path; O_EVTONLY opens for event notification only.
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                continue;
            }
            // SAFETY: `fd` was just opened and is owned by nothing else.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let change = libc::kevent {
                ident: fd.as_raw_fd() as libc::uintptr_t,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE
                    | libc::NOTE_EXTEND
                    | libc::NOTE_DELETE
                    | libc::NOTE_RENAME
                    | libc::NOTE_ATTRIB
                    | libc::NOTE_REVOKE,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: one valid change record in, no event list out, no timeout.
            let rc = unsafe { libc::kevent(self.kq.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            self.fds.push(fd);
        }
        Ok(self.fds.len())
    }

    /// Waits up to `timeout` for a change to a watched path; whether one came. Every change
    /// pending is taken, so one wake covers a burst.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        let ts = libc::timespec {
            tv_sec: timeout.as_secs().min(i64::MAX as u64) as libc::time_t,
            tv_nsec: libc::c_long::from(timeout.subsec_nanos() as i32),
        };
        let mut events: [libc::kevent; 16] =
            // SAFETY: kevent is plain data; an all-zero record is valid and only written by the kernel.
            unsafe { std::mem::zeroed() };
        // SAFETY: no changes in; `events` has room for 16 records; `ts` lives for the call.
        let n = unsafe { libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, events.as_mut_ptr(), events.len() as i32, &ts) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
        }
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rename_into_a_watched_directory_and_a_write_in_place_wake_it() {
        let dir = std::env::temp_dir().join(format!("sim-coresim-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("a.plist");
        std::fs::write(&file, b"1").expect("write");
        let mut watch = FileWatch::new().expect("kqueue");
        assert_eq!(watch.set(&[dir.clone(), file.clone(), dir.join("missing")]).expect("set"), 2, "a missing path is skipped");
        assert!(!watch.wait(Duration::from_millis(20)).expect("wait"), "quiet");
        let tmp = dir.join("a.plist.tmp");
        std::fs::write(&tmp, b"2").expect("write");
        std::fs::rename(&tmp, &file).expect("rename");
        assert!(watch.wait(Duration::from_secs(5)).expect("wait"), "the rename");
        watch.set(std::slice::from_ref(&file)).expect("set");
        std::fs::OpenOptions::new().append(true).open(&file).and_then(|mut f| std::io::Write::write_all(&mut f, b"3")).expect("append");
        assert!(watch.wait(Duration::from_secs(5)).expect("wait"), "the write in place");
        std::fs::remove_dir_all(dir).ok();
    }
}
