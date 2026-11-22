//! Named values for the debug surface: render-server overlays, Dynamic Type sizes, status bar
//! enums and biometric notifications.

/// Core Animation render-server debug overlays: (id, key, label). Ids were mapped by toggling each
/// option on a running device and comparing frames.
pub const CA_OPTIONS: &[(u32, &str, &str)] = &[
    (0, "flash-updates", "Flash updated regions"),
    (1, "copied-images", "Color copied images"),
    (2, "blended-layers", "Color blended layers"),
    (17, "offscreen", "Color off-screen rendered"),
    (19, "cache-hits", "Color cache hits green"),
    (20, "misaligned", "Color misaligned images"),
];

/// Looks up an overlay by key or numeric id.
pub fn ca_option(name: &str) -> Option<u32> {
    CA_OPTIONS.iter().find(|(_, k, _)| *k == name).map(|(id, _, _)| *id).or_else(|| name.parse().ok())
}

/// Dynamic Type sizes in service order.
pub const CONTENT_SIZES: &[&str] = &[
    "extra-small",
    "small",
    "medium",
    "large",
    "extra-large",
    "extra-extra-large",
    "extra-extra-extra-large",
    "accessibility-medium",
    "accessibility-large",
    "accessibility-extra-large",
    "accessibility-extra-extra-large",
    "accessibility-extra-extra-extra-large",
];

/// Looks up a Dynamic Type size by name or index.
pub fn content_size(name: &str) -> Option<i64> {
    CONTENT_SIZES
        .iter()
        .position(|s| *s == name)
        .map(|i| i as i64)
        .or_else(|| name.parse().ok().filter(|&i: &i64| (0..CONTENT_SIZES.len() as i64).contains(&i)))
}

/// Status bar data network names in service order.
pub const DATA_NETWORKS: &[&str] = &["hide", "wifi", "3g", "4g", "lte", "lte-a", "lte+", "5g", "5g+", "5g-uwb", "5g-uc"];
/// Wi-Fi / cellular mode names in service order.
pub const RADIO_MODES: &[&str] = &["not-supported", "searching", "failed", "active"];
/// Battery state names in service order.
pub const BATTERY_STATES: &[&str] = &["charging", "charged", "discharging"];

/// Index of `name` in `table`.
pub fn index_of(table: &[&str], name: &str) -> Option<u64> {
    table.iter().position(|s| *s == name).map(|i| i as u64)
}

/// The Darwin notification that simulates a biometric attempt.
pub fn biometric_notification(face: bool, matched: bool) -> &'static str {
    match (face, matched) {
        (true, true) => "com.apple.BiometricKit_Sim.pearl.match",
        (true, false) => "com.apple.BiometricKit_Sim.pearl.nomatch",
        (false, true) => "com.apple.BiometricKit_Sim.fingerTouch.match",
        (false, false) => "com.apple.BiometricKit_Sim.fingerTouch.nomatch",
    }
}

/// The Darwin notification whose state is the biometric enrollment flag.
pub const BIOMETRIC_ENROLLMENT: &str = "com.apple.BiometricKit.enrollmentChanged";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        assert_eq!(ca_option("blended-layers"), Some(2));
        assert_eq!(ca_option("17"), Some(17));
        assert_eq!(ca_option("nope"), None);
        assert_eq!(content_size("large"), Some(3));
        assert_eq!(content_size("11"), Some(11));
        assert_eq!(content_size("12"), None);
        assert_eq!(index_of(DATA_NETWORKS, "lte"), Some(4));
        assert_eq!(biometric_notification(true, false), "com.apple.BiometricKit_Sim.pearl.nomatch");
    }
}
