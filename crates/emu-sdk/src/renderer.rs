//! The host renderer (gfxstream), fetched on demand from the SDK's `emulator` package.
//!
//! Only the renderer's `lib64` is unpacked (minus the stock emulator's Qt UI libraries), into
//! `<sdk>/emu-rs/renderer/<revision>/lib64`, never `<sdk>/emulator`: sdkmanager keeps tracking
//! its own packages, and a later real emulator install doesn't collide with this one. A marker
//! file records what was installed; `current` links to the newest revision so a build-time rpath
//! (`apps/emu/build.rs`) can name a stable path.
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::{Error, Revision};

/// The repository package that carries the renderer.
pub const PACKAGE: &str = "emulator";
/// The renderer library; a `lib64` without it isn't a renderer.
pub const BACKEND: &str = "libgfxstream_backend.dylib";
/// Archive members unpacked: the renderer's `lib64` (backend, its `@rpath` dependencies, the
/// shader translator it `dlopen`s by name, `gles_angle`/`gles_swiftshader` and the Vulkan
/// loader, ICDs and layers it finds relative to the launcher directory), plus the license files.
pub const INCLUDE: &[&str] = &["emulator/lib64/*", "emulator/LICENSE", "emulator/NOTICE.txt"];
/// Members skipped: the stock emulator's Qt window (about half of `lib64`), which the renderer
/// never loads.
pub const EXCLUDE: &[&str] = &["emulator/lib64/qt/*"];
/// Written into each install: revision, sha1 and URL of the archive it came from.
pub const MARKER: &str = "renderer.properties";
/// Symlink to the newest installed revision.
pub const CURRENT: &str = "current";

/// `<sdk>/emu-rs/renderer`.
pub fn root(sdk_root: &Path) -> PathBuf {
    sdk_root.join("emu-rs/renderer")
}

/// `<sdk>/emu-rs/renderer/<revision>`.
pub fn dir(sdk_root: &Path, revision: Revision) -> PathBuf {
    root(sdk_root).join(revision.to_string())
}

/// The SDK's own emulator package's `lib64`, if it holds the renderer.
pub fn sdk_emulator_lib64(sdk_root: &Path) -> Option<PathBuf> {
    let p = sdk_root.join("emulator/lib64");
    p.join(BACKEND).is_file().then_some(p)
}

/// `/usr/bin/unzip` arguments that unpack just the renderer from `archive` into `dest`.
pub fn unzip_args(archive: &Path, dest: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-q".into(), archive.into()];
    args.extend(INCLUDE.iter().map(OsString::from));
    args.push("-x".into());
    args.extend(EXCLUDE.iter().map(OsString::from));
    args.push("-d".into());
    args.push(dest.into());
    args
}

/// The contents of [`MARKER`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub revision: Revision,
    pub sha1: String,
    pub url: String,
}

impl fmt::Display for Marker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "# The gfxstream renderer from the SDK emulator package, installed by emu -provision-renderer.")?;
        writeln!(f, "Pkg.Path={PACKAGE}")?;
        writeln!(f, "Pkg.Revision={}", self.revision)?;
        writeln!(f, "Archive.Sha1={}", self.sha1)?;
        writeln!(f, "Archive.Url={}", self.url)
    }
}

impl FromStr for Marker {
    type Err = Error;

    fn from_str(s: &str) -> Result<Marker, Error> {
        let get = |key: &str| {
            s.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_string())
                .ok_or_else(|| Error::Manifest(format!("{MARKER}: no {key}")))
        };
        Ok(Marker { revision: get("Pkg.Revision")?.parse()?, sha1: get("Archive.Sha1")?, url: get("Archive.Url")? })
    }
}

/// A renderer installed under `<sdk>/emu-rs/renderer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub dir: PathBuf,
    pub marker: Marker,
}

impl Installed {
    pub fn lib64(&self) -> PathBuf {
        self.dir.join("lib64")
    }
}

/// Complete renderer installs, newest first: directories named after their revision, holding
/// a marker for that revision and the backend library. Partial unpacks never qualify (an install
/// is moved into place whole, marker included).
pub fn installed(sdk_root: &Path) -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(root(sdk_root)) else { return Vec::new() };
    let mut out: Vec<Installed> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let revision: Revision = e.file_name().to_str()?.parse().ok()?;
            let dir = e.path();
            let marker: Marker = std::fs::read_to_string(dir.join(MARKER)).ok()?.parse().ok()?;
            (marker.revision == revision && dir.join("lib64").join(BACKEND).is_file()).then_some(Installed { dir, marker })
        })
        .collect();
    out.sort_by_key(|i| std::cmp::Reverse(i.marker.revision));
    out
}

/// The `lib64` to load the renderer from: the SDK's own emulator package when it has one (it is
/// what the stock emulator uses, and what sdkmanager keeps up to date), else the newest renderer
/// installed by `emu -provision-renderer`.
pub fn find_lib64(sdk_root: &Path) -> Option<PathBuf> {
    sdk_emulator_lib64(sdk_root).or_else(|| installed(sdk_root).first().map(Installed::lib64))
}

/// Bytes on disk under `path`, symlinks counted as themselves.
pub fn disk_usage(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map(|rd| rd.flatten().map(|e| disk_usage(&e.path())).sum()).unwrap_or(0)
}
