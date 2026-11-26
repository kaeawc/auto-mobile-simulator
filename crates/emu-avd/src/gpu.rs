//! `-gpu` modes and the AVD's `hw.gpu.mode`: which Vulkan ICD gfxstream runs on.
//!
//! The mode names are the stock emulator's. SDK images ship no guest renderer (no
//! API 34-37 image carries one), so `guest` and `software` can't render in the guest; like
//! stock, they fall back to lavapipe and say so.

/// Every mode `-gpu` accepts, stock's current names first, then its legacy ones.
pub const MODES: &[&str] =
    &["auto", "host", "software", "lavapipe", "swiftshader", "swangle", "off", "none", "swiftshader_indirect", "angle_indirect", "guest"];

/// What the mode runs the renderer on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The host GPU through MoltenVK, falling back to software if it fails on this machine.
    Host,
    SwiftShader,
    Lavapipe,
    /// No renderer at all (`-gpu off`).
    Off,
}

/// The decision for one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuChoice {
    pub backend: Backend,
    /// The mode asked for in-guest rendering, which images can't do: print stock's fallback notice.
    pub guest_fallback: bool,
}

/// Stock's notice when an AVD wants in-guest rendering and the image can't (adapted).
pub const GUEST_FALLBACK_NOTICE: &str = "emu: Your AVD has been configured with an in-guest renderer, but the system image \
     does not support guest rendering. Falling back to 'lavapipe' mode.";

/// Parse a `-gpu` argument; an unknown name is an error that lists the valid ones.
pub fn parse_flag(mode: &str) -> Result<(), String> {
    if MODES.contains(&mode) { Ok(()) } else { Err(format!("unknown -gpu mode '{mode}'; valid modes: {}", MODES.join(", "))) }
}

fn choose(mode: &str) -> Option<GpuChoice> {
    let (backend, guest_fallback) = match mode {
        "auto" | "host" => (Backend::Host, false),
        "off" | "none" => (Backend::Off, false),
        "swiftshader" | "swiftshader_indirect" => (Backend::SwiftShader, false),
        "lavapipe" | "swangle" | "angle_indirect" => (Backend::Lavapipe, false),
        "software" | "guest" => (Backend::Lavapipe, true),
        _ => return None,
    };
    Some(GpuChoice { backend, guest_fallback })
}

/// Resolve the mode: the `-gpu` flag beats the AVD's `hw.gpu.mode`. An `off` in `config.ini`
/// (and any name this doesn't know) keeps today's default, the host renderer; only the flag
/// turns rendering off.
pub fn resolve(flag: Option<&str>, config_mode: Option<&str>) -> GpuChoice {
    let host = GpuChoice { backend: Backend::Host, guest_fallback: false };
    match flag {
        Some(f) => choose(f).unwrap_or(host),
        None => match config_mode.and_then(choose) {
            Some(c) if c.backend != Backend::Off => c,
            _ => host,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stock_mode_parses_and_unknown_lists_them() {
        for m in MODES {
            assert!(parse_flag(m).is_ok(), "{m}");
            assert!(choose(m).is_some(), "{m} has no mapping");
        }
        let e = parse_flag("vulkan").expect_err("unknown mode");
        assert!(e.contains("'vulkan'") && e.contains("lavapipe") && e.contains("swangle"), "{e}");
    }

    #[test]
    fn modes_map_to_backends() {
        let b = |m| resolve(Some(m), None).backend;
        assert_eq!(b("host"), Backend::Host);
        assert_eq!(b("swiftshader_indirect"), Backend::SwiftShader);
        assert_eq!(b("lavapipe"), Backend::Lavapipe);
        assert_eq!(b("swangle"), Backend::Lavapipe);
        assert_eq!(b("off"), Backend::Off);
        assert!(!resolve(Some("lavapipe"), None).guest_fallback);
    }

    #[test]
    fn guest_and_software_use_lavapipe_with_the_notice() {
        for m in ["guest", "software"] {
            assert_eq!(resolve(Some(m), None), GpuChoice { backend: Backend::Lavapipe, guest_fallback: true });
            assert_eq!(resolve(None, Some(m)), GpuChoice { backend: Backend::Lavapipe, guest_fallback: true });
        }
    }

    #[test]
    fn flag_beats_config() {
        assert_eq!(resolve(Some("host"), Some("guest")), GpuChoice { backend: Backend::Host, guest_fallback: false });
        assert_eq!(resolve(Some("swiftshader"), Some("software")).backend, Backend::SwiftShader);
        assert_eq!(resolve(Some("off"), Some("auto")).backend, Backend::Off);
    }

    #[test]
    fn default_is_unchanged() {
        let host = GpuChoice { backend: Backend::Host, guest_fallback: false };
        assert_eq!(resolve(None, None), host);
        assert_eq!(resolve(None, Some("auto")), host);
        assert_eq!(resolve(None, Some("off")), host);
        assert_eq!(resolve(None, Some("mesa")), host);
    }
}
