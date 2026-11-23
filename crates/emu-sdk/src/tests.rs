use super::*;

const SYS_IMG: &str = include_str!("../testdata/sys-img2-3.xml");
const REPO: &str = include_str!("../testdata/repository2-3.xml");
const SYS_IMG_BASE: &str = "https://dl.google.com/android/repository/sys-img/google_apis/";
const HOST: Host = Host::MACOS_ARM64;

fn sys_img() -> Manifest {
    Manifest::parse(SYS_IMG, SYS_IMG_BASE).expect("parse sys-img fixture")
}

fn repo() -> Manifest {
    Manifest::parse(REPO, REPO_URL).expect("parse repository fixture")
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("emu-sdk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

#[test]
fn resolves_newest_stable_system_image() {
    let d = sys_img().resolve(&system_image_path("36", "google_apis", "arm64-v8a"), HOST).expect("resolve");
    // The beta r8 is newer but not stable.
    assert_eq!(d.revision, Revision { major: 7, ..Revision::default() });
    assert_eq!(d.url, format!("{SYS_IMG_BASE}arm64-v8a-36_r07.zip"));
    assert_eq!(d.size, 1_872_691_175);
    assert_eq!(d.sha1, "5a99183b6d924da606260e45fd41a3eb8eca6eb7");
    assert_eq!(d.api_level.as_deref(), Some("36"));
    assert_eq!(d.install_dir(), PathBuf::from("system-images/android-36/google_apis/arm64-v8a"));
    assert_eq!(d.file_name(), "arm64-v8a-36_r07.zip");

    // Duplicate stable entries: the higher revision wins regardless of order.
    let old = sys_img().resolve("system-images;android-24;google_apis;arm64-v8a", HOST).expect("resolve 24");
    assert_eq!(old.revision.major, 29);
}

#[test]
fn resolution_errors() {
    let m = sys_img();
    // Only a canary build exists.
    let e = m.resolve(&system_image_path("CANARY", "google_apis", "arm64-v8a"), HOST).expect_err("canary only");
    assert!(matches!(e, Error::NoArchive(..)), "{e}");
    assert!(e.to_string().contains("no stable archive") && e.to_string().contains("macosx/aarch64"), "{e}");
    let e = m.resolve(&system_image_path("99", "google_apis", "arm64-v8a"), HOST).expect_err("unknown");
    assert!(matches!(e, Error::UnknownPackage(..)), "{e}");
    // An x86-only archive is not for this host.
    assert!(matches!(repo().resolve("x86-only", HOST), Err(Error::NoArchive(..))));
}

#[test]
fn resolves_platform_tools_for_host() {
    let d = repo().resolve("platform-tools", HOST).expect("resolve");
    // The dev-channel 38.0.0-rc1 is skipped; the darwin archive is picked over linux/windows.
    assert_eq!(d.revision.to_string(), "37.0.3");
    assert_eq!(d.url, "https://dl.google.com/android/repository/platform-tools_r37.0.3-darwin.zip");
    assert_eq!(d.size, 16_597_626);
    assert_eq!(d.sha1, "d48920e2650321710b4d15d3acd9e277d59cd55b", "checksums are normalised to lowercase");
    assert_eq!(d.install_dir(), PathBuf::from("platform-tools"));

    // host-arch matters when present (and absolute archive URLs are kept as-is).
    let ndk = repo().resolve("ndk;23.0.7421159", HOST).expect("ndk");
    assert_eq!(ndk.url, "https://example.invalid/android-ndk-r23-darwin-aarch64.zip");
    let intel = Host { os: "macosx", arch: "x64" };
    assert!(repo().resolve("ndk;23.0.7421159", intel).expect("ndk x64").url.ends_with("darwin-x86_64.zip"));
    assert!(repo().resolve("platform-tools", Host { os: "linux", arch: "x64" }).expect("linux").url.ends_with("-linux.zip"));
}

#[test]
fn lists_system_images_for_host() {
    let m = sys_img();
    let all: Vec<String> = m.system_images(HOST, None, "arm64-v8a").into_iter().map(|d| format!("{} r{}", d.path, d.revision)).collect();
    assert_eq!(
        all,
        [
            "system-images;android-24;google_apis;arm64-v8a r29",
            "system-images;android-36;google_apis;arm64-v8a r7",
            "system-images;android-36;google_apis_ps16k;arm64-v8a r7",
        ]
    );
    let tagged = m.system_images(HOST, Some("google_apis"), "arm64-v8a");
    assert_eq!(tagged.len(), 2);
    let x86 = m.system_images(HOST, None, "x86_64");
    assert_eq!(x86.len(), 1);
    assert_eq!(x86[0].file_name(), "x86_64-36_r07.zip");

    // Platforms order numerically, extension and minor releases after their base.
    let mut platforms = ["android-CANARY", "android-37.0", "android-36.1", "android-36-ext19", "android-9", "android-36"];
    let key = |p: &&str| (SysImagePath { platform: p, tag: "t", abi: "a" }).api_key();
    platforms.sort_by_key(key);
    assert_eq!(platforms, ["android-9", "android-36", "android-36-ext19", "android-36.1", "android-37.0", "android-CANARY"]);
}

#[test]
fn lists_android_xr_images_by_path_or_package_tag() {
    let m =
        Manifest::parse(include_str!("../testdata/sys-img-google_xr.xml"), "https://dl.google.com/android/repository/sys-img/google_xr/")
            .expect("parse XR fixture");
    let paths =
        |tag| m.system_images(HOST, tag, "arm64-v8a").into_iter().map(|d| format!("{} r{}", d.path, d.revision)).collect::<Vec<_>>();
    // The dev-channel r8 of google-xr isn't stable.
    let all = [
        "system-images;android-34;android-xr-v3-playstore;arm64-v8a r3",
        "system-images;android-34;google-xr;arm64-v8a r7",
        "system-images;android-36;ai-glasses;arm64-v8a r6",
    ];
    assert_eq!(paths(None), all);
    // No path says `android-xr`: the headsets carry it as a package tag.
    assert_eq!(paths(Some("android-xr")), all[..2]);
    assert_eq!(paths(Some("google-xr")), all[1..2]);
    assert_eq!(paths(Some("ai-glasses")), all[2..]);
    let d = m.resolve("system-images;android-34;google-xr;arm64-v8a", HOST).expect("google-xr");
    assert_eq!(
        (d.tags.as_slice(), d.file_name()),
        (["android-xr".to_owned(), "google_apis_playstore".to_owned()].as_slice(), "xr-arm64-v8a-playstore-34_r07.zip")
    );
}

#[test]
fn revisions_order_and_print() {
    let r = |major, minor, micro, preview| Revision { major, minor, micro, preview };
    assert!(r(38, 0, 0, Some(1)) < r(38, 0, 0, None));
    assert!(r(37, 0, 3, None) < r(38, 0, 0, Some(1)));
    assert_eq!(r(7, 0, 0, None).to_string(), "7");
    assert_eq!(r(38, 0, 0, Some(2)).to_string(), "38.0.0-rc2");
}

#[test]
fn site_list_and_urls() {
    let xml = r#"<common:site-list xmlns:sdk="http://schemas.android.com/sdk/android/addons-list/5" xmlns:common="http://schemas.android.com/repository/android/sites-common/1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <site xsi:type="sdk:sysImgSiteType"><displayName>Android System Images</displayName><url>sys-img/android/sys-img2-3.xml</url></site>
  <site xsi:type="sdk:addonSiteType"><displayName>Some add-on</displayName><url>addon2-3.xml</url></site>
  <site xsi:type="sdk:sysImgSiteType"><displayName>Google APIs</displayName><url>sys-img/google_apis/sys-img2-3.xml</url></site>
</common:site-list>"#;
    assert_eq!(
        parse_site_list(xml, REPO_URL).expect("sites"),
        [format!("{REPO_URL}sys-img/android/sys-img2-3.xml"), format!("{REPO_URL}sys-img/google_apis/sys-img2-3.xml")]
    );
    assert_eq!(sys_img_url("default"), format!("{REPO_URL}sys-img/android/sys-img2-3.xml"));
    assert_eq!(sys_img_url("google_apis_ps16k"), format!("{REPO_URL}sys-img/google_apis/sys-img2-3.xml"));
    assert_eq!(sys_img_url("android-tv"), format!("{REPO_URL}sys-img/android-tv/sys-img2-3.xml"));
    for xr in ["google-xr", "android-xr", "android-xr-v3-playstore", "android-xr-preview-playstore", "ai-glasses"] {
        assert_eq!(sys_img_url(xr), format!("{REPO_URL}sys-img/google_xr/sys-img2-3.xml"), "{xr}");
    }
    assert_eq!(system_image_path("android-36.1", "google_apis", "arm64-v8a"), "system-images;android-36.1;google_apis;arm64-v8a");
    assert!(Manifest::parse("<not xml", REPO_URL).is_err());
}

#[test]
fn sha1_known_values() {
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    let dir = scratch("sha1");
    let f = dir.join("big");
    // Spans several read buffers.
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&f, &data).expect("write");
    assert_eq!(sha1_file(&f).expect("sha1"), sha1_hex(&data));
    let _ = std::fs::remove_dir_all(dir);
}

/// The whole install path, offline: curl fetches a `file://` URL, the archive is verified,
/// unpacked and moved into place. Needs macOS's `/usr/bin/zip` to build the archive.
#[test]
fn installs_from_a_local_archive() {
    if !Path::new("/usr/bin/zip").is_file() || !Path::new("/usr/bin/curl").is_file() || !Path::new("/usr/bin/unzip").is_file() {
        return;
    }
    let work = scratch("install");
    let src = work.join("src/platform-tools");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("source.properties"), "Pkg.Revision=37.0.3\n").expect("write");
    std::fs::write(src.join("adb"), "#!/bin/sh\necho fake adb\n").expect("write");
    let zip = work.join("platform-tools_r37.0.3-darwin.zip");
    let ok = Command::new("/usr/bin/zip").current_dir(work.join("src")).arg("-qr").arg(&zip).arg("platform-tools").status().expect("zip");
    assert!(ok.success());
    let bytes = std::fs::read(&zip).expect("read zip");

    let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
    sdk.quiet = true;
    let good = Download {
        path: "platform-tools".into(),
        revision: Revision { major: 37, minor: 0, micro: 3, preview: None },
        api_level: None,
        tags: Vec::new(),
        url: format!("file://{}", zip.display()),
        size: bytes.len() as u64,
        sha1: sha1_hex(&bytes),
    };

    // A bad checksum is refused and the partial download is discarded.
    let bad = Download { sha1: "0".repeat(40), ..good.clone() };
    let e = sdk.install(&bad).expect_err("sha1 mismatch");
    assert!(matches!(e, Error::Verify { what: "sha1", .. }), "{e}");
    assert!(!sdk.downloads().join("platform-tools_r37.0.3-darwin.zip.part").exists());
    assert!(!sdk.is_installed(Path::new("platform-tools")));

    let dest = sdk.install(&good).expect("install");
    assert_eq!(dest, sdk.root().join("platform-tools"));
    assert!(sdk.is_installed(Path::new("platform-tools")));
    assert!(dest.join("adb").is_file());
    assert!(!sdk.downloads().join("platform-tools_r37.0.3-darwin.zip.part").exists(), "archive removed after install");
    let leftovers: Vec<_> = std::fs::read_dir(sdk.downloads()).expect("ls").flatten().collect();
    assert!(leftovers.is_empty(), "no temp dirs left: {leftovers:?}");

    // Manifests are fetched once, then served from the cache.
    let mf = work.join("repository2-3.xml");
    std::fs::write(&mf, REPO).expect("write manifest");
    let url = format!("file://{}", mf.display());
    let m = sdk.manifest(&url).expect("manifest");
    assert_eq!(m.base_url, format!("file://{}/", work.display()));
    std::fs::remove_file(&mf).expect("rm");
    assert_eq!(sdk.manifest(&url).expect("cached").packages.len(), m.packages.len());
    let _ = std::fs::remove_dir_all(work);
}

#[test]
fn resolves_emulator_for_host() {
    let d = repo().resolve(renderer::PACKAGE, HOST).expect("resolve emulator");
    // The dev-channel 37.4.1 is skipped; of the stable ones the newest wins, darwin/aarch64 archive.
    assert_eq!(d.revision.to_string(), "37.3.10");
    assert_eq!(d.url, "https://dl.google.com/android/repository/emulator-darwin_aarch64-16970787.zip");
    assert_eq!(d.size, 420_629_851);
    assert_eq!(d.sha1, "3ec739595d44092e835f006d806dd380bc8d6dd7");
    assert_eq!(renderer::dir(Path::new("/sdk"), d.revision), PathBuf::from("/sdk/emu-rs/renderer/37.3.10"));
    let intel = Host { os: "macosx", arch: "x64" };
    assert!(repo().resolve(renderer::PACKAGE, intel).expect("x64").url.ends_with("darwin_x64-16970787.zip"));
}

#[test]
fn revisions_parse() {
    for s in ["37.3.10", "7", "38.0.0-rc2", "36.1.0"] {
        let r: Revision = s.parse().expect(s);
        assert_eq!(r.to_string().parse::<Revision>().expect("round trip"), r, "{s}");
    }
    assert_eq!("37.3".parse::<Revision>().expect("two parts"), Revision { major: 37, minor: 3, micro: 0, preview: None });
    assert_eq!("38.0.0-rc2".parse::<Revision>().expect("rc").preview, Some(2));
    for bad in ["", "current", "37.x", "1.2.3.4", "37-rc", ".1"] {
        assert!(bad.parse::<Revision>().is_err(), "{bad}");
    }
    // Numeric, not lexicographic.
    assert!("37.3.9".parse::<Revision>().expect("a") < "37.3.10".parse::<Revision>().expect("b"));
}

#[test]
fn renderer_unzip_patterns() {
    let args: Vec<String> =
        renderer::unzip_args(Path::new("/dl/e.zip.part"), Path::new("/tmp/u")).iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(
        args,
        [
            "-q",
            "/dl/e.zip.part",
            "emulator/lib64/*",
            "emulator/LICENSE",
            "emulator/NOTICE.txt",
            "-x",
            "emulator/lib64/qt/*",
            "-d",
            "/tmp/u"
        ]
    );
    let m = renderer::Marker { revision: "37.3.10".parse().expect("rev"), sha1: "ab".repeat(20), url: "https://x/e.zip".into() };
    assert_eq!(m.to_string().parse::<renderer::Marker>().expect("marker round trip"), m);
    assert!("Pkg.Revision=37\n".parse::<renderer::Marker>().is_err());
}

/// A fake renderer install: `<sdk>/emu-rs/renderer/<name>` with a marker for `rev` (if any).
fn fake_renderer(sdk: &Path, name: &str, rev: Option<&str>, backend: bool) -> PathBuf {
    let dir = renderer::root(sdk).join(name);
    std::fs::create_dir_all(dir.join("lib64")).expect("mkdir");
    if backend {
        std::fs::write(dir.join("lib64").join(renderer::BACKEND), b"").expect("write");
    }
    if let Some(rev) = rev {
        let m = renderer::Marker { revision: rev.parse().expect("rev"), sha1: "0".repeat(40), url: "file:///e.zip".into() };
        std::fs::write(dir.join(renderer::MARKER), m.to_string()).expect("marker");
    }
    dir
}

#[test]
fn renderer_lookup_preference() {
    let sdk = scratch("lookup");
    assert_eq!(renderer::find_lib64(&sdk), None);
    fake_renderer(&sdk, "37.3.9", Some("37.3.9"), true);
    let newest = fake_renderer(&sdk, "37.3.10", Some("37.3.10"), true);
    // Not complete installs: no marker, a marker for another revision, no backend, a stray name.
    fake_renderer(&sdk, "37.4.0", None, true);
    fake_renderer(&sdk, "37.5.0", Some("37.3.9"), true);
    fake_renderer(&sdk, "38.0.0", Some("38.0.0"), false);
    fake_renderer(&sdk, "junk", Some("39"), true);
    std::os::unix::fs::symlink("37.3.9", renderer::root(&sdk).join(renderer::CURRENT)).expect("symlink");
    let revs: Vec<String> = renderer::installed(&sdk).iter().map(|i| i.marker.revision.to_string()).collect();
    assert_eq!(revs, ["37.3.10", "37.3.9"]);
    assert_eq!(renderer::find_lib64(&sdk), Some(newest.join("lib64")));

    // An emulator package without the renderer doesn't count...
    let emu_lib64 = sdk.join("emulator/lib64");
    std::fs::create_dir_all(&emu_lib64).expect("mkdir");
    assert_eq!(renderer::find_lib64(&sdk), Some(newest.join("lib64")));
    // ...one with it wins over emu-rs's own copies.
    std::fs::write(emu_lib64.join(renderer::BACKEND), b"").expect("write");
    assert_eq!(renderer::sdk_emulator_lib64(&sdk), Some(emu_lib64.clone()));
    assert_eq!(renderer::find_lib64(&sdk), Some(emu_lib64));
    let _ = std::fs::remove_dir_all(sdk);
}

/// Zips `<work>/<src>/emulator` with `/usr/bin/zip -y` (symlinks stored as links) and returns its
/// `Download` for revision `rev`.
fn zip_emulator(work: &Path, src: &str, rev: &str) -> Download {
    let zip = work.join(format!("emulator-darwin_aarch64-{src}.zip"));
    let _ = std::fs::remove_file(&zip);
    let ok = Command::new("/usr/bin/zip").current_dir(work.join(src)).arg("-qry").arg(&zip).arg("emulator").status().expect("zip");
    assert!(ok.success());
    let bytes = std::fs::read(&zip).expect("read zip");
    Download {
        path: renderer::PACKAGE.into(),
        revision: rev.parse().expect("rev"),
        api_level: None,
        tags: Vec::new(),
        url: format!("file://{}", zip.display()),
        size: bytes.len() as u64,
        sha1: sha1_hex(&bytes),
    }
}

/// A tree shaped like the emulator package under `<work>/<src>/emulator`.
fn fake_emulator_tree(work: &Path, src: &str, backend: bool) {
    let e = work.join(src).join("emulator");
    for d in ["lib64/qt/lib", "lib64/vulkan/layers", "lib64/gles_angle", "lib", "bin64"] {
        std::fs::create_dir_all(e.join(d)).expect("mkdir");
    }
    for (f, body) in [
        ("emulator", "#!/bin/sh\n"),
        ("source.properties", "Pkg.Path=emulator\n"),
        ("LICENSE", "license\n"),
        ("NOTICE.txt", "notice\n"),
        ("lib/advancedFeatures.ini", "x\n"),
        ("bin64/qemu-img", "x\n"),
        ("lib64/libemugl_common.dylib", "x"),
        ("lib64/libprotobuf.28.4.0.dylib", "protobuf"),
        ("lib64/qt/lib/QtCore.dylib", "qt"),
        ("lib64/vulkan/libvulkan.dylib", "x"),
        ("lib64/vulkan/MoltenVK_icd.json", "{}"),
        ("lib64/vulkan/layers/VkLayer_khronos_validation.json", "{}"),
        ("lib64/gles_angle/libEGL.dylib", "x"),
    ] {
        std::fs::write(e.join(f), body).expect("write");
    }
    if backend {
        std::fs::write(e.join("lib64").join(renderer::BACKEND), src).expect("write");
    }
    std::os::unix::fs::symlink("libprotobuf.28.4.0.dylib", e.join("lib64/libprotobuf.dylib")).expect("symlink");
}

/// Files under `dir`, relative, sorted; symlinks as `name -> target`.
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("ls").flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).expect("prefix").to_string_lossy().into_owned();
            let t = e.file_type().expect("type");
            if t.is_symlink() {
                out.push(format!("{rel} -> {}", std::fs::read_link(&p).expect("readlink").display()));
            } else if t.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The renderer install path, offline: only lib64 (minus Qt) and the license files are
/// unpacked, symlinks survive, a marker and `current` are written, the archive is deleted, and a
/// newer revision replaces the older one.
#[test]
fn installs_renderer_from_a_local_archive() {
    if !Path::new("/usr/bin/zip").is_file() || !Path::new("/usr/bin/curl").is_file() || !Path::new("/usr/bin/unzip").is_file() {
        return;
    }
    let work = scratch("renderer");
    let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
    sdk.quiet = true;
    let current = renderer::root(sdk.root()).join(renderer::CURRENT);

    fake_emulator_tree(&work, "r1", true);
    let d = zip_emulator(&work, "r1", "37.3.10");
    let inst = sdk.install_renderer(&d).expect("install renderer");
    assert_eq!(inst.dir, sdk.root().join("emu-rs/renderer/37.3.10"));
    assert_eq!((inst.marker.revision, inst.marker.sha1.as_str(), inst.marker.url.as_str()), (d.revision, d.sha1.as_str(), d.url.as_str()));
    assert_eq!(
        tree(&inst.dir),
        [
            "LICENSE",
            "NOTICE.txt",
            "lib64/gles_angle/libEGL.dylib",
            "lib64/libemugl_common.dylib",
            "lib64/libgfxstream_backend.dylib",
            "lib64/libprotobuf.28.4.0.dylib",
            "lib64/libprotobuf.dylib -> libprotobuf.28.4.0.dylib",
            "lib64/vulkan/MoltenVK_icd.json",
            "lib64/vulkan/layers/VkLayer_khronos_validation.json",
            "lib64/vulkan/libvulkan.dylib",
            "renderer.properties",
        ]
    );
    // Nothing that would make sdkmanager think an `emulator` package lives here.
    assert!(!inst.dir.join("source.properties").exists() && !sdk.root().join("emulator").exists());
    assert_eq!(std::fs::read_link(&current).expect("current"), PathBuf::from("37.3.10"));
    assert_eq!(renderer::find_lib64(sdk.root()), Some(inst.lib64()));
    assert!(renderer::disk_usage(&inst.dir) > 0);
    let leftovers: Vec<_> = std::fs::read_dir(sdk.downloads()).expect("ls").flatten().collect();
    assert!(leftovers.is_empty(), "archive and temp dirs removed: {leftovers:?}");

    // A newer revision takes over `current` and the old one is removed.
    fake_emulator_tree(&work, "r2", true);
    let inst2 = sdk.install_renderer(&zip_emulator(&work, "r2", "37.3.11")).expect("install newer");
    assert_eq!(renderer::installed(sdk.root()), std::slice::from_ref(&inst2));
    assert!(!inst.dir.exists());
    assert_eq!(std::fs::read_link(&current).expect("current"), PathBuf::from("37.3.11"));
    assert_eq!(std::fs::read_to_string(current.join("lib64").join(renderer::BACKEND)).expect("backend via current"), "r2");

    // An emulator archive without the renderer is refused and nothing changes.
    fake_emulator_tree(&work, "r3", false);
    assert!(matches!(sdk.install_renderer(&zip_emulator(&work, "r3", "40")), Err(Error::NoRenderer(..))));
    assert_eq!(renderer::installed(sdk.root()), [inst2]);
    assert!(!renderer::dir(sdk.root(), "40".parse().expect("rev")).exists());
    assert_eq!(std::fs::read_dir(sdk.downloads()).expect("ls").count(), 0, "useless archive deleted");
    let _ = std::fs::remove_dir_all(work);
}

#[test]
fn bare_api_levels_fall_back_to_the_newest_extension_level() {
    let m = Manifest::parse(
        include_str!("../testdata/sys-img-android-automotive.xml"),
        "https://dl.google.com/android/repository/sys-img/android-automotive/",
    )
    .expect("parse Automotive fixture");
    let path = |api: &str| m.resolve_system_image(api, "android-automotive", "arm64-v8a", HOST).map(|d| d.path);
    // Automotive has no plain API 34 or 35 image, only extension levels.
    assert_eq!(path("34").expect("34"), "system-images;android-34-ext9;android-automotive;arm64-v8a");
    assert_eq!(path("android-35").expect("35"), "system-images;android-35-ext15;android-automotive;arm64-v8a");
    assert_eq!(path("33").expect("33"), "system-images;android-33;android-automotive;arm64-v8a", "exact match first");
    assert_eq!(path("34-ext9").expect("explicit"), "system-images;android-34-ext9;android-automotive;arm64-v8a");
    assert!(matches!(path("34-ext8"), Err(Error::UnknownPackage(..))), "an explicit extension level is never swapped");
    assert!(matches!(path("3"), Err(Error::UnknownPackage(..))), "34-ext9 isn't an extension of 3");
    assert!(matches!(path("36"), Err(Error::UnknownPackage(..))), "36.1 is a dev-channel preview, not 36");
    let tv = m.resolve_system_image("34", "android-tv", "arm64-v8a", HOST);
    assert!(matches!(tv, Err(Error::UnknownPackage(..))), "only the asked tag");
}

#[test]
fn the_current_host_matches_the_build_target() {
    let host = Host::current();
    #[cfg(target_os = "linux")]
    assert_eq!(host.os, "linux");
    #[cfg(target_os = "macos")]
    assert_eq!(host.os, "macosx");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    assert_eq!(host, Host::MACOS_ARM64);
    assert!(repo().resolve("platform-tools", Host { os: "linux", arch: "x64" }).is_ok());
}
