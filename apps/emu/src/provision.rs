//! `-provision`, `-provision-renderer` and `-list-images`: install what emu-rs needs
//! (platform-tools, a system image and the gfxstream renderer) straight from Google's
//! repository, with no Java, sdkmanager or avdmanager.
use std::path::PathBuf;

use anyhow::{Result, bail};
use emu_avd::Env;
use emu_sdk::{Host, Sdk, renderer};

use crate::args::Opts;

const HOST: Host = Host::MACOS_ARM64;
const DEFAULT_TAG: &str = "google_apis";
const DEFAULT_ABI: &str = "arm64-v8a";

/// The SDK to provision: the resolved SDK root, else the platform default; created if missing.
fn sdk(env: &Env) -> Result<Sdk> {
    let root = env.sdk_root.clone().unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        home.join("Library/Android/sdk")
    });
    Ok(Sdk::new(root)?)
}

pub fn list_images(env: &Env, opts: &Opts) -> Result<()> {
    let sdk = sdk(env)?;
    let abi = opts.abi.as_deref().unwrap_or(DEFAULT_ABI);
    println!("{:<16} {:<28} {:>5} {:>9}  installed", "api", "tag", "rev", "size");
    let mut n = 0;
    for m in sdk.system_image_manifests(opts.tag.as_deref())? {
        for d in m.system_images(HOST, opts.tag.as_deref(), abi) {
            let Some(p) = emu_sdk::SysImagePath::parse(&d.path) else { continue };
            let api = p.api();
            let have = if sdk.is_installed(&d.install_dir()) { "yes" } else { "" };
            println!("{api:<16} {:<28} {:>5} {:>9}  {have}", p.tag, d.revision.to_string(), emu_sdk::format_size(d.size));
            n += 1;
        }
    }
    if n == 0 {
        bail!("no stable {abi} system images for {HOST}{}", opts.tag.as_ref().map(|t| format!(" with tag {t}")).unwrap_or_default());
    }
    Ok(())
}

pub fn provision(env: &Env, api: &str, opts: &Opts) -> Result<()> {
    let sdk = sdk(env)?;
    let tag = opts.tag.as_deref().unwrap_or(DEFAULT_TAG);
    let abi = opts.abi.as_deref().unwrap_or(DEFAULT_ABI);
    let image = emu_sdk::system_image_path(api, tag, abi);
    // Refuse before a multi-gigabyte download, not after.
    if let Some(name) = &opts.create_avd
        && (env.avd_home.join(format!("{name}.avd")).exists() || env.avd_home.join(format!("{name}.ini")).exists())
    {
        bail!("AVD '{name}' already exists");
    }
    println!("SDK: {}", sdk.root().display());

    for (url, path) in [(emu_sdk::repository_url(), "platform-tools"), (emu_sdk::sys_img_url(tag), image.as_str())] {
        let dir = emu_sdk::install_dir(path);
        let installed = sdk.is_installed(&dir);
        if installed && !opts.dry_run {
            println!("have {path}");
            continue;
        }
        let d = match sdk.manifest(&url)?.resolve(path, HOST) {
            Err(e @ emu_sdk::Error::UnknownPackage(..)) => bail!("{e} (`emu -list-images -tag {tag}` shows what is available)"),
            r => r?,
        };
        if opts.dry_run {
            println!("{path} r{}{}", d.revision, if installed { " (installed)" } else { "" });
            println!("  url  {}", d.url);
            println!("  size {} ({})", d.size, emu_sdk::format_size(d.size));
            println!("  sha1 {}", d.sha1);
            println!("  into {}", sdk.root().join(&dir).display());
            continue;
        }
        println!("installing {path} r{}", d.revision);
        let dest = sdk.install(&d)?;
        println!("installed {path} r{} at {}", d.revision, dest.display());
    }
    if !opts.no_renderer {
        renderer(&sdk, opts, opts.provision_renderer)?;
    }

    if let Some(name) = &opts.create_avd {
        let sysdir = emu_sdk::install_dir(&image).to_string_lossy().into_owned();
        if opts.dry_run {
            println!("would create AVD {name} from {sysdir}");
        } else {
            let env = Env { sdk_root: Some(sdk.root().to_path_buf()), ..env.clone() };
            crate::create_avd(&env, name, &sysdir, opts)?;
        }
    }
    Ok(())
}

/// `-provision-renderer`: install (or update to) the newest renderer.
pub fn provision_renderer(env: &Env, opts: &Opts) -> Result<()> {
    let sdk = sdk(env)?;
    println!("SDK: {}", sdk.root().display());
    renderer(&sdk, opts, true)
}

/// Installs the gfxstream renderer out of the `emulator` package. With `update` the newest
/// stable revision is installed unless it already is; without it (plain `-provision`) any
/// renderer the SDK already has will do, including the SDK's own emulator package.
fn renderer(sdk: &Sdk, opts: &Opts, update: bool) -> Result<()> {
    let root = sdk.root();
    let sdk_package = renderer::sdk_emulator_lib64(root);
    let installed = renderer::installed(root);
    let existing = sdk_package.clone().or_else(|| installed.first().map(renderer::Installed::lib64));
    if !update
        && !opts.dry_run
        && let Some(lib64) = &existing
    {
        println!("have renderer ({})", lib64.display());
        return Ok(());
    }
    let d = sdk.manifest(&emu_sdk::repository_url())?.resolve(renderer::PACKAGE, HOST)?;
    let have = installed.iter().find(|i| i.marker.revision == d.revision);
    if opts.dry_run {
        let state = match (have, &existing) {
            (Some(_), _) => " (installed)".to_string(),
            (None, Some(lib64)) if !update => format!(" (not needed: have {})", lib64.display()),
            _ => String::new(),
        };
        println!("renderer: {} r{}{state}", renderer::PACKAGE, d.revision);
        println!("  url  {}", d.url);
        println!("  size {} ({})", d.size, emu_sdk::format_size(d.size));
        println!("  sha1 {}", d.sha1);
        println!("  only {} (not {})", renderer::INCLUDE.join(" "), renderer::EXCLUDE.join(" "));
        println!("  into {}", renderer::dir(root, d.revision).join("lib64").display());
        return Ok(());
    }
    if let Some(i) = have {
        println!("have renderer r{} ({})", d.revision, i.lib64().display());
        return Ok(());
    }
    if let Some(lib64) = &sdk_package {
        println!("note: the SDK's emulator package ({}) stays the renderer emu uses while it is installed", lib64.display());
    }
    println!("installing renderer r{} (lib64 of the {} package)", d.revision, renderer::PACKAGE);
    let inst = sdk.install_renderer(&d)?;
    println!(
        "installed renderer r{} at {} ({} on disk, from a {} archive)",
        d.revision,
        inst.lib64().display(),
        emu_sdk::format_size(renderer::disk_usage(&inst.dir)),
        emu_sdk::format_size(d.size)
    );
    Ok(())
}
