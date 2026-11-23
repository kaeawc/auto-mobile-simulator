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
use sim_core::form_factor::{Density, FormFactor, Profile, Shape, android_profile};

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

    /// What the device is (phone, tablet, foldable, Wear OS watch, TV, Automotive head unit) and
    /// its screen, shape, fold and inputs, inferred from `config.ini`. The system image tag is
    /// read from the image's `source.properties` when the config doesn't name it.
    pub fn profile(&self) -> Profile {
        let image_tag = || {
            let props = std::fs::read_to_string(self.sysdir.join("source.properties")).ok()?;
            parse_ini(&props).get("SystemImage.TagId").cloned()
        };
        android_profile(|k| match (self.get(k), k) {
            (Some(v), _) => Some(v.to_owned()),
            (None, "tag.id") => image_tag(),
            (None, _) => None,
        })
    }

    /// Whether the device has hardware Back/Home keys (`hw.mainKeys`, stock's default yes). The
    /// guest hides its navigation bar when it does (`qemu.hw.mainkeys=1`).
    pub fn main_keys(&self) -> bool {
        self.get("hw.mainKeys").is_none_or(|v| matches!(v.trim(), "yes" | "true"))
    }

    /// The properties stock forwards to the guest at boot from `boot.prop`: the AVD's own, else
    /// the system image's (avdmanager writes `ro.emulator.circular=true` for round watches).
    pub fn boot_properties(&self) -> Vec<(String, String)> {
        [self.dir.join("boot.prop"), self.sysdir.join("boot.prop")]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse_properties(&text))
            .unwrap_or_default()
    }

    /// The properties the stock host hands the guest's `qemu-props` for this AVD: `boot.prop`'s
    /// lines, then the LCD density and whether there are hardware keys (set last, as in stock).
    pub fn qemu_props(&self) -> Vec<(String, String)> {
        let mut props = self.boot_properties();
        props.push(("qemu.sf.lcd_density".to_owned(), self.lcd().2.to_string()));
        props.push(("qemu.hw.mainkeys".to_owned(), if self.main_keys() { "1" } else { "0" }.to_owned()));
        props
    }

    /// Whether the screen is round: `ro.emulator.circular` in `boot.prop` (what stock checks;
    /// `1`, `y`, `on`, `yes`, `true` and their prefixes count), or `hw.lcd.circular`.
    pub fn circular(&self) -> bool {
        let truthy = |v: &str| ["1", "y", "on", "yes", "true"].iter().any(|t| v.starts_with(t));
        self.boot_properties().iter().any(|(k, v)| k == "ro.emulator.circular" && truthy(v)) || self.flag("hw.lcd.circular")
    }

    /// The sensors the guest's HAL is told about (`list-sensors`), decided like stock: by the
    /// `hw.sensor*` flags, plus heart rate and wrist tilt on Wear OS at API 28 and later.
    pub fn sensor_mask(&self) -> u32 {
        sim_core::goldfish::sensor_mask(|k| self.get(k).map(str::to_owned), self.profile().kind.form_factor, self.api_level())
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
    /// Hardware Back/Home keys (`hw.mainKeys`): the guest then shows no navigation bar.
    pub main_keys: bool,
    /// A D-pad (`hw.dPad`).
    pub dpad: bool,
    /// More `config.ini` lines (a foldable's hinge sensor, for one).
    pub extra: Vec<(String, String)>,
    /// `<avd>/boot.prop` lines, forwarded to the guest at boot (a round watch's
    /// `ro.emulator.circular`). None writes no file.
    pub boot_props: Vec<(String, String)>,
}

impl NewAvd {
    /// A phone: on-screen navigation, no D-pad, a hardware keyboard.
    pub fn new(name: &str, sysdir: &str) -> NewAvd {
        NewAvd {
            name: name.to_string(),
            sysdir: sysdir.trim_end_matches('/').to_string() + "/",
            lcd: (720, 1280, 320),
            ram_mb: 2048,
            cores: 4,
            data_gb: 4,
            main_keys: false,
            dpad: false,
            extra: Vec::new(),
            boot_props: Vec::new(),
        }
    }

    /// An AVD shaped like `profile`: its screen, and the hardware the SDK's device profiles give
    /// its form factor.
    pub fn for_profile(name: &str, sysdir: &str, profile: &Profile) -> NewAvd {
        let mut spec = NewAvd::new(name, sysdir);
        if let Density::Dpi(dpi) = profile.density {
            spec.lcd = (profile.width, profile.height, dpi);
        }
        let kv = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        match profile.kind.form_factor {
            // The hinge sensor the stock emulator's foldable definitions declare (one hinge,
            // 0-180°, three postures).
            FormFactor::Foldable => {
                if let Some(fold) = profile.fold {
                    spec.extra = vec![
                        kv("hw.sensor.hinge", "yes"),
                        kv("hw.sensor.hinge.count", "1"),
                        kv("hw.sensor.hinge.type", "0"),
                        kv("hw.sensor.hinge.ranges", "0-180"),
                        kv("hw.sensor.hinge.defaults", &fold.angle.to_string()),
                        kv("hw.sensor.posture_list", "1,2,3"),
                        kv("hw.sensor.hinge_angles_posture_definitions", "0-30, 30-150, 150-180"),
                    ];
                }
            }
            // Wear OS: 512 MB (booted with stock's 512 MB of padding, see `Avd::ram_bytes`),
            // hardware keys, no D-pad; avdmanager marks a round screen in boot.prop.
            FormFactor::Watch => {
                spec.ram_mb = 512;
                spec.main_keys = true;
                if profile.shape == Shape::Round {
                    spec.boot_props.push(kv("ro.emulator.circular", "true"));
                }
            }
            // Android TV (tv.xml): 2 GB (stock refuses less than 1 GB), hardware keys, a D-pad.
            FormFactor::Tv => {
                spec.ram_mb = 2048;
                spec.main_keys = true;
                spec.dpad = true;
            }
            // Automotive (device/generic/car/common/config.ini): 4 GB, on-screen navigation,
            // and only the motion sensors.
            FormFactor::Auto => {
                spec.ram_mb = 4096;
                spec.extra = ["light", "pressure", "humidity", "proximity", "magnetic_field", "orientation", "temperature"]
                    .iter()
                    .map(|s| kv(&format!("hw.sensors.{s}"), "no"))
                    .collect();
            }
            FormFactor::Phone | FormFactor::Tablet | FormFactor::Vision => {}
        }
        spec
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
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        let config = format!(
            "AvdId={name}\navd.ini.displayname={name}\nabi.type={abi}\nhw.cpu.arch={arch}\nhw.cpu.ncore={cores}\n\
             hw.ramSize={ram}\nhw.lcd.width={w}\nhw.lcd.height={h}\nhw.lcd.density={dpi}\nhw.gpu.enabled=yes\n\
             hw.gpu.mode=auto\nhw.keyboard=yes\nhw.mainKeys={main_keys}\nhw.dPad={dpad}\nhw.trackBall=no\n\
             hw.sdCard=no\nimage.sysdir.1={sysdir}\ntag.id={tag}\ndisk.dataPartition.size={data}G\nfastboot.forceColdBoot=yes\n",
            name = spec.name,
            cores = spec.cores,
            ram = spec.ram_mb,
            main_keys = yes_no(spec.main_keys),
            dpad = yes_no(spec.dpad),
            sysdir = spec.sysdir,
            data = spec.data_gb,
        );
        let lines = |kv: &[(String, String)]| kv.iter().map(|(k, v)| format!("{k}={v}\n")).collect::<String>();
        let cfg = dir.join("config.ini");
        std::fs::write(&cfg, config + &lines(&spec.extra)).map_err(|e| io(&cfg, e))?;
        if !spec.boot_props.is_empty() {
            let boot = dir.join("boot.prop");
            std::fs::write(&boot, lines(&spec.boot_props)).map_err(|e| io(&boot, e))?;
        }
        Ok(dir)
    }
}

/// Where running emulators advertise themselves (`pid_<pid>.ini`), as the SDK tools look for them.
pub fn running_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/TemporaryItems/avd/running"))
}

/// A running emulator, from its advertisement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub pid: i32,
    pub avd_name: String,
    pub console_port: u16,
    pub adb_port: u16,
    pub avd_dir: Option<PathBuf>,
    /// emu-rs wrote it (the stock emulator writes the same file without `emulator=emu-rs`).
    pub emu_rs: bool,
}

impl Running {
    /// The adb serial, `emulator-<console port>`.
    pub fn serial(&self) -> String {
        format!("emulator-{}", self.console_port)
    }

    /// Parses `pid_<pid>.ini` named `file_name`.
    pub fn parse(file_name: &str, text: &str) -> Option<Running> {
        let pid = file_name.strip_prefix("pid_")?.strip_suffix(".ini")?.parse().ok()?;
        let kv = parse_ini(text);
        let console_port: u16 = kv.get("port.serial")?.parse().ok()?;
        Some(Running {
            pid,
            avd_name: kv.get("avd.name")?.clone(),
            console_port,
            adb_port: kv.get("port.adb").and_then(|p| p.parse().ok()).unwrap_or(console_port + 1),
            avd_dir: kv.get("avd.dir").map(PathBuf::from),
            emu_rs: kv.get("emulator").is_some_and(|e| e == "emu-rs"),
        })
    }

    /// Whether its process still exists.
    pub fn alive(&self) -> bool {
        rustix::process::Pid::from_raw(self.pid).is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
    }
}

/// Every advertised emulator whose process is still alive.
pub fn running_instances() -> Vec<Running> {
    running_dir().map(|d| running_instances_in(&d)).unwrap_or_default()
}

/// Every emulator advertised in `dir` whose process is still alive.
pub fn running_instances_in(dir: &Path) -> Vec<Running> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Running::parse(e.file_name().to_str()?, &std::fs::read_to_string(e.path()).ok()?))
        .filter(Running::alive)
        .collect()
}

/// Parses an Android property file (`name=value` lines; blank lines, `#` comments and lines
/// without `=` skipped), in order.
pub fn parse_properties(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
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
    fn running_advertisements_parse() {
        let text = "port.serial=5554\nport.adb=5555\navd.name=pixel\navd.dir=/a/pixel.avd\nemulator=emu-rs\n";
        let r = Running::parse("pid_42.ini", text).expect("parsed");
        assert_eq!((r.pid, r.avd_name.as_str(), r.console_port, r.adb_port, r.emu_rs), (42, "pixel", 5554, 5555, true));
        assert_eq!(r.serial(), "emulator-5554");
        assert_eq!(r.avd_dir.as_deref(), Some(Path::new("/a/pixel.avd")));
        let stock = Running::parse("pid_7.ini", "port.serial=5556\navd.name=other\n").expect("stock");
        assert_eq!((stock.adb_port, stock.emu_rs), (5557, false));
        assert_eq!(Running::parse("notes.txt", text), None);
        assert_eq!(Running::parse("pid_1.ini", "avd.name=x\n"), None);
        let me = Running { pid: std::process::id() as i32, ..r };
        assert!(me.alive());
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
        let env = Env { sdk_root: Some(sdk.clone()), avd_home };
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
        assert_eq!(made.profile().kind.form_factor, FormFactor::Phone);

        // Form factors come back from what was written: a foldable's hinge, a Wear image's tag.
        let fold = sim_core::form_factor::Kind::new(sim_core::platform::Platform::Android, FormFactor::Foldable).expect("fold").profile();
        env.create_avd(&NewAvd::for_profile("fold", "system-images/android-36/google_apis/arm64-v8a", &fold)).expect("create fold");
        let made = env.load("fold").expect("load fold");
        assert_eq!(made.lcd(), (2208, 1840, 420));
        let p = made.profile();
        assert_eq!((p.kind.form_factor, p.fold.map(|f| f.posture())), (FormFactor::Foldable, Some(sim_core::form_factor::Posture::Opened)));
        let wear_sys = sdk.join("system-images/android-34/android-wear/arm64-v8a");
        std::fs::create_dir_all(&wear_sys).expect("mkdir");
        std::fs::write(wear_sys.join("system.img"), b"s").expect("write");
        std::fs::write(wear_sys.join("source.properties"), "SystemImage.TagId=android-wear\n").expect("write");
        let mut watch = NewAvd::new("watch", "system-images/android-34/android-wear/arm64-v8a");
        watch.lcd = (454, 454, 320);
        env.create_avd(&watch).expect("create watch");
        let w = env.load("watch").expect("load watch").profile();
        assert_eq!((w.kind.sub_platform.as_str(), w.shape), ("wearos", sim_core::form_factor::Shape::Round));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn properties_parse_in_order() {
        let text = "# avdmanager\nro.emulator.circular = true\n\n  qemu.x=a=b\nnot a property\n=v\n";
        let expect = [("ro.emulator.circular", "true"), ("qemu.x", "a=b")].map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(parse_properties(text), expect);
    }

    /// Creates each form factor's AVD from its profile and loads it back.
    #[test]
    fn new_avds_get_their_form_factors_hardware() {
        use sim_core::form_factor::Kind;
        use sim_core::platform::Platform;
        let tmp = std::env::temp_dir().join(format!("emu-avd-kinds-{}", std::process::id()));
        let sdk = tmp.join("sdk");
        let env = Env { sdk_root: Some(sdk.clone()), avd_home: tmp.join("avd") };
        let image = |tag: &str, api: u32| {
            let rel = format!("system-images/android-{api}/{tag}/arm64-v8a");
            let dir = sdk.join(&rel);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("system.img"), b"s").expect("write");
            std::fs::write(dir.join("source.properties"), format!("AndroidVersion.ApiLevel={api}\n")).expect("write");
            rel
        };
        let create = |name: &str, form_factor: FormFactor, sysdir: &str| {
            let profile = Kind::new(Platform::Android, form_factor).expect("kind").profile();
            env.create_avd(&NewAvd::for_profile(name, sysdir, &profile)).expect("create");
            env.load(name).expect("load")
        };
        let hw =
            |avd: &Avd| ["hw.mainKeys", "hw.dPad", "hw.keyboard", "hw.trackBall"].map(|k| avd.get(k).unwrap_or("-").to_string()).join(" ");
        let phone_mask = 0x7ff | (1 << 17);

        let phone = create("phone", FormFactor::Phone, &image("google_apis", 36));
        assert_eq!(hw(&phone), "no no yes no");
        assert_eq!((phone.main_keys(), phone.circular(), phone.ram_bytes()), (false, false, 2 << 30));
        assert_eq!(phone.qemu_props().last(), Some(&("qemu.hw.mainkeys".to_string(), "0".to_string())), "a navigation bar");
        assert!(!phone.dir.join("boot.prop").exists());
        assert_eq!(phone.sensor_mask(), phone_mask);

        // Wear: 512 MB (booted as 1 GB), hardware keys, round, heart rate and wrist tilt.
        let watch = create("watch", FormFactor::Watch, &image("android-wear", 34));
        assert_eq!(hw(&watch), "yes no yes no");
        assert_eq!((watch.get("hw.ramSize"), watch.ram_bytes()), (Some("512"), 1 << 30));
        assert_eq!(watch.boot_properties(), [("ro.emulator.circular".to_string(), "true".to_string())]);
        assert!(watch.main_keys() && watch.circular());
        let props: Vec<String> = watch.qemu_props().iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(props, ["ro.emulator.circular=true", "qemu.sf.lcd_density=320", "qemu.hw.mainkeys=1"]);
        assert_eq!(watch.sensor_mask(), phone_mask | (1 << 14) | (1 << 16));

        let tv = create("tv", FormFactor::Tv, &image("google-tv", 34));
        assert_eq!(hw(&tv), "yes yes yes no");
        assert_eq!((tv.profile().kind.form_factor, tv.ram_bytes(), tv.circular()), (FormFactor::Tv, 2 << 30, false));

        let car = create("car", FormFactor::Auto, &image("android-automotive", 34));
        assert_eq!(hw(&car), "no no yes no");
        assert_eq!((car.profile().kind.form_factor, car.ram_bytes()), (FormFactor::Auto, 4 << 30));
        assert_eq!(car.sensor_mask(), 0b11 | (1 << 9) | (1 << 10) | (1 << 17), "motion sensors only");

        let fold = create("fold", FormFactor::Foldable, &image("google_apis", 36));
        assert_eq!(fold.sensor_mask(), phone_mask | (1 << 11), "the hinge reaches the HAL");
        assert_eq!(hw(&fold), "no no yes no");

        // An AVD without boot.prop falls back to the image's; `hw.mainKeys` defaults to yes.
        let mut bare = phone.clone();
        bare.config.remove("hw.mainKeys");
        assert!(bare.main_keys(), "stock's default");
        std::fs::write(bare.sysdir.join("boot.prop"), "ro.emulator.circular=1\n").expect("write");
        assert!(bare.circular());
        std::fs::write(bare.dir.join("boot.prop"), "ro.emulator.circular=no\n").expect("write");
        assert!(!bare.circular(), "the AVD's boot.prop wins");
        bare.config.insert("hw.lcd.circular".into(), "yes".into());
        assert!(bare.circular());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
