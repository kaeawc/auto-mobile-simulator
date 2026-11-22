//! The device's filesystem: app containers, sizes, files, preferences and crash reports. Every
//! path is confined to the device's data directory.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// An app's containers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    pub bundle_id: String,
    /// Data container (Documents, Library, tmp), relative to the device data directory.
    pub data: Option<String>,
    /// Bundle container (the installed .app), relative to the device data directory.
    pub bundle: Option<String>,
    pub data_bytes: u64,
    pub bundle_bytes: u64,
}

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub bytes: u64,
}

/// Joins `rel` under `root`, refusing absolute paths and `..`.
pub fn confine(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim_start_matches('/');
    let mut out = root.to_path_buf();
    for part in Path::new(rel).components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return Err(format!("path escapes the device: {rel}")),
        }
    }
    Ok(out)
}

/// Total size of a file or directory tree (symlinks not followed).
pub fn size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map(|it| it.flatten().map(|e| size(&e.path())).sum()).unwrap_or(0)
}

/// Lists a directory under the device data directory.
pub fn list(root: &Path, rel: &str) -> Result<Vec<Entry>, String> {
    let dir = confine(root, rel)?;
    let mut out: Vec<Entry> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| {
            let meta = e.metadata().ok();
            let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
            Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                dir: is_dir,
                bytes: if is_dir { 0 } else { meta.map_or(0, |m| m.len()) },
            }
        })
        .collect();
    out.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
    Ok(out)
}

/// Reads a file under the device data directory.
pub fn read(root: &Path, rel: &str) -> Result<Vec<u8>, String> {
    let path = confine(root, rel)?;
    fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes a file under the device data directory, creating parent directories.
pub fn write(root: &Path, rel: &str, bytes: &[u8]) -> Result<(), String> {
    let path = confine(root, rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Removes a file or directory tree under the device data directory.
pub fn remove(root: &Path, rel: &str) -> Result<(), String> {
    let path = confine(root, rel)?;
    if path == root {
        return Err("refusing to remove the whole device".into());
    }
    let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) }.map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads the bundle identifier of a container from its metadata plist.
fn container_id(dir: &Path) -> Option<String> {
    let meta = dir.join(".com.apple.mobile_container_manager.metadata.plist");
    let out = crate::run("/usr/bin/plutil", &["-extract", "MCMMetadataIdentifier", "raw", "-o", "-", &meta.to_string_lossy()]).ok()?;
    let id = out.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

fn containers(root: &Path, kind: &str) -> Vec<(String, PathBuf)> {
    let base = root.join("Containers").join(kind).join("Application");
    fs::read_dir(base).map(|it| it.flatten().filter_map(|e| Some((container_id(&e.path())?, e.path()))).collect()).unwrap_or_default()
}

/// Every app with a container, with sizes when `sizes` is set (slow for large devices).
pub fn apps(root: &Path, sizes: bool) -> Vec<App> {
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned();
    let mut apps: Vec<App> = Vec::new();
    for (id, path) in containers(root, "Data") {
        let data_bytes = if sizes { size(&path) } else { 0 };
        apps.push(App { bundle_id: id, data: Some(rel(&path)), data_bytes, ..App::default() });
    }
    for (id, path) in containers(root, "Bundle") {
        let bundle_bytes = if sizes { size(&path) } else { 0 };
        match apps.iter_mut().find(|a| a.bundle_id == id) {
            Some(app) => {
                app.bundle = Some(rel(&path));
                app.bundle_bytes = bundle_bytes;
            }
            None => apps.push(App { bundle_id: id, bundle: Some(rel(&path)), bundle_bytes, ..App::default() }),
        }
    }
    apps.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));
    apps
}

/// Crash reports on the host that belong to the device, newest first.
pub fn crash_reports(udid: &str) -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let dir = Path::new(&home).join("Library/Logs/DiagnosticReports");
    let mut found: Vec<(std::time::SystemTime, String)> = fs::read_dir(&dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "ips" || x == "crash"))
                .filter(|e| fs::read_to_string(e.path()).is_ok_and(|t| t.contains(udid)))
                .map(|e| {
                    (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path().to_string_lossy().into_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confine_rejects_escapes() {
        let root = Path::new("/dev-data");
        assert_eq!(confine(root, "Library/Preferences").ok(), Some(PathBuf::from("/dev-data/Library/Preferences")));
        assert_eq!(confine(root, "/Documents/./a").ok(), Some(PathBuf::from("/dev-data/Documents/a")));
        assert!(confine(root, "../etc").is_err());
        assert!(confine(root, "a/../../b").is_err());
    }

    #[test]
    fn list_read_write_remove_and_size() {
        let root = std::env::temp_dir().join(format!("sim-debug-test-{}", std::process::id()));
        fs::create_dir_all(&root).expect("tmp");
        write(&root, "a/b.txt", b"hello").expect("write");
        assert_eq!(read(&root, "a/b.txt").expect("read"), b"hello");
        assert_eq!(list(&root, "a").expect("list"), vec![Entry { name: "b.txt".into(), dir: false, bytes: 5 }]);
        assert_eq!(size(&root), 5);
        assert!(remove(&root, "").is_err());
        remove(&root, "a").expect("remove");
        assert!(list(&root, "").expect("list").is_empty());
        fs::remove_dir_all(root).ok();
    }
}
