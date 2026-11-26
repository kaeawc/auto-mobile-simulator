//! The SDK emulator's command line: single-dash long flags, some taking a value.
use anyhow::{Result, bail};

pub const USAGE: &str = "\
usage: emu -avd NAME [options]
       emu @NAME [options]
       emu -avd NAME -dry-run   (print the boot plan without booting; any host)
       emu -list-avds | -version | -accel-check
       emu -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a [-lcd 720x1280@320] [-cores N] [-memory MB]
       emu -clone-avd NAME -from GOLDEN   (copy-on-write clone of a stopped AVD's settled simee disks)
       emu -delete-avd NAME               (refused while any emulator, simee or stock, runs it)
       emu -provision API [-tag google_apis] [-abi arm64-v8a] [-no-renderer] [-dry-run] [-create-avd NAME [-lcd WxH@DPI] [-cores N] [-memory MB]]
       emu -provision-renderer [-dry-run]
       emu -list-images [-tag TAG] [-abi arm64-v8a]

provisioning (no Java or sdkmanager; SDK at $ANDROID_HOME, default ~/Library/Android/sdk):
  -provision API    install platform-tools, system-images;android-API;TAG;ABI (API: 36, 36.1, ...)
                    and the gfxstream renderer (unless the SDK already has one)
  -provision-renderer
                    install or update just the renderer: lib64 of the emulator package, into
                    $ANDROID_HOME/simee/renderer/<revision>
  -no-renderer      with -provision: skip the renderer (emu then boots without graphics)
  -tag TAG          default, google_apis (default), google_apis_playstore, android-wear, ...
  -abi ABI          default arm64-v8a
  -dry-run          resolve and print URL, size and sha1 without downloading; with -avd, print
                    the boot plan (cmdline, bootconfig, boot props, disks, devices) instead of booting
  -list-images      system images available for this host (all tags unless -tag)

options:
  -cores N          vCPUs (default: hw.cpu.ncore)
  -memory MB        guest RAM (default: hw.ramSize)
  -port N           console port; adb uses N+1
  -read-only        keep every disk write in memory (several instances per AVD)
  -wipe-data        reset user data before booting
  -show-kernel      print the guest console, every kernel and init message (without it the
                    console, <avd>/simee/console.log, gets only critical ones and panics, like
                    stock; EMU_CONSOLE=full logs everything there without printing it)
  -logcat TAGS      stream the guest's logcat (filter TAGS, e.g. '*:I') to stdout
  -logcat-output F  stream it to file F instead (all of it unless -logcat; F rolls over to F.1
                    at 4 MiB)
  -verbose          diagnostics, including vCPU exit counts at shutdown
  -no-boot-anim     skip the boot animation
  -disk-cache MODE  unsafe (default: guest flushes stay in the host page cache) or fsync
  -gpu MODE         rendering mode, default auto (host GPU through MoltenVK):
                      auto, host     host GPU through MoltenVK, software if that fails
                      lavapipe, swangle, angle_indirect
                                     software Vulkan (lavapipe): no GPU, CI, headless
                      swiftshader, swiftshader_indirect
                                     software Vulkan (SwiftShader)
                      software, guest
                                     lavapipe, with stock's notice: SDK images have no
                                     in-guest renderer, so guest rendering isn't possible
                      off, none      no renderer, no graphics
                    An AVD's hw.gpu.mode (guest, software, lavapipe, ...) applies when -gpu
                    isn't given; the flag wins.
  -no-audio         the sound card stays silent and no host audio device is opened (-audio none)
  -allow-host-audio the guest records from the host's default microphone (otherwise silence)
  -netspeed S       initial network speed: gsm, hscsd, gprs, umts, edge, hsdpa, lte, evdo, 5g,
                    full (default), <kbps> or <up>:<down>; `network status` reports it, traffic
                    isn't shaped yet
  -netdelay D       initial network latency: gsm, gprs, edge, umts, ..., none (default), <ms> or
                    <min>:<max>; reported the same way
Other SDK emulator flags are accepted.
";

/// Flags that take a value and that we don't act on (yet).
const IGNORED_WITH_VALUE: &[&str] = &[
    "ports",
    "grpc",
    "camera-back",
    "camera-front",
    "packet-streamer-endpoint",
    "feature",
    "virtualscene-poster",
    "skin",
    "skindir",
    "dns-server",
    "http-proxy",
    "timezone",
    "shell-serial",
    "prop",
    "datadir",
    "sdcard",
    "partition-size",
    "snapshot",
    "id",
    "accel",
    "selinux",
    "screen",
    "grpc-use-token",
    "grpc-use-jwt",
    "metrics-collection",
    "metrics-to-file",
    "idle-grpc-timeout",
    "radio",
    "encryption-key",
    "cache",
    "cache-size",
    "kernel",
    "ramdisk",
    "system",
    "vendor",
    "data",
    "initdata",
];

#[derive(Debug, Default)]
pub struct Opts {
    pub avd: Option<String>,
    pub list_avds: bool,
    pub version: bool,
    pub help: bool,
    pub accel_check: bool,
    pub cores: Option<u32>,
    pub memory_mb: Option<u64>,
    pub port: Option<u16>,
    pub read_only: bool,
    pub wipe_data: bool,
    pub show_kernel: bool,
    /// `-logcat TAGS`: the guest's logcat filter, streamed to stdout.
    pub logcat: Option<String>,
    /// `-logcat-output FILE`: the stream goes to FILE.
    pub logcat_output: Option<String>,
    pub verbose: bool,
    pub no_boot_anim: bool,
    pub no_window: bool,
    /// `-no-audio` (or `-audio none`): the sound card neither plays nor records on the host.
    pub no_audio: bool,
    /// `-allow-host-audio`: the sound card records from the host's microphone.
    pub allow_host_audio: bool,
    /// `-gpu MODE`: `off`/`none` disables host rendering; anything else uses gfxstream.
    pub gpu: Option<String>,
    /// `-netspeed`, `-netdelay` (`-netfast`: both unlimited, the default).
    pub network: sim_core::network::Conditions,
    /// `-disk-cache unsafe|fsync`: whether guest disk flushes fsync on the host.
    pub disk_fsync: bool,
    pub create_avd: Option<String>,
    /// `-clone-avd NAME -from GOLDEN`.
    pub clone_avd: Option<String>,
    pub from: Option<String>,
    pub delete_avd: Option<String>,
    pub sysdir: Option<String>,
    /// WxH@DPI
    pub lcd: Option<(u32, u32, u32)>,
    /// `-provision API`: install platform-tools, a system image and the renderer.
    pub provision: Option<String>,
    /// `-provision-renderer`: install or update only the gfxstream renderer.
    pub provision_renderer: bool,
    /// `-no-renderer`: `-provision` without the renderer.
    pub no_renderer: bool,
    pub list_images: bool,
    pub tag: Option<String>,
    pub abi: Option<String>,
    pub dry_run: bool,
    /// Flags accepted for compatibility but not implemented.
    pub ignored: Vec<String>,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if let Some(name) = arg.strip_prefix('@') {
            o.avd = Some(name.to_string());
            continue;
        }
        let Some(flag) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) else {
            bail!("unexpected argument '{arg}'");
        };
        let mut value = |flag: &str| it.next().ok_or_else(|| anyhow::anyhow!("-{flag} needs a value"));
        match flag {
            "avd" => o.avd = Some(value(flag)?),
            "list-avds" => o.list_avds = true,
            "version" => o.version = true,
            "help" | "h" => o.help = true,
            "accel-check" => o.accel_check = true,
            "cores" => o.cores = Some(value(flag)?.parse()?),
            "memory" => o.memory_mb = Some(value(flag)?.parse()?),
            "port" => o.port = Some(value(flag)?.parse()?),
            "read-only" => o.read_only = true,
            "wipe-data" => o.wipe_data = true,
            "show-kernel" => o.show_kernel = true,
            "logcat" => o.logcat = Some(value(flag)?),
            "logcat-output" => o.logcat_output = Some(value(flag)?),
            "verbose" | "debug-init" => o.verbose = true,
            "no-boot-anim" => o.no_boot_anim = true,
            "no-window" => o.no_window = true,
            "no-audio" => o.no_audio = true,
            "allow-host-audio" => o.allow_host_audio = true,
            // `-audio BACKEND`: QEMU's audio driver; only `none` means anything here.
            "audio" => o.no_audio |= value(flag)? == "none",
            "gpu" => {
                let mode = value(flag)?;
                emu_avd::gpu::parse_flag(&mode).map_err(|e| anyhow::anyhow!(e))?;
                o.gpu = Some(mode);
            }
            "netspeed" => {
                let v = value(flag)?;
                if !o.network.set_speed(&v) {
                    bail!("invalid -netspeed parameter '{v}'");
                }
            }
            "netdelay" => {
                let v = value(flag)?;
                if !o.network.set_latency(&v) {
                    bail!("invalid -netdelay parameter '{v}'");
                }
            }
            "netfast" => o.network = Default::default(),
            "disk-cache" => {
                o.disk_fsync = match value(flag)?.as_str() {
                    "fsync" | "writethrough" | "safe" => true,
                    "unsafe" | "writeback" | "none" => false,
                    other => bail!("-disk-cache takes unsafe or fsync, not '{other}'"),
                }
            }
            "create-avd" => o.create_avd = Some(value(flag)?),
            "clone-avd" => o.clone_avd = Some(value(flag)?),
            "from" => o.from = Some(value(flag)?),
            "delete-avd" => o.delete_avd = Some(value(flag)?),
            "sysdir" => o.sysdir = Some(value(flag)?),
            "lcd" => o.lcd = Some(parse_lcd(&value(flag)?)?),
            "provision" => o.provision = Some(value(flag)?),
            "provision-renderer" => o.provision_renderer = true,
            "no-renderer" => o.no_renderer = true,
            "list-images" => o.list_images = true,
            "tag" => o.tag = Some(value(flag)?),
            "abi" => o.abi = Some(value(flag)?),
            "dry-run" => o.dry_run = true,
            // Everything after -qemu belongs to QEMU, which we aren't.
            "qemu" => {
                let rest: Vec<String> = it.by_ref().collect();
                o.ignored.push(format!("-qemu {}", rest.join(" ")));
            }
            f if IGNORED_WITH_VALUE.contains(&f) => {
                let v = value(f)?;
                o.ignored.push(format!("-{f} {v}"));
            }
            f => o.ignored.push(format!("-{f}")),
        }
    }
    Ok(o)
}

fn parse_lcd(v: &str) -> Result<(u32, u32, u32)> {
    let (size, dpi) = v.split_once('@').unwrap_or((v, "320"));
    let (w, h) = size.split_once('x').ok_or_else(|| anyhow::anyhow!("-lcd wants WxH@DPI, got '{v}'"))?;
    Ok((w.parse()?, h.parse()?, dpi.parse()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Opts {
        parse(s.split_whitespace().map(String::from)).expect("parse")
    }

    #[test]
    fn gpu_modes_are_validated() {
        for m in ["guest", "software", "swangle", "off"] {
            assert_eq!(p(&format!("-gpu {m}")).gpu.as_deref(), Some(m));
        }
        assert!(parse("-gpu bogus".split_whitespace().map(String::from)).is_err());
        assert_eq!(p("-avd x").gpu, None);
    }

    #[test]
    fn harness_launch_line() {
        let o = p("-avd pixel -no-window -no-audio -no-snapshot-save -gpu swiftshader_indirect -grpc 8554 -port 5570 -no-boot-anim");
        assert_eq!(o.avd.as_deref(), Some("pixel"));
        assert_eq!(o.port, Some(5570));
        assert!(o.no_window && o.no_boot_anim && o.no_audio);
        assert_eq!(o.gpu.as_deref(), Some("swiftshader_indirect"));
        assert!(o.ignored.iter().any(|i| i == "-grpc 8554"));
    }

    #[test]
    fn audio_flags() {
        let o = p("-avd pixel -allow-host-audio -port 5570");
        assert!(o.allow_host_audio && !o.no_audio);
        assert_eq!(o.port, Some(5570), "-allow-host-audio takes no value");
        assert!(p("-avd pixel -audio none").no_audio);
        assert!(!p("-avd pixel -audio coreaudio").no_audio);
        assert!(!p("-avd pixel").no_audio && !p("-avd pixel").allow_host_audio);
    }

    #[test]
    fn at_name_and_qemu_tail() {
        let o = p("@pixel -cores 2 -qemu -m 2048 -smp 2");
        assert_eq!(o.avd.as_deref(), Some("pixel"));
        assert_eq!(o.cores, Some(2));
        assert_eq!(o.ignored, vec!["-qemu -m 2048 -smp 2".to_string()]);
    }

    #[test]
    fn create_avd_flags() {
        let o = p("-create-avd small -sysdir system-images/android-36/google_apis/arm64-v8a -lcd 480x800@240 -cores 2");
        assert_eq!(o.create_avd.as_deref(), Some("small"));
        assert_eq!(o.lcd, Some((480, 800, 240)));
        assert!(parse_lcd("480").is_err());
        let o = p("-clone-avd farm-1 -from golden");
        assert_eq!((o.clone_avd.as_deref(), o.from.as_deref()), (Some("farm-1"), Some("golden")));
        assert_eq!(p("-delete-avd farm-1").delete_avd.as_deref(), Some("farm-1"));
    }

    #[test]
    fn provision_flags() {
        let o =
            p("-provision 36 -tag google_apis_playstore -abi arm64-v8a -create-avd play36 -lcd 480x800@240 -cores 2 -memory 1536 -dry-run");
        assert_eq!(o.provision.as_deref(), Some("36"));
        assert_eq!(o.tag.as_deref(), Some("google_apis_playstore"));
        assert_eq!(o.abi.as_deref(), Some("arm64-v8a"));
        assert_eq!(o.create_avd.as_deref(), Some("play36"));
        assert_eq!((o.lcd, o.cores, o.memory_mb), (Some((480, 800, 240)), Some(2), Some(1536)));
        assert!(o.dry_run && !o.list_images && o.ignored.is_empty());
        assert!(!o.no_renderer && !o.provision_renderer);

        let o = p("-provision 36 -no-renderer");
        assert!(o.no_renderer && !o.provision_renderer && o.ignored.is_empty());
        let o = p("-provision-renderer -dry-run");
        assert!(o.provision_renderer && o.dry_run && o.provision.is_none() && o.ignored.is_empty());

        let o = p("-list-images -tag android-tv");
        assert!(o.list_images && o.provision.is_none());
        assert_eq!(o.tag.as_deref(), Some("android-tv"));
        assert!(parse(["-provision".to_string()]).is_err());
        assert!(parse(["-list-images".to_string(), "-tag".to_string()]).is_err());
    }

    #[test]
    fn network_flags_set_the_initial_conditions() {
        let o = p("-avd pixel -netspeed edge -netdelay 20:80 -port 5570");
        assert_eq!((o.network.upload_bps, o.network.download_bps), (473600.0, 473600.0));
        assert_eq!((o.network.min_latency_ms, o.network.max_latency_ms), (20, 80));
        assert!(o.ignored.is_empty());
        assert!(!p("-avd pixel -netspeed edge -netfast").network.is_shaped());
        assert!(!p("-avd pixel").network.is_shaped());
        assert!(parse(["-netspeed".to_string(), "warp".to_string()]).is_err());
        assert!(parse(["-netdelay".to_string(), "slow".to_string()]).is_err());
    }

    #[test]
    fn missing_value_is_an_error() {
        assert!(parse(["-port".to_string()]).is_err());
        assert!(parse(["stray".to_string()]).is_err());
    }
}
