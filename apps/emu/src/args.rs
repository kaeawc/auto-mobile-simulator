//! The SDK emulator's command line: single-dash long flags, some taking a value.
use anyhow::{Result, bail};

pub const USAGE: &str = "\
usage: emu -avd NAME [options]
       emu @NAME [options]
       emu -avd NAME -dry-run   (print the boot plan without booting; any host)
       emu -list-avds | -version | -accel-check
       emu -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a [-lcd 720x1280@320] [-cores N] [-memory MB]
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
  -show-kernel      print the guest console
  -verbose          diagnostics, including vCPU exit counts at shutdown
  -no-boot-anim     skip the boot animation
  -disk-cache MODE  unsafe (default: guest flushes stay in the host page cache) or fsync
  -gpu MODE         off disables host rendering; swiftshader/lavapipe pick software Vulkan
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
    "netdelay",
    "netspeed",
    "dns-server",
    "http-proxy",
    "timezone",
    "logcat",
    "shell-serial",
    "prop",
    "datadir",
    "sdcard",
    "partition-size",
    "snapshot",
    "id",
    "accel",
    "selinux",
    "audio",
    "screen",
    "grpc-use-token",
    "grpc-use-jwt",
    "metrics-collection",
    "metrics-to-file",
    "idle-grpc-timeout",
    "allow-host-audio",
    "radio",
    "encryption-key",
    "cache",
    "cache-size",
    "logcat-output",
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
    pub verbose: bool,
    pub no_boot_anim: bool,
    pub no_window: bool,
    /// `-gpu MODE`: `off`/`none` disables host rendering; anything else uses gfxstream.
    pub gpu: Option<String>,
    /// `-disk-cache unsafe|fsync`: whether guest disk flushes fsync on the host.
    pub disk_fsync: bool,
    pub create_avd: Option<String>,
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
            "verbose" | "debug-init" => o.verbose = true,
            "no-boot-anim" => o.no_boot_anim = true,
            "no-window" => o.no_window = true,
            "gpu" => o.gpu = Some(value(flag)?),
            "disk-cache" => {
                o.disk_fsync = match value(flag)?.as_str() {
                    "fsync" | "writethrough" | "safe" => true,
                    "unsafe" | "writeback" | "none" => false,
                    other => bail!("-disk-cache takes unsafe or fsync, not '{other}'"),
                }
            }
            "create-avd" => o.create_avd = Some(value(flag)?),
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
    fn harness_launch_line() {
        let o = p("-avd pixel -no-window -no-audio -no-snapshot-save -gpu swiftshader_indirect -grpc 8554 -port 5570 -no-boot-anim");
        assert_eq!(o.avd.as_deref(), Some("pixel"));
        assert_eq!(o.port, Some(5570));
        assert!(o.no_window && o.no_boot_anim);
        assert_eq!(o.gpu.as_deref(), Some("swiftshader_indirect"));
        assert!(o.ignored.iter().any(|i| i == "-grpc 8554"));
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
    fn missing_value_is_an_error() {
        assert!(parse(["-port".to_string()]).is_err());
        assert!(parse(["stray".to_string()]).is_err());
    }
}
