//! No-root network shaping for simulator apps.
//!
//! Simulator apps are host processes, so a dylib inserted with `DYLD_INSERT_LIBRARIES` runs inside
//! them. `native/netshape.c` interposes the socket calls (including the batch calls Network.framework
//! uses) and applies a [`Profile`] read from a file it re-reads whenever the file changes. This
//! module builds that dylib with Xcode's simulator toolchain (once per source version, cached),
//! writes profiles, and returns the environment a launch needs.
//!
//! Scope: app-level shaping of sockets to non-loopback peers. Traffic made for the app by system
//! daemons (background `URLSession` transfers in `nsurlsessiond`) is not covered unless those
//! daemons are injected too.

pub mod proxy;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// The shaper's C source, compiled on demand.
pub const SOURCE: &str = include_str!("../native/netshape.c");
/// A link stub for the runtime's private libdns_services (the resolver the shaper hooks).
pub const DNS_SERVICES_TBD: &str = include_str!("../native/libdns_services.tbd");
/// The in-device proxy tool's source (sets the device's proxy in its own `configd_sim`).
pub const SCPROXY_SOURCE: &str = include_str!("../native/scproxy.m");

/// A network profile. Zero means "no limit" for every field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Refuse every new remote connection (`ENETUNREACH`).
    pub offline: bool,
    /// Added round-trip latency, applied to the handshake and to each request/response turnaround.
    pub latency_ms: u32,
    /// Download and upload bandwidth in kilobits per second.
    pub down_kbps: u32,
    pub up_kbps: u32,
    /// Percent of new connections that time out.
    pub loss_pct: u32,
    /// When non-zero, shaped sockets bind to source ports `[port_base, port_base + port_count)`
    /// (default count 1000), so a host pf/dummynet rule can shape exactly these flows.
    pub port_base: u16,
    pub port_count: u16,
    /// Per-host overrides, first match wins.
    pub rules: Vec<Rule>,
}

/// A per-host override. `pattern` is a hostname suffix (`example.com` also matches
/// `api.example.com`), an address or CIDR (`10.0.0.0/8`, `2001:db8::/32`), or `*`. Unset fields keep
/// the base profile's values.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub pattern: String,
    pub offline: Option<bool>,
    pub latency_ms: Option<u32>,
    pub down_kbps: Option<u32>,
    pub up_kbps: Option<u32>,
    pub loss_pct: Option<u32>,
}

impl Rule {
    /// Whether this rule applies to `host` (a name or an address literal).
    pub fn matches(&self, host: &str) -> bool {
        let pat = self.pattern.trim();
        if pat == "*" {
            return true;
        }
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(addr) = host.parse::<std::net::IpAddr>() {
            return cidr_match(addr, pat);
        }
        let (h, p) = (host.to_ascii_lowercase(), pat.to_ascii_lowercase());
        h == p || h.ends_with(&format!(".{p}"))
    }
}

fn cidr_match(addr: std::net::IpAddr, pattern: &str) -> bool {
    let (net, bits) = pattern.split_once('/').map_or((pattern, None), |(n, b)| (n, b.parse::<u32>().ok()));
    let Ok(net) = net.parse::<std::net::IpAddr>() else { return false };
    let (a, n, width) = match (addr, net) {
        (std::net::IpAddr::V4(a), std::net::IpAddr::V4(n)) => (u128::from(u32::from(a)), u128::from(u32::from(n)), 32),
        (std::net::IpAddr::V6(a), std::net::IpAddr::V6(n)) => (u128::from(a), u128::from(n), 128),
        _ => return false,
    };
    let bits = bits.unwrap_or(width).min(width);
    if bits == 0 {
        return true;
    }
    let shift = width - bits;
    (a >> shift) == (n >> shift)
}

impl Profile {
    /// Named presets, similar to the host's Network Link Conditioner profiles.
    pub fn preset(name: &str) -> Option<Self> {
        let p = |latency_ms, down_kbps, up_kbps, loss_pct| Profile { latency_ms, down_kbps, up_kbps, loss_pct, ..Profile::default() };
        Some(match name {
            "none" | "wifi" => Profile::default(),
            "offline" => Profile { offline: true, ..Profile::default() },
            "3g" => p(200, 780, 330, 0),
            "edge" => p(400, 240, 200, 0),
            "lte" => p(50, 12_000, 4_000, 0),
            "lossy" => p(100, 2_000, 1_000, 10),
            "very-bad" => p(500, 1_000, 1_000, 10),
            _ => return None,
        })
    }

    /// The upload (`up`) or download rate limit for `host` in kbit/s (0 = none), without building
    /// its whole profile.
    pub fn rate_kbps(&self, host: &str, up: bool) -> u32 {
        let rule = self.rules.iter().find(|r| r.matches(host));
        if up { rule.and_then(|r| r.up_kbps).unwrap_or(self.up_kbps) } else { rule.and_then(|r| r.down_kbps).unwrap_or(self.down_kbps) }
    }

    /// The profile in effect for connections to `host`.
    pub fn for_host(&self, host: &str) -> Profile {
        let mut p = self.clone();
        if let Some(r) = self.rules.iter().find(|r| r.matches(host)) {
            p.offline = r.offline.unwrap_or(p.offline);
            p.latency_ms = r.latency_ms.unwrap_or(p.latency_ms);
            p.down_kbps = r.down_kbps.unwrap_or(p.down_kbps);
            p.up_kbps = r.up_kbps.unwrap_or(p.up_kbps);
            p.loss_pct = r.loss_pct.unwrap_or(p.loss_pct);
        }
        p
    }

    /// The `key=value` file format the shaper reads.
    pub fn to_conf(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "offline={}", u8::from(self.offline));
        let _ = writeln!(s, "latency_ms={}", self.latency_ms);
        let _ = writeln!(s, "down_kbps={}", self.down_kbps);
        let _ = writeln!(s, "up_kbps={}", self.up_kbps);
        let _ = writeln!(s, "loss_pct={}", self.loss_pct.min(100));
        let _ = writeln!(s, "port_base={}", self.port_base);
        let _ = writeln!(s, "port_count={}", self.port_count);
        for r in &self.rules {
            if r.pattern.trim().is_empty() || r.pattern.contains(char::is_whitespace) {
                continue;
            }
            let _ = write!(s, "rule {}", r.pattern);
            if let Some(v) = r.offline {
                let _ = write!(s, " offline={}", u8::from(v));
            }
            for (k, v) in [("latency_ms", r.latency_ms), ("down_kbps", r.down_kbps), ("up_kbps", r.up_kbps), ("loss_pct", r.loss_pct)] {
                if let Some(v) = v {
                    let _ = write!(s, " {k}={v}");
                }
            }
            s.push('\n');
        }
        s
    }

    /// Parses the file format back (unknown keys ignored).
    pub fn from_conf(text: &str) -> Self {
        let mut p = Profile::default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("rule ") {
                let mut parts = rest.split_whitespace();
                let Some(pattern) = parts.next() else { continue };
                let mut r = Rule { pattern: pattern.to_owned(), ..Rule::default() };
                for kv in parts {
                    let Some((k, v)) = kv.split_once('=') else { continue };
                    let v: Option<u32> = v.parse().ok();
                    match k {
                        "offline" => r.offline = v.map(|x| x != 0),
                        "latency_ms" => r.latency_ms = v,
                        "down_kbps" => r.down_kbps = v,
                        "up_kbps" => r.up_kbps = v,
                        "loss_pct" => r.loss_pct = v,
                        _ => {}
                    }
                }
                p.rules.push(r);
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v: u32 = v.trim().parse().unwrap_or(0);
            match k.trim() {
                "offline" => p.offline = v != 0,
                "latency_ms" => p.latency_ms = v,
                "down_kbps" => p.down_kbps = v,
                "up_kbps" => p.up_kbps = v,
                "loss_pct" => p.loss_pct = v,
                "port_base" => p.port_base = u16::try_from(v).unwrap_or(0),
                "port_count" => p.port_count = u16::try_from(v).unwrap_or(0),
                _ => {}
            }
        }
        p
    }
}

/// Where simee keeps the built dylib and per-device profiles.
pub fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    home.join("Library/Caches/simee/netshape")
}

/// The profile file for a device.
pub fn profile_path(udid: &str) -> PathBuf {
    cache_dir().join(format!("{udid}.conf"))
}

/// Writes a device's profile; running injected apps pick it up within a quarter second.
pub fn write_profile(udid: &str, profile: &Profile) -> Result<PathBuf, String> {
    let path = profile_path(udid);
    std::fs::create_dir_all(cache_dir()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, profile.to_conf()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Reads a device's current profile (default when none was written).
pub fn read_profile(udid: &str) -> Profile {
    std::fs::read_to_string(profile_path(udid)).map(|t| Profile::from_conf(&t)).unwrap_or_default()
}

fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Builds `source` for the iOS simulator once per source version and returns the cached product.
fn build(name: &str, ext: &str, source: &str, flags: &[&str]) -> Result<PathBuf, String> {
    let dir = cache_dir();
    let out = dir.join(format!("{name}-{:016x}{ext}", fnv1a(source)));
    if out.is_file() {
        return Ok(out);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let src_ext = if flags.contains(&"-fobjc-arc") { "m" } else { "c" };
    let src = dir.join(format!("{name}.{src_ext}"));
    std::fs::write(&src, source).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{name}.building"));
    run(Command::new("/usr/bin/xcrun")
        .args(["--sdk", "iphonesimulator", "clang", "-target", "arm64-apple-ios15.0-simulator", "-O2"])
        .args(flags)
        .arg("-o")
        .arg(&tmp)
        .arg(&src))?;
    run(Command::new("/usr/bin/codesign").args(["-fs", "-"]).arg(&tmp))?;
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Builds (or reuses) the shaper dylib for the iOS simulator and returns its path.
pub fn dylib() -> Result<PathBuf, String> {
    // The resolver hook binds to libdns_services, which lives in the simulator runtime, not the SDK:
    // link against a stub that names its two symbols.
    std::fs::create_dir_all(cache_dir()).map_err(|e| e.to_string())?;
    let tbd = cache_dir().join("libdns_services.tbd");
    std::fs::write(&tbd, DNS_SERVICES_TBD).map_err(|e| e.to_string())?;
    let tbd = tbd.display().to_string();
    build(
        "netshape",
        ".dylib",
        &format!("{SOURCE}// link: {}\n", fnv1a(DNS_SERVICES_TBD)),
        &["-dynamiclib", "-framework", "Network", "-framework", "SystemConfiguration", &tbd],
    )
}

/// Builds (or reuses) the in-device proxy tool. Run it inside a device with
/// `set <host> <port>`, `clear` or `get`.
pub fn scproxy_tool() -> Result<PathBuf, String> {
    build("scproxy", "", SCPROXY_SOURCE, &["-fobjc-arc", "-framework", "Foundation", "-framework", "CFNetwork"])
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim())) }
}

/// The environment that makes a launched simulator process shaped by `udid`'s profile.
pub fn launch_env(udid: &str, dylib: &Path) -> Vec<(String, String)> {
    vec![("DYLD_INSERT_LIBRARIES".into(), dylib.display().to_string()), ("SIMEE_NETSHAPE".into(), profile_path(udid).display().to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_match_the_hosts_profile() {
        let p = Profile {
            down_kbps: 780,
            up_kbps: 330,
            rules: vec![Rule { pattern: "example.com".into(), down_kbps: Some(50), ..Rule::default() }],
            ..Profile::default()
        };
        for host in ["example.com", "api.example.com", "other.org"] {
            let full = p.for_host(host);
            assert_eq!((p.rate_kbps(host, true), p.rate_kbps(host, false)), (full.up_kbps, full.down_kbps), "{host}");
        }
    }

    #[test]
    fn conf_round_trips_and_presets_exist() {
        let p = Profile {
            offline: true,
            latency_ms: 250,
            down_kbps: 780,
            up_kbps: 330,
            loss_pct: 150,
            port_base: 40000,
            port_count: 500,
            rules: vec![Rule { pattern: "example.com".into(), offline: Some(true), latency_ms: Some(900), ..Rule::default() }],
        };
        let back = Profile::from_conf(&p.to_conf());
        assert_eq!(back, Profile { loss_pct: 100, ..p });
        assert_eq!(Profile::preset("offline").map(|p| p.offline), Some(true));
        assert_eq!(Profile::preset("3g").map(|p| p.latency_ms), Some(200));
        assert_eq!(Profile::preset("nope"), None);
        assert_eq!(Profile::from_conf("garbage\nlatency_ms = 9\n").latency_ms, 9);
    }

    #[test]
    fn rules_match_hosts_and_cidrs() {
        let r = |p: &str| Rule { pattern: p.into(), ..Rule::default() };
        assert!(r("example.com").matches("api.example.com"));
        assert!(r("example.com").matches("EXAMPLE.com"));
        assert!(!r("example.com").matches("badexample.com"));
        assert!(r("10.0.0.0/8").matches("10.2.3.4"));
        assert!(!r("10.0.0.0/8").matches("11.0.0.1"));
        assert!(r("2001:db8::/32").matches("[2001:db8::1]"));
        assert!(r("192.168.1.5").matches("192.168.1.5"));
        assert!(r("*").matches("anything"));
        let p = Profile {
            latency_ms: 10,
            rules: vec![Rule { pattern: "slow.test".into(), latency_ms: Some(500), ..Rule::default() }],
            ..Profile::default()
        };
        assert_eq!(p.for_host("a.slow.test").latency_ms, 500);
        assert_eq!(p.for_host("fast.test").latency_ms, 10);
    }

    #[test]
    fn source_version_changes_the_cache_name() {
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }
}
