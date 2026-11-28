//! Android's userspace in a container on the host's kernel (#210), for Linux hosts that have
//! binder: the third of emu's backends after KVM and before QEMU's TCG software mode.
//!
//! - [`probe`]: what the host kernel offers (binderfs, memfd, cgroups v2, user namespaces,
//!   overlayfs), each missing piece with the fix, over an injectable `/proc` and `/sys` view.
//! - [`select`]: which backend runs a guest, in the owner's order (KVM, container, TCG) with
//!   `-backend` to override. [`choose`] is the one function `apps/emu` calls.
//! - [`image`]: the official Redroid image, pinned by SHA-256, downloaded only on request.
//! - [`launch`]: the namespaces, mounts and `/init` arguments as data, and the runner.
//!
//! Safe Rust throughout: namespaces and mounts go through `unshare(1)` and `mount(8)`.

pub mod image;
pub mod launch;
pub mod probe;
pub mod select;

pub use select::{Availability, Backend, Facts, Selection, choose};

use std::path::Path;
use std::process::{Command, Stdio};

/// What [`ensure_image`] needs to know.
#[derive(Debug, Clone)]
pub struct ImageRequest<'a> {
    /// The directory images live under.
    pub cache: &'a Path,
    /// The Android version (its major number).
    pub android: u32,
    /// The host architecture (`std::env::consts::ARCH`).
    pub arch: &'a str,
    pub fetch: image::Fetch,
    pub idmap: &'a launch::IdMap,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Image(#[from] image::Error),
    #[error(transparent)]
    Launch(#[from] launch::Error),
    #[error("`{command}` failed ({status}){detail}")]
    Command { command: String, status: std::process::ExitStatus, detail: String },
    #[error("running `{command}`: {err} (is it installed?)")]
    Spawn { command: String, err: std::io::Error },
}

pub type Result<T> = std::result::Result<T, Error>;

fn run(argv: &[String]) -> Result<()> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Error::Spawn { command: argv[0].clone(), err })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = if stderr.trim().is_empty() { String::new() } else { format!(": {}", stderr.trim()) };
    Err(Error::Command { command: argv.join(" "), status: out.status, detail })
}

/// Makes sure the pinned image is unpacked under `req.cache` and returns where. A download
/// happens only when `req.fetch` allows it; what is on disk is verified against the pin before it
/// is unpacked, so a bad download or a swapped file stops here.
pub fn ensure_image(req: &ImageRequest) -> Result<image::Paths> {
    let (img, pin) =
        image::pinned(req.android, req.arch).ok_or_else(|| image::Error::NoPin { android: req.android, arch: req.arch.to_string() })?;
    ensure_pinned(img, pin, req)
}

/// [`ensure_image`] for a given pin.
fn ensure_pinned(img: &image::Pinned, pin: &image::ArchPin, req: &ImageRequest) -> Result<image::Paths> {
    let paths = image::paths(req.cache, img, pin);
    let mut step = image::next_step(&paths, pin, req.fetch)?;
    if step == image::Step::Pull {
        std::fs::create_dir_all(paths.layout.parent().unwrap_or(req.cache)).map_err(|e| image::Error::Io(paths.layout.clone(), e))?;
        run(&image::pull_command(img, pin, &paths.layout))?;
        step = image::Step::Unpack;
    }
    if step == image::Step::Unpack {
        let layer = image::verify_layout(&paths.layout, pin)?;
        // A half-unpacked rootfs from a failed run is not a rootfs: start clean.
        let _ = std::fs::remove_dir_all(&paths.rootfs);
        std::fs::create_dir_all(&paths.rootfs).map_err(|e| image::Error::Io(paths.rootfs.clone(), e))?;
        // Unpack inside the container's user namespace, so file owners land as the container sees
        // them (script: wait for the id maps, then tar).
        let tar = image::unpack_command(&layer, &paths.rootfs).iter().map(|a| launch::quote(a)).collect::<Vec<_>>().join(" ");
        let child = launch::spawn_isolated(req.idmap, &format!("set -eu\nread -r _go\nexec {tar}\n"), Stdio::null(), Stdio::piped())?;
        let out = child.wait_with_output().map_err(|err| Error::Spawn { command: "unshare tar".into(), err })?;
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&paths.rootfs);
            return Err(Error::Command {
                command: format!("unshare … tar into {}", paths.rootfs.display()),
                status: out.status,
                detail: format!(": {}", String::from_utf8_lossy(&out.stderr).trim()),
            });
        }
        std::fs::write(&paths.unpacked, format!("{}\n", pin.layer)).map_err(|e| image::Error::Io(paths.unpacked.clone(), e))?;
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::image::fixture::{HELLO, HELLO_LAYOUT, copy_dir, hello_image};
    use super::*;

    fn cache(tag: &str) -> std::path::PathBuf {
        image::fixture::tmp(tag)
    }

    fn request<'a>(cache: &'a Path, fetch: image::Fetch, idmap: &'a launch::IdMap) -> ImageRequest<'a> {
        ImageRequest { cache, android: 15, arch: "x86_64", fetch, idmap }
    }

    #[test]
    fn an_unpinned_version_or_architecture_is_refused() {
        let c = cache("lib-nopin");
        let map = launch::IdMap::Identity;
        for (android, arch) in [(16, "x86_64"), (15, "riscv64")] {
            let req = ImageRequest { android, arch, ..request(&c, image::Fetch::Allowed, &map) };
            assert!(matches!(ensure_image(&req), Err(Error::Image(image::Error::NoPin { .. }))));
        }
    }

    #[test]
    fn nothing_is_downloaded_unless_asked() {
        let c = cache("lib-denied");
        let map = launch::IdMap::Identity;
        let e = ensure_image(&request(&c, image::Fetch::Denied, &map)).expect_err("not on disk");
        assert!(e.to_string().contains("-container-pull"), "{e}");
        assert!(!c.join("redroid-15-x86_64").exists(), "refusing to download leaves nothing behind");
    }

    #[test]
    fn a_layout_that_isnt_the_pinned_image_is_refused_before_unpacking() {
        let c = cache("lib-wrong");
        let map = launch::IdMap::Identity;
        // A real layout of some other image, sitting where the pinned one belongs.
        let (img, pin) = image::pinned(15, "x86_64").expect("pin");
        let paths = image::paths(&c, img, pin);
        copy_dir(Path::new(HELLO_LAYOUT), &paths.layout);
        let e = ensure_image(&request(&c, image::Fetch::Denied, &map)).expect_err("wrong image");
        assert!(matches!(e, Error::Image(image::Error::Digest { .. })), "{e}");
        assert!(!paths.rootfs.exists() && !paths.unpacked.exists(), "nothing unpacked");
    }

    /// Unpacks a real layout through a real user namespace with `tar`. Needs `unshare` and `tar`;
    /// run with `SIMEE_E2E_CONTAINER=1`.
    #[test]
    fn real_host_unpacks_a_verified_layer_with_its_owners() {
        if std::env::var_os("SIMEE_E2E_CONTAINER").is_none() {
            eprintln!("skipped: set SIMEE_E2E_CONTAINER=1 to unpack through this host's user namespaces");
            return;
        }
        let c = cache("lib-unpack");
        let idmap = launch::IdMap::for_host(&probe::FsView, rustix::process::getegid().as_raw()).expect("an id mapping for this user");
        let (img, pin) = (hello_image(), HELLO);
        copy_dir(Path::new(HELLO_LAYOUT), &image::paths(&c, &img, &pin).layout);
        let req = request(&c, image::Fetch::Denied, &idmap);
        let paths = ensure_pinned(&img, &pin, &req).expect("unpacked");
        assert!(paths.rootfs.join("hello").is_file(), "the layer's one file");
        assert_eq!(std::fs::read_to_string(&paths.unpacked).expect("marker").trim(), pin.layer);
        assert_eq!(image::next_step(&paths, &pin, image::Fetch::Denied).expect("step"), image::Step::Ready);
        // Ready means no work: a second call changes nothing.
        let again = ensure_pinned(&img, &pin, &req).expect("ready");
        assert_eq!(again, paths);
    }
}
