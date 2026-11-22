//! Android Virtual Device discovery and configuration.
//!
//! Reads the same files the SDK tools write (`~/.android/avd/<name>.ini`,
//! `<name>.avd/config.ini`, the system image directory) so existing AVDs boot unchanged.

mod bootconfig;
mod ini;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use bootconfig::BootConfig;
pub use ini::parse_ini;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unknown AVD '{0}' (looked in {1})")]
    UnknownAvd(String, PathBuf),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))
}

/// Where the SDK and AVDs live, resolved like the SDK tools do.
#[derive(Clone, Debug)]
pub struct Env {
    pub sdk_root: Option<PathBuf>,
    pub avd_home: PathBuf,
}

impl Env {
    /// `ANDROID_SDK_ROOT` / `ANDROID_HOME` (then the platform default) for the SDK;
    /// `ANDROID_AVD_HOME`, then `ANDROID_EMULATOR_HOME/avd`, then `ANDROID_USER_HOME/avd`,
    /// then `~/.android/avd` for AVDs.
    pub fn from_env() -> Env {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME").unwrap_or_else(|| PathBuf::from("."));
        let sdk_root = var("ANDROID_SDK_ROOT").or_else(|| var("ANDROID_HOME")).or_else(|| {
            let p = home.join("Library/Android/sdk");
            p.is_dir().then_some(p)
        });
        let avd_home = var("ANDROID_AVD_HOME")
            .or_else(|| var("ANDROID_EMULATOR_HOME").map(|p| p.join("avd")))
            .or_else(|| var("ANDROID_USER_HOME").map(|p| p.join("avd")))
            .unwrap_or_else(|| home.join(".android/avd"));
        Env { sdk_root, avd_home }
    }

    /// AVD names, sorted, as `emulator -list-avds` prints them.
    pub fn list_avds(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.avd_home)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let stem = name.strip_suffix(".ini")?;
                e.path().is_file().then(|| stem.to_string())
            })
            .collect();
        names.sort();
        names
    }

    pub fn load(&self, name: &str) -> Result<Avd> {
        let pointer = self.avd_home.join(format!("{name}.ini"));
        let content_dir = if pointer.is_file() {
            let p = parse_ini(&read(&pointer)?);
            p.get("path")
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .or_else(|| p.get("path.rel").map(|r| self.avd_home.parent().unwrap_or(&self.avd_home).join(r)))
                .unwrap_or_else(|| self.avd_home.join(format!("{name}.avd")))
        } else {
            let d = self.avd_home.join(format!("{name}.avd"));
            if !d.is_dir() {
                return Err(Error::UnknownAvd(name.to_string(), self.avd_home.clone()));
            }
            d
        };
        let config = parse_ini(&read(&content_dir.join("config.ini"))?);
        Avd::new(name, content_dir, config, self.sdk_root.as_deref())
    }
}

/// A resolved AVD: its directory, config and system image.
#[derive(Clone, Debug)]
pub struct Avd {
    pub name: String,
    pub dir: PathBuf,
    pub config: BTreeMap<String, String>,
    pub sysdir: PathBuf,
    /// The SDK the image came from (holds the gfxstream renderer in `emulator/lib64` or
    /// `emu-rs/renderer/<revision>/lib64`).
    pub sdk_root: Option<PathBuf>,
}

impl Avd {
    fn new(name: &str, dir: PathBuf, config: BTreeMap<String, String>, sdk_root: Option<&Path>) -> Result<Avd> {
        let sysdir = (1..=3)
            .filter_map(|i| config.get(&format!("image.sysdir.{i}")))
            .map(|rel| match sdk_root {
                Some(root) if Path::new(rel).is_relative() => root.join(rel),
                _ => PathBuf::from(rel),
            })
            .find(|p| p.join("kernel-ranchu").is_file() || p.join("system.img").is_file())
            .ok_or_else(|| Error::Config(format!("{name}: no system image found (image.sysdir.N in config.ini, SDK root {sdk_root:?})")))?;
        Ok(Avd { name: name.to_string(), dir, config, sysdir, sdk_root: sdk_root.map(Path::to_path_buf) })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.config.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some("yes" | "true"))
    }

    pub fn cpus(&self) -> u32 {
        self.get("hw.cpu.ncore").and_then(|v| v.parse().ok()).unwrap_or(2).clamp(1, 16)
    }

    /// Guest RAM in bytes (`hw.ramSize`, which accepts `2048`, `2048M` or `2G`).
    ///
    /// Below 1 GB the stock launcher's 512 MB of padding is added: current Wear images are
    /// configured with 512 MB and the OOM killer takes `system_server` and adbd without it.
    /// Guest RAM is committed lazily, so the padding costs host memory only if it's used.
    pub fn ram_bytes(&self) -> u64 {
        let mb = self.get("hw.ramSize").and_then(parse_size_mb).unwrap_or(2048);
        let mb = if mb < 1024 { mb + 512 } else { mb };
        mb * 1024 * 1024
    }

    pub fn data_partition_bytes(&self) -> u64 {
        self.get("disk.dataPartition.size").and_then(parse_size_mb).unwrap_or(6 * 1024) * 1024 * 1024
    }

    pub fn lcd(&self) -> (u32, u32, u32) {
        let n = |k: &str, d: u32| self.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        (n("hw.lcd.width", 1080), n("hw.lcd.height", 2400), n("hw.lcd.density", 420))
    }

    pub fn has_sdcard(&self) -> bool {
        self.flag("hw.sdCard")
    }

    pub fn kernel(&self) -> PathBuf {
        self.sysdir.join("kernel-ranchu")
    }

    pub fn ramdisk(&self) -> PathBuf {
        self.sysdir.join("ramdisk.img")
    }

    pub fn system_image(&self) -> PathBuf {
        self.sysdir.join("system.img")
    }

    pub fn vendor_image(&self) -> PathBuf {
        self.sysdir.join("vendor.img")
    }

    /// API level from the system image's `source.properties`, if present.
    pub fn api_level(&self) -> Option<u32> {
        let props = std::fs::read_to_string(self.sysdir.join("source.properties")).ok()?;
        parse_ini(&props).get("AndroidVersion.ApiLevel")?.parse().ok()
    }

    /// Extra kernel command line the image ships (`kernel_cmdline.txt`).
    pub fn image_cmdline(&self) -> String {
        std::fs::read_to_string(self.sysdir.join("kernel_cmdline.txt")).map(|s| s.trim().to_string()).unwrap_or_default()
    }

    /// `androidboot.vbmeta.*` parameters from `VerifiedBootParams.textproto`.
    pub fn vbmeta_params(&self) -> Vec<(String, String)> {
        let Ok(text) = std::fs::read_to_string(self.sysdir.join("VerifiedBootParams.textproto")) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("param:"))
            .filter_map(|v| {
                let v = v.trim().trim_matches('"');
                let (k, val) = v.split_once('=')?;
                Some((k.to_string(), val.to_string()))
            })
            .collect()
    }
}

/// What `create_avd` writes. Defaults favour cheap, fast boots: a small screen boots markedly
/// faster than a large one, and more than 2 GB of RAM makes cold boots slower, not faster.
#[derive(Clone, Debug)]
pub struct NewAvd {
    pub name: String,
    /// Relative to the SDK root, e.g. `system-images/android-36/google_apis/arm64-v8a/`.
    pub sysdir: String,
    pub lcd: (u32, u32, u32),
    pub ram_mb: u64,
    pub cores: u32,
    pub data_gb: u32,
}

impl NewAvd {
    pub fn new(name: &str, sysdir: &str) -> NewAvd {
        NewAvd {
            name: name.to_string(),
            sysdir: sysdir.trim_end_matches('/').to_string() + "/",
            lcd: (720, 1280, 320),
            ram_mb: 2048,
            cores: 4,
            data_gb: 4,
        }
    }
}

impl Env {
    /// Creates an AVD the way the SDK's avdmanager lays it out (`<name>.ini` pointer and
    /// `<name>.avd/config.ini`), with no Java involved. Refuses to overwrite an existing AVD.
    pub fn create_avd(&self, spec: &NewAvd) -> Result<PathBuf> {
        let valid = !spec.name.is_empty() && spec.name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        if !valid {
            return Err(Error::Config(format!("invalid AVD name '{}' (letters, digits, . _ -)", spec.name)));
        }
        let dir = self.avd_home.join(format!("{}.avd", spec.name));
        let pointer = self.avd_home.join(format!("{}.ini", spec.name));
        if dir.exists() || pointer.exists() {
            return Err(Error::Config(format!("AVD '{}' already exists", spec.name)));
        }
        let parts: Vec<&str> = spec.sysdir.trim_end_matches('/').split('/').collect();
        let (target, tag, abi) = match parts.as_slice() {
            [.., target, tag, abi] => (*target, *tag, *abi),
            _ => return Err(Error::Config(format!("system image dir '{}' should end in <target>/<tag>/<abi>", spec.sysdir))),
        };
        let arch = if abi == "arm64-v8a" { "arm64" } else { abi };
        let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        let pointer_body = format!("avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/{}.avd\ntarget={target}\n", dir.display(), spec.name);
        std::fs::write(&pointer, pointer_body).map_err(|e| io(&pointer, e))?;
        let (w, h, dpi) = spec.lcd;
        let config = format!(
            "AvdId={name}\navd.ini.displayname={name}\nabi.type={abi}\nhw.cpu.arch={arch}\nhw.cpu.ncore={cores}\n\
             hw.ramSize={ram}\nhw.lcd.width={w}\nhw.lcd.height={h}\nhw.lcd.density={dpi}\nhw.gpu.enabled=yes\n\
             hw.gpu.mode=auto\nhw.keyboard=yes\nhw.sdCard=no\nimage.sysdir.1={sysdir}\ntag.id={tag}\n\
             disk.dataPartition.size={data}G\nfastboot.forceColdBoot=yes\n",
            name = spec.name,
            cores = spec.cores,
            ram = spec.ram_mb,
            sysdir = spec.sysdir,
            data = spec.data_gb,
        );
        let cfg = dir.join("config.ini");
        std::fs::write(&cfg, config).map_err(|e| io(&cfg, e))?;
        Ok(dir)
    }
}

/// Parses `2048`, `2048M`, `2048MB`, `2G`, `2GB` into megabytes.
pub fn parse_size_mb(v: &str) -> Option<u64> {
    let v = v.trim();
    let upper = v.to_ascii_uppercase();
    let (num, mult) = if let Some(n) = upper.strip_suffix("GB").or_else(|| upper.strip_suffix('G')) {
        (n.to_string(), 1024)
    } else if let Some(n) = upper.strip_suffix("MB").or_else(|| upper.strip_suffix('M')) {
        (n.to_string(), 1)
    } else if let Some(n) = upper.strip_suffix("KB").or_else(|| upper.strip_suffix('K')) {
        return n.trim().parse::<u64>().ok().map(|k| k.div_ceil(1024));
    } else {
        (upper.clone(), 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size_mb("2048"), Some(2048));
        assert_eq!(parse_size_mb("2G"), Some(2048));
        assert_eq!(parse_size_mb("10G"), Some(10240));
        assert_eq!(parse_size_mb("66MB"), Some(66));
        assert_eq!(parse_size_mb("512M"), Some(512));
        assert_eq!(parse_size_mb("bogus"), None);
    }

    #[test]
    fn loads_avd_from_dir() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-test-{}", std::process::id()));
        let avd_home = tmp.join("avd");
        let sdk = tmp.join("sdk");
        let sys = sdk.join("system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(avd_home.join("pixel.avd")).expect("mkdir");
        std::fs::create_dir_all(&sys).expect("mkdir");
        std::fs::write(sys.join("kernel-ranchu"), b"k").expect("write");
        std::fs::write(sys.join("source.properties"), "AndroidVersion.ApiLevel=36\n").expect("write");
        std::fs::write(
            avd_home.join("pixel.avd/config.ini"),
            "hw.cpu.ncore=4\nhw.ramSize=2G\nimage.sysdir.1=system-images/android-36/google_apis/arm64-v8a/\nhw.lcd.width=1080\n",
        )
        .expect("write");
        std::fs::write(avd_home.join("pixel.ini"), format!("path={}\n", avd_home.join("pixel.avd").display())).expect("write");
        let env = Env { sdk_root: Some(sdk), avd_home };
        assert_eq!(env.list_avds(), vec!["pixel".to_string()]);
        let avd = env.load("pixel").expect("load");
        assert_eq!(avd.cpus(), 4);
        assert_eq!(avd.ram_bytes(), 2 << 30);
        let mut small = avd.clone();
        small.config.insert("hw.ramSize".into(), "512".into());
        assert_eq!(small.ram_bytes(), 1 << 30, "stock pads small devices by 512 MB");
        assert_eq!(avd.api_level(), Some(36));
        assert!(env.load("nope").is_err());

        // A created AVD loads back with the requested hardware, and can't be created twice.
        let mut spec = NewAvd::new("small", "system-images/android-36/google_apis/arm64-v8a");
        spec.lcd = (480, 800, 240);
        env.create_avd(&spec).expect("create");
        let made = env.load("small").expect("load created");
        assert_eq!((made.lcd(), made.cpus(), made.ram_bytes()), ((480, 800, 240), 4, 2 << 30));
        assert!(env.create_avd(&spec).is_err());
        assert!(env.create_avd(&NewAvd::new("../evil", "a/b/c")).is_err());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
