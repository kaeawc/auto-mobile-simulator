//! Provisions an Android SDK without Java, sdkmanager or avdmanager.
//!
//! Packages (platform-tools, system images) are resolved from Google's repository manifests,
//! downloaded with macOS's own `/usr/bin/curl` (resumable, into `<sdk>/.downloads`), checked
//! against the manifest's size and sha1, unpacked with `/usr/bin/unzip` into a temporary
//! directory under the SDK and renamed into the standard layout. Shelling out to the system's
//! curl and unzip keeps a TLS stack and a zip implementation out of the binary. The host
//! renderer is unpacked out of the `emulator` package the same way, into emu-rs's own directory
//! (see [`renderer`]). Other hosts (Linux, for preparing images and AVDs) use the `curl` and
//! `unzip` on their `PATH`.

mod manifest;
pub mod renderer;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

pub use manifest::{
    Archive, Download, Host, Manifest, RemotePackage, Revision, SysImagePath, install_dir, parse_site_list, system_image_path,
};

/// Google's SDK repository.
pub const REPO_URL: &str = "https://dl.google.com/android/repository/";

/// The main manifest (platform-tools, platforms, build-tools, emulator...).
pub fn repository_url() -> String {
    format!("{REPO_URL}repository2-3.xml")
}

/// The list of system-image manifests, one per tag family.
pub fn sites_url() -> String {
    format!("{REPO_URL}addons_list-5.xml")
}

/// The system-image manifest that carries `tag`. `default` lives under `sys-img/android/`, 16 KB
/// page-size variants (`google_apis_ps16k`) share their base tag's manifest, and every Android XR
/// image (`google-xr`, `android-xr-*`, `ai-glasses`, or the `android-xr` tag they carry) is
/// under `sys-img/google_xr/`.
pub fn sys_img_url(tag: &str) -> String {
    let dir = match tag {
        "default" => "android",
        t if sim_core::form_factor::is_xr_tag(t) => "google_xr",
        t => t.strip_suffix("_ps16k").unwrap_or(t),
    };
    format!("{REPO_URL}sys-img/{dir}/sys-img2-3.xml")
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("bad manifest: {0}")]
    Manifest(String),
    #[error("no package '{0}' in {1}")]
    UnknownPackage(String, String),
    #[error("no stable archive of '{0}' for {1}")]
    NoArchive(String, String),
    #[error("{0}")]
    Command(String),
    #[error("{path}: {what} mismatch (expected {expected}, got {got})")]
    Verify { path: PathBuf, what: &'static str, expected: String, got: String },
    #[error("{0}: expected exactly one top-level directory in the archive, found {1} entries")]
    Layout(PathBuf, usize),
    #[error("{0}: no emulator/lib64/{1} in the archive")]
    NoRenderer(PathBuf, &'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Io(path.to_path_buf(), e)
}

/// Lowercase hex sha1 of `data`.
pub fn sha1_hex(data: &[u8]) -> String {
    sha1_smol::Sha1::from(data).digest().to_string()
}

/// Lowercase hex sha1 of a file, streamed.
pub fn sha1_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha1_smol::Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.digest().to_string());
        }
        h.update(&buf[..n]);
    }
}

/// Human-readable byte count (`16.6 MB`, `1.74 GB`).
pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1e6;
    let b = bytes as f64;
    if b >= 1e9 { format!("{:.2} GB", b / 1e9) } else { format!("{:.1} MB", b / MB) }
}

/// Moves the unpacked directory `src` to `dest`, replacing whatever is there.
fn replace_dir(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    if dest.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(dest).map_err(io(dest))?;
    }
    std::fs::rename(src, dest).map_err(io(dest))
}

/// A directory removed on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A system tool: macOS's own copy in `/usr/bin` (never a `PATH` lookup there), else `name` as
/// found on the `PATH`.
fn host_tool(name: &str) -> PathBuf {
    if cfg!(target_os = "macos") { Path::new("/usr/bin").join(name) } else { PathBuf::from(name) }
}

/// An SDK root being provisioned.
#[derive(Clone, Debug)]
pub struct Sdk {
    root: PathBuf,
    /// Cached manifests older than this are fetched again.
    pub manifest_max_age: Duration,
    /// Silence curl's progress meter and our own progress lines.
    pub quiet: bool,
    pub curl: PathBuf,
    pub unzip: PathBuf,
}

impl Sdk {
    /// Opens (creating if needed) the SDK at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Sdk> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(io(&root))?;
        Ok(Sdk { root, manifest_max_age: Duration::from_secs(24 * 3600), quiet: false, curl: host_tool("curl"), unzip: host_tool("unzip") })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Download cache: manifests and in-flight archives (`<file>.part`).
    pub fn downloads(&self) -> PathBuf {
        self.root.join(".downloads")
    }

    fn say(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }

    /// Whether a package is installed at `dir` (relative to the SDK root), as the SDK tools see it.
    pub fn is_installed(&self, dir: &Path) -> bool {
        let d = self.root.join(dir);
        d.join("source.properties").is_file() || d.join("package.xml").is_file()
    }

    fn run(&self, cmd: &mut Command) -> Result<std::process::ExitStatus> {
        cmd.status().map_err(|e| {
            let program = Path::new(cmd.get_program());
            if e.kind() == std::io::ErrorKind::NotFound {
                let name = program.file_name().unwrap_or(program.as_os_str()).to_string_lossy();
                Error::Command(format!("`{}` not found: install {name} (e.g. `apt install {name}`)", program.display()))
            } else {
                Error::Command(format!("{}: {e}", program.display()))
            }
        })
    }

    /// Fetches (or reads from the daily cache) and parses a manifest.
    pub fn manifest(&self, url: &str) -> Result<Manifest> {
        let base = &url[..url.rfind('/').map_or(0, |i| i + 1)];
        Manifest::parse(&self.fetch_cached(url)?, base)
    }

    /// A manifest's text, refreshed when the cached copy is older than `manifest_max_age`. A
    /// failed refresh falls back to a stale cached copy.
    fn fetch_cached(&self, url: &str) -> Result<String> {
        let dl = self.downloads();
        std::fs::create_dir_all(&dl).map_err(io(&dl))?;
        let name: String = url.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        let cache = dl.join(name);
        let age = std::fs::metadata(&cache).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
        if age.is_none_or(|a| a > self.manifest_max_age) {
            let part = cache.with_extension("part");
            let status = self.run(Command::new(&self.curl).args(["-sSfL", "--retry", "3", "-o"]).arg(&part).arg(url))?;
            if status.success() {
                std::fs::rename(&part, &cache).map_err(io(&cache))?;
            } else if cache.is_file() {
                self.say(&format!("warning: could not refresh {url} ({status}); using the cached copy"));
            } else {
                return Err(Error::Command(format!("fetching {url} failed ({status})")));
            }
        }
        std::fs::read_to_string(&cache).map_err(io(&cache))
    }

    /// Every system-image manifest for `tag`, or for all tags (from `addons_list-5.xml`).
    pub fn system_image_manifests(&self, tag: Option<&str>) -> Result<Vec<Manifest>> {
        let urls = match tag {
            Some(t) => vec![sys_img_url(t)],
            None => parse_site_list(&self.fetch_cached(&sites_url())?, REPO_URL)?,
        };
        urls.iter().map(|u| self.manifest(u)).collect()
    }

    /// Downloads `d` (resuming a partial download), verifies its size and sha1, and unpacks it
    /// into `d.install_dir()`. Returns the installed directory. Replaces any incomplete install
    /// already there; callers skip installed packages with [`Sdk::is_installed`].
    pub fn install(&self, d: &Download) -> Result<PathBuf> {
        let part = self.fetch_verified(d)?;
        let tmp = self.unpack(d, false, |dest| vec!["-q".into(), part.clone().into(), "-d".into(), dest.into()])?;
        let entries: Vec<PathBuf> = std::fs::read_dir(&tmp.0)
            .map_err(io(&tmp.0))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n != "__MACOSX"))
            .collect();
        let top = match entries.as_slice() {
            [one] if one.is_dir() => one.clone(),
            _ => return Err(Error::Layout(part, entries.len())),
        };
        if !top.join("source.properties").is_file() {
            self.say(&format!("warning: {} has no source.properties", d.file_name()));
        }
        self.sparsify_images(&top);

        let dest = self.root.join(d.install_dir());
        replace_dir(&top, &dest)?;
        // The unpacked package is the cache; a system image archive is close to 2 GB.
        let _ = std::fs::remove_file(&part);
        Ok(dest)
    }

    /// Downloads the `emulator` package `d` and unpacks only the renderer
    /// ([`renderer::INCLUDE`] minus [`renderer::EXCLUDE`]) into
    /// `<sdk>/emu-rs/renderer/<revision>` with a [`renderer::MARKER`], points
    /// [`renderer::CURRENT`] at it and removes older revisions. The archive is deleted.
    pub fn install_renderer(&self, d: &Download) -> Result<renderer::Installed> {
        let part = self.fetch_verified(d)?;
        // LICENSE or NOTICE.txt may be missing from some revision; the backend may not.
        let tmp = self.unpack(d, true, |dest| renderer::unzip_args(&part, dest))?;
        let top = tmp.0.join("emulator");
        if !top.join("lib64").join(renderer::BACKEND).is_file() {
            let _ = std::fs::remove_file(&part);
            return Err(Error::NoRenderer(part, renderer::BACKEND));
        }
        let marker = renderer::Marker { revision: d.revision, sha1: d.sha1.clone(), url: d.url.clone() };
        let m = top.join(renderer::MARKER);
        std::fs::write(&m, marker.to_string()).map_err(io(&m))?;

        let dest = renderer::dir(&self.root, d.revision);
        replace_dir(&top, &dest)?;
        let _ = std::fs::remove_file(&part);

        // Repoint `current` atomically, then drop superseded revisions (a running emu keeps the
        // libraries it has already mapped).
        let root = renderer::root(&self.root);
        let current = root.join(renderer::CURRENT);
        let link = root.join(format!(".{}.tmp", renderer::CURRENT));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(d.revision.to_string(), &link).map_err(io(&link))?;
        std::fs::rename(&link, &current).map_err(io(&current))?;
        for old in renderer::installed(&self.root).into_iter().filter(|i| i.dir != dest) {
            self.say(&format!("removing renderer r{}", old.marker.revision));
            let _ = std::fs::remove_dir_all(&old.dir);
        }
        Ok(renderer::Installed { dir: dest, marker })
    }

    /// Downloads `d` into the download cache (resuming a partial download) and verifies its size
    /// and sha1. Returns the archive's path; a bad archive is deleted.
    fn fetch_verified(&self, d: &Download) -> Result<PathBuf> {
        let dl = self.downloads();
        std::fs::create_dir_all(&dl).map_err(io(&dl))?;
        let part = dl.join(format!("{}.part", d.file_name()));
        self.fetch_archive(d, &part)?;

        let got = std::fs::metadata(&part).map_err(io(&part))?.len();
        if got != d.size {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Verify { path: part, what: "size", expected: d.size.to_string(), got: got.to_string() });
        }
        self.say(&format!("verifying {}", d.file_name()));
        let sha = sha1_file(&part).map_err(io(&part))?;
        if sha != d.sha1 {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Verify { path: part, what: "sha1", expected: d.sha1.clone(), got: sha });
        }
        Ok(part)
    }

    /// Punches holes in the zero blocks of a package's disk images (`*.img`): unzip writes them
    /// out in full, and most of a system image is zeros (8.1 of TV 34's 8.6 GB `system.img`).
    /// Reads still return zeros there. Best effort: a filesystem without holes keeps the file.
    fn sparsify_images(&self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for path in entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "img") && p.is_file()) {
            match sparsify(&path) {
                Ok(0) => {}
                Ok(freed) => self.say(&format!(
                    "{}: {} of zeros made sparse",
                    path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                    format_size(freed)
                )),
                Err(e) => self.say(&format!("warning: {} stays dense: {e}", path.display())),
            }
        }
    }

    /// Runs unzip with the arguments `args` builds for a fresh temporary directory under the
    /// download cache, and returns that directory. Exit status 11 (a pattern matched nothing) is
    /// accepted when `allow_unmatched`; the caller then checks for the files it needs.
    fn unpack(&self, d: &Download, allow_unmatched: bool, args: impl FnOnce(&Path) -> Vec<OsString>) -> Result<TempDir> {
        self.say(&format!("unpacking {}", d.file_name()));
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|t| t.subsec_nanos()).unwrap_or(0);
        let tmp = TempDir(self.downloads().join(format!("unpack-{}-{nanos}", std::process::id())));
        std::fs::create_dir_all(&tmp.0).map_err(io(&tmp.0))?;
        let status = self.run(Command::new(&self.unzip).args(args(&tmp.0)))?;
        if !status.success() && !(allow_unmatched && status.code() == Some(11)) {
            return Err(Error::Command(format!("unzip of {} failed ({status})", d.file_name())));
        }
        Ok(tmp)
    }

    fn fetch_archive(&self, d: &Download, part: &Path) -> Result<()> {
        let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have > d.size {
            std::fs::remove_file(part).map_err(io(part))?;
        } else if have == d.size {
            return Ok(()); // complete; verified by the caller
        }
        self.say(&format!("downloading {} ({})", d.url, format_size(d.size)));
        for attempt in 0..2 {
            let mut cmd = Command::new(&self.curl);
            cmd.args(["-fL", "--retry", "3", "-C", "-", "-o"]).arg(part).arg(&d.url);
            if self.quiet {
                cmd.arg("-sS");
            }
            let status = self.run(&mut cmd)?;
            if status.success() {
                return Ok(());
            }
            // 33: the server can't resume (no byte ranges); start over once.
            if attempt == 0 && status.code() == Some(33) {
                let _ = std::fs::remove_file(part);
                continue;
            }
            return Err(Error::Command(format!("downloading {} failed ({status}); rerun to resume", d.url)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// Deallocates every all-zero, block-aligned run of `path` (4 KiB blocks, the APFS and ext4
/// block size) and returns how many bytes it freed. Contents read back unchanged.
pub fn sparsify(path: &Path) -> std::io::Result<u64> {
    use std::io::Read;
    const BLOCK: usize = 4096;
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut reader = std::io::BufReader::with_capacity(4 << 20, &file);
    let mut block = vec![0u8; BLOCK];
    let (mut offset, mut run_start, mut freed) = (0u64, None::<u64>, 0u64);
    let mut punch = |start: u64, end: u64| -> std::io::Result<()> {
        emu_hvf::host::punch_hole(&file, start, end - start)?;
        freed += end - start;
        Ok(())
    };
    loop {
        let mut n = 0;
        while n < BLOCK {
            match reader.read(&mut block[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        // A short final block stays allocated: holes cover whole blocks only.
        let zero = n == BLOCK && block.iter().all(|&b| b == 0);
        match (zero, run_start) {
            (true, None) => run_start = Some(offset),
            (false, Some(start)) => {
                punch(start, offset)?;
                run_start = None;
            }
            _ => {}
        }
        offset += n as u64;
        if n < BLOCK {
            break;
        }
    }
    if let Some(start) = run_start {
        punch(start, offset)?;
    }
    Ok(freed)
}
