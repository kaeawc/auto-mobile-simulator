//! Platforms: which engine runs a device (Android emulator or Apple simulator) and the
//! sub-platform it presents (iPadOS, watchOS, Wear OS, Android TV, Android XR, CarPlay, ...).
//!
//! Every device, API method and UI element carries these labels, so a reader always knows which
//! side of the house they are looking at.

use serde::{Deserialize, Serialize};

/// Which backend runs a device. `Ios` is the whole Apple simulator family; the Apple
/// sub-platform (iPadOS, watchOS, ...) is an [`AppleOs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Android,
    Ios,
}

impl Platform {
    pub const ALL: [Platform; 2] = [Platform::Android, Platform::Ios];

    /// Machine name, as used in the API and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Android => "Android",
            Self::Ios => "iOS",
        }
    }

    /// Parses a machine name (case-insensitive); `apple` is accepted for `ios`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "android" => Some(Self::Android),
            "ios" | "apple" => Some(Self::Ios),
            _ => None,
        }
    }
}

/// An Apple sub-platform. CarPlay is not a separate runtime: it is the car head-unit display an
/// iPhone simulator projects onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppleOs {
    Ios,
    IpadOs,
    WatchOs,
    TvOs,
    VisionOs,
    CarPlay,
}

impl AppleOs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::IpadOs => "ipados",
            Self::WatchOs => "watchos",
            Self::TvOs => "tvos",
            Self::VisionOs => "visionos",
            Self::CarPlay => "carplay",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ios => "iOS",
            Self::IpadOs => "iPadOS",
            Self::WatchOs => "watchOS",
            Self::TvOs => "tvOS",
            Self::VisionOs => "visionOS",
            Self::CarPlay => "CarPlay",
        }
    }
}

/// An Android sub-platform: the system image family a device boots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AndroidOs {
    /// Phones, tablets and foldables (`default`, `google_apis`, `google_apis_playstore` images).
    #[serde(rename = "android")]
    Mobile,
    /// `android-wear` images.
    WearOs,
    /// `android-tv` and `google-tv` images.
    #[serde(rename = "androidtv")]
    Tv,
    /// Android Automotive OS (`android-automotive*` images): the OS runs on the head unit itself.
    /// Android Auto, by contrast, projects from a phone; the emulator boots Automotive images.
    Automotive,
    /// Android XR headsets and glasses (`google-xr`, `android-xr-*` and `ai-glasses` images).
    #[serde(rename = "androidxr")]
    Xr,
}

impl AndroidOs {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mobile => "android",
            Self::WearOs => "wearos",
            Self::Tv => "androidtv",
            Self::Automotive => "automotive",
            Self::Xr => "androidxr",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mobile => "Android",
            Self::WearOs => "Wear OS",
            Self::Tv => "Android TV",
            Self::Automotive => "Android Automotive",
            Self::Xr => "Android XR",
        }
    }
}

/// The sub-platform of either family. Serialized as its machine name (`ipados`, `wearos`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubPlatform {
    Apple(AppleOs),
    Android(AndroidOs),
}

impl SubPlatform {
    /// The family this sub-platform belongs to.
    pub fn platform(self) -> Platform {
        match self {
            Self::Apple(_) => Platform::Ios,
            Self::Android(_) => Platform::Android,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apple(os) => os.as_str(),
            Self::Android(os) => os.as_str(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Apple(os) => os.label(),
            Self::Android(os) => os.label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for p in Platform::ALL {
            assert_eq!(Platform::parse(p.as_str()), Some(p));
            assert_eq!(serde_json::to_value(p).expect("json"), p.as_str());
        }
        assert_eq!(Platform::parse("Apple"), Some(Platform::Ios));
        assert_eq!(Platform::parse("windows"), None);
        assert_eq!(Platform::Ios.label(), "iOS");
    }

    #[test]
    fn sub_platforms_serialize_by_machine_name() {
        let cases = [
            (SubPlatform::Apple(AppleOs::IpadOs), "ipados", "iPadOS", Platform::Ios),
            (SubPlatform::Apple(AppleOs::CarPlay), "carplay", "CarPlay", Platform::Ios),
            (SubPlatform::Android(AndroidOs::Mobile), "android", "Android", Platform::Android),
            (SubPlatform::Android(AndroidOs::WearOs), "wearos", "Wear OS", Platform::Android),
            (SubPlatform::Android(AndroidOs::Tv), "androidtv", "Android TV", Platform::Android),
            (SubPlatform::Android(AndroidOs::Xr), "androidxr", "Android XR", Platform::Android),
            (SubPlatform::Apple(AppleOs::VisionOs), "visionos", "visionOS", Platform::Ios),
        ];
        for (sub, name, label, platform) in cases {
            assert_eq!(serde_json::to_value(sub).expect("json"), name);
            assert_eq!(serde_json::from_value::<SubPlatform>(name.into()).expect("parse"), sub);
            assert_eq!((sub.as_str(), sub.label(), sub.platform()), (name, label, platform));
        }
    }
}
