//! Parsed `.ini` files remembered by (path, mtime, size, inode), so resolving a device doesn't
//! read and parse every AVD's pointer and `config.ini` on every call (#256).
//!
//! A hit costs the one `stat` that validates it; an edit, a replacement or a different file at
//! that path changes the stamp and is read again. A file modified within [`SETTLE`] of the read is
//! not remembered: a second edit in the same clock tick, of the same size, would keep the stamp
//! (git's "racily clean" rule), so a file is trusted once it is older than the timestamp's grain.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

use crate::{Error, Result, parse_ini};

/// How old a file's mtime must be before its parse is remembered (coarser than any filesystem's
/// timestamp grain: ext4 and HFS+ 1-2 s at worst).
const SETTLE: Duration = Duration::from_secs(2);

/// Remembered files before the table starts over (a home has tens of AVDs, two files each).
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime: SystemTime,
    len: u64,
    ino: u64,
}

struct Entry {
    stamp: Stamp,
    map: Arc<BTreeMap<String, String>>,
}

fn table() -> MutexGuard<'static, HashMap<PathBuf, Entry>> {
    static TABLE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    TABLE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

fn stamp(meta: &std::fs::Metadata) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    Some(Stamp { mtime: meta.modified().ok()?, len: meta.len(), ino: meta.ino() })
}

/// The parsed `.ini` at `path`; `None` when it isn't a regular file (missing, or a directory).
/// What [`parse_ini`] returns for the file's current contents, from memory when its stamp is
/// unchanged.
pub(crate) fn ini(path: &Path) -> Result<Option<Arc<BTreeMap<String, String>>>> {
    // `metadata` follows links, as `is_file` and `read` do.
    let Ok(meta) = std::fs::metadata(path) else { return Ok(None) };
    if !meta.is_file() {
        return Ok(None);
    }
    let now = stamp(&meta);
    if let Some(now) = now
        && let Some(e) = table().get(path)
        && e.stamp == now
    {
        return Ok(Some(e.map.clone()));
    }
    let read_at = SystemTime::now();
    let text = read_text(path, meta.len()).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let map = Arc::new(parse_ini(&text));
    let mut table = table();
    match now {
        // Settled before the read began: the contents read are the contents the stamp names.
        Some(s) if s.mtime + SETTLE <= read_at => {
            if table.len() >= MAX_ENTRIES {
                table.clear();
            }
            table.insert(path.to_path_buf(), Entry { stamp: s, map: map.clone() });
        }
        _ => {
            table.remove(path);
        }
    }
    Ok(Some(map))
}

/// The file's text, read with the size `stat` just gave (std's `read_to_string` asks the file
/// for its size again: two more system calls per file, which add up over a home of AVDs).
fn read_text(path: &Path, len: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0).saturating_add(1));
    // `take` hides the file from `read_to_end`, which would ask it for its size and position.
    std::fs::File::open(path)?.take(len.saturating_add(1)).read_to_end(&mut bytes)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Entries remembered, for tests.
#[cfg(test)]
pub(crate) fn remembered(path: &Path) -> bool {
    table().contains_key(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emu-avd-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    fn write_at(path: &Path, text: &str, age: Duration) {
        std::fs::write(path, text).expect("write");
        let f = std::fs::File::options().write(true).open(path).expect("open");
        f.set_modified(SystemTime::now() - age).expect("mtime");
    }

    fn get(path: &Path, key: &str) -> Option<String> {
        ini(path).expect("read").and_then(|m| m.get(key).cloned())
    }

    #[test]
    fn an_unchanged_settled_file_is_not_read_again() {
        let d = dir("hit");
        let f = d.join("config.ini");
        write_at(&f, "a=1\n", Duration::from_secs(60));
        let mtime = std::fs::metadata(&f).expect("meta").modified().expect("mtime");
        assert_eq!(get(&f, "a").as_deref(), Some("1"));
        assert!(remembered(&f));
        // Same size, same mtime, same inode: the stamp can't tell, so the remembered parse answers.
        std::fs::write(&f, "a=2\n").expect("rewrite");
        std::fs::File::options().write(true).open(&f).expect("open").set_modified(mtime).expect("restore mtime");
        assert_eq!(get(&f, "a").as_deref(), Some("1"), "an unchanged stamp is a hit");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_changed_mtime_size_or_file_is_read_again() {
        let d = dir("miss");
        let f = d.join("config.ini");
        write_at(&f, "a=1\n", Duration::from_secs(60));
        assert_eq!(get(&f, "a").as_deref(), Some("1"));
        // A newer mtime.
        write_at(&f, "a=2\n", Duration::from_secs(30));
        assert_eq!(get(&f, "a").as_deref(), Some("2"), "mtime");
        // The same mtime, another size.
        let mtime = std::fs::metadata(&f).expect("meta").modified().expect("mtime");
        std::fs::write(&f, "a=33\n").expect("rewrite");
        std::fs::File::options().write(true).open(&f).expect("open").set_modified(mtime).expect("restore mtime");
        assert_eq!(get(&f, "a").as_deref(), Some("33"), "size");
        // Replaced by another file of that size and mtime (a rename over it): another inode.
        let other = d.join("other.ini");
        write_at(&other, "a=44\n", Duration::from_secs(30));
        std::fs::File::options().write(true).open(&other).expect("open").set_modified(mtime).expect("mtime");
        std::fs::rename(&other, &f).expect("rename");
        assert_eq!(get(&f, "a").as_deref(), Some("44"), "inode");
        // Gone, and a directory: not a file.
        std::fs::remove_file(&f).expect("remove");
        assert!(ini(&f).expect("gone").is_none());
        std::fs::create_dir(&f).expect("dir");
        assert!(ini(&f).expect("dir").is_none());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_file_written_just_now_is_never_remembered() {
        let d = dir("racy");
        let f = d.join("config.ini");
        std::fs::write(&f, "a=1\n").expect("write");
        assert_eq!(get(&f, "a").as_deref(), Some("1"));
        assert!(!remembered(&f), "a second write in the same timestamp tick would keep its stamp");
        // An edit of the same size straight after is seen.
        std::fs::write(&f, "a=2\n").expect("rewrite");
        assert_eq!(get(&f, "a").as_deref(), Some("2"));
        // An mtime in the future (a skewed clock) isn't trusted either.
        write_at(&f, "a=3\n", Duration::ZERO);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .expect("open")
            .set_modified(UNIX_EPOCH + Duration::from_secs(u32::MAX.into()))
            .expect("mtime");
        assert_eq!(get(&f, "a").as_deref(), Some("3"));
        assert!(!remembered(&f));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_miss() {
        let d = dir("denied");
        let f = d.join("config.ini");
        write_at(&f, "a=1\n", Duration::from_secs(60));
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // Root reads it anyway; everyone else gets the error the plain read gave.
        if std::fs::read_to_string(&f).is_err() {
            assert!(matches!(ini(&f), Err(Error::Io(..))));
        }
        std::fs::remove_dir_all(d).ok();
    }
}
