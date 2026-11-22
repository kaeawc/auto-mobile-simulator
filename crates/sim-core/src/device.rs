//! Device, runtime and device-type models as reported by the simulator service.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a simulated device. Values match the service's raw state numbers.
/// Serialized with the same names `simctl list -j` uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceState {
    Creating,
    Shutdown,
    Booting,
    Booted,
    #[serde(rename = "Shutting Down")]
    ShuttingDown,
    Unknown,
}

impl DeviceState {
    /// Maps the service's raw state number.
    pub fn from_raw(raw: u64) -> Self {
        match raw {
            0 => Self::Creating,
            1 => Self::Shutdown,
            2 => Self::Booting,
            3 => Self::Booted,
            4 => Self::ShuttingDown,
            _ => Self::Unknown,
        }
    }

    /// Human-readable name, matching the service's own wording.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "Creating",
            Self::Shutdown => "Shutdown",
            Self::Booting => "Booting",
            Self::Booted => "Booted",
            Self::ShuttingDown => "Shutting Down",
            Self::Unknown => "Unknown",
        }
    }
}

/// One simulated device in a device set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub state: DeviceState,
    pub runtime_id: String,
    pub device_type_id: String,
    pub available: bool,
}

/// An installed OS runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub identifier: String,
    pub name: String,
    pub version: String,
    pub build: String,
    pub available: bool,
}

/// A device model (hardware profile).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceTypeInfo {
    pub identifier: String,
    pub name: String,
    pub model: String,
    pub product_family: String,
    /// Main screen size in pixels.
    pub screen_width: f64,
    pub screen_height: f64,
    pub screen_scale: f64,
}

/// Maps a short privacy service name (`photos`, `camera`, ...) to its TCC service identifier.
/// Identifiers that already start with `kTCCService` pass through unchanged.
pub fn privacy_service(name: &str) -> Option<String> {
    if name.starts_with("kTCCService") {
        return Some(name.to_owned());
    }
    let id = match name {
        "all" => "All",
        "calendar" => "Calendar",
        "contacts" => "AddressBook",
        "contacts-limited" => "ContactsLimited",
        "photos" => "Photos",
        "photos-add" => "PhotosAdd",
        "media-library" => "MediaLibrary",
        "microphone" => "Microphone",
        "camera" => "Camera",
        "motion" => "Motion",
        "reminders" => "Reminders",
        "siri" => "Siri",
        "speech" => "SpeechRecognition",
        "bluetooth" => "BluetoothAlways",
        "focus" => "FocusStatus",
        _ => return None,
    };
    Some(format!("kTCCService{id}"))
}

/// How a user-supplied device reference matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    One(usize),
    None,
    Ambiguous,
}

/// Resolves `query` against devices: exact UDID (case-insensitive) first, then `booted` (the only
/// booted device), then exact name. A name shared by several devices prefers the single booted one.
pub fn select(devices: &[DeviceInfo], query: &str) -> Selection {
    if let Some(i) = devices.iter().position(|d| d.udid.eq_ignore_ascii_case(query)) {
        return Selection::One(i);
    }
    let pick = |mut it: Box<dyn Iterator<Item = usize> + '_>| match (it.next(), it.next()) {
        (Some(i), None) => Selection::One(i),
        (None, _) => Selection::None,
        _ => Selection::Ambiguous,
    };
    let booted = |d: &DeviceInfo| d.state == DeviceState::Booted;
    if query.eq_ignore_ascii_case("booted") {
        return pick(Box::new(devices.iter().enumerate().filter(|(_, d)| booted(d)).map(|(i, _)| i)));
    }
    let named = |d: &DeviceInfo| d.name == query;
    match pick(Box::new(devices.iter().enumerate().filter(|(_, d)| named(d)).map(|(i, _)| i))) {
        Selection::Ambiguous => {
            pick(Box::new(devices.iter().enumerate().filter(|(_, d)| named(d) && booted(d)).map(|(i, _)| i))).or_ambiguous()
        }
        other => other,
    }
}

impl Selection {
    fn or_ambiguous(self) -> Self {
        if self == Self::None { Self::Ambiguous } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(udid: &str, name: &str, state: DeviceState) -> DeviceInfo {
        DeviceInfo { udid: udid.into(), name: name.into(), state, runtime_id: "r".into(), device_type_id: "t".into(), available: true }
    }

    #[test]
    fn privacy_names_map() {
        assert_eq!(privacy_service("photos").as_deref(), Some("kTCCServicePhotos"));
        assert_eq!(privacy_service("contacts").as_deref(), Some("kTCCServiceAddressBook"));
        assert_eq!(privacy_service("kTCCServiceCamera").as_deref(), Some("kTCCServiceCamera"));
        assert_eq!(privacy_service("nope"), None);
    }

    #[test]
    fn raw_states_map() {
        assert_eq!(DeviceState::from_raw(3), DeviceState::Booted);
        assert_eq!(DeviceState::from_raw(1), DeviceState::Shutdown);
        assert_eq!(DeviceState::from_raw(99), DeviceState::Unknown);
    }

    #[test]
    fn selects_by_udid_name_and_booted() {
        let ds = [
            dev("AAAA", "iPhone", DeviceState::Shutdown),
            dev("BBBB", "iPhone", DeviceState::Booted),
            dev("CCCC", "iPad", DeviceState::Shutdown),
        ];
        assert_eq!(select(&ds, "aaaa"), Selection::One(0));
        assert_eq!(select(&ds, "booted"), Selection::One(1));
        assert_eq!(select(&ds, "iPad"), Selection::One(2));
        assert_eq!(select(&ds, "iPhone"), Selection::One(1));
        assert_eq!(select(&ds, "nope"), Selection::None);
    }

    #[test]
    fn ambiguous_when_nothing_disambiguates() {
        let ds = [dev("A", "x", DeviceState::Shutdown), dev("B", "x", DeviceState::Shutdown)];
        assert_eq!(select(&ds, "x"), Selection::Ambiguous);
        assert_eq!(select(&ds, "booted"), Selection::None);
        let both = [dev("A", "x", DeviceState::Booted), dev("B", "y", DeviceState::Booted)];
        assert_eq!(select(&both, "booted"), Selection::Ambiguous);
    }
}
