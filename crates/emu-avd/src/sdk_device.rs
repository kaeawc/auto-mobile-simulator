//! The SDK's device definitions (`sdklib/.../devices/{wear,tv,automotive,xr}.xml` in
//! `platform/tools/base`) that new AVDs of each form factor follow, and the `config.ini` keys
//! avdmanager writes for them (`HardwareProperties.getHardwareProperties`). Only the hardware
//! that reaches the guest is kept: sensors, navigation, buttons, touch, battery, RAM.

/// A sensor in a device definition's `<d:sensors>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    Accelerometer,
    Barometer,
    Compass,
    Gps,
    Gyroscope,
    Light,
    Proximity,
}

/// `<d:nav>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    None,
    Dpad,
    Trackball,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdkDevice {
    /// `<d:id>`, written as `hw.device.name`.
    pub id: &'static str,
    pub sensors: &'static [Sensor],
    /// `<d:mechanism>` other than `notouch`.
    pub touch: bool,
    /// `<d:buttons>hard</d:buttons>`: Back/Home are hardware keys, no navigation bar.
    pub hard_buttons: bool,
    pub nav: Nav,
    /// `<d:power-type>battery</d:power-type>` (else plugged in).
    pub battery: bool,
    /// `<d:ram>` in MiB, as avdmanager rounds it.
    pub ram_mb: u64,
    /// `<d:touchpad>` width and height (display glasses' temple touchpad).
    pub touchpad: Option<(u32, u32)>,
}

use Sensor::*;

/// `wear.xml`: the large round watch, the size emu-rs's Wear profile has (454x454).
pub const WEAR_LARGE_ROUND: SdkDevice = SdkDevice {
    id: "wearos_large_round",
    sensors: &[Accelerometer, Barometer, Compass, Gps, Gyroscope, Light, Proximity],
    touch: true,
    hard_buttons: true,
    nav: Nav::None,
    battery: true,
    ram_mb: 512,
    touchpad: None,
};

/// `tv.xml`: no touch screen and no motion sensors; a D-pad remote.
pub const TV_1080P: SdkDevice = SdkDevice {
    id: "tv_1080p",
    sensors: &[Light, Gps],
    touch: false,
    hard_buttons: true,
    nav: Nav::Dpad,
    battery: false,
    ram_mb: 2048,
    touchpad: None,
};

/// `automotive.xml`: the 1024x768 head unit, on-screen navigation, mains power.
pub const AUTOMOTIVE_1024P_LANDSCAPE: SdkDevice = SdkDevice {
    id: "automotive_1024p_landscape",
    sensors: &[Light, Gps],
    touch: true,
    hard_buttons: false,
    nav: Nav::None,
    battery: false,
    // 3774492 KiB.
    ram_mb: 3686,
    touchpad: None,
};

/// `xr.xml`: the headset.
pub const XR_HEADSET: SdkDevice = SdkDevice {
    id: "xr_headset_device",
    sensors: &[Accelerometer, Barometer, Compass, Gps, Gyroscope, Light, Proximity],
    touch: true,
    hard_buttons: false,
    nav: Nav::Trackball,
    battery: true,
    ram_mb: 4096,
    touchpad: None,
};

/// `xr.xml`: display glasses, driven from a touchpad on the temple.
pub const AI_GLASSES: SdkDevice = SdkDevice {
    id: "ai_glasses_device",
    sensors: &[Accelerometer, Compass, Gyroscope, Light, Proximity],
    touch: true,
    hard_buttons: false,
    nav: Nav::None,
    battery: true,
    ram_mb: 2048,
    touchpad: Some((1542, 297)),
};

impl SdkDevice {
    /// The `config.ini` keys avdmanager derives from the definition, in its own spelling. The
    /// orientation sensor follows the gyroscope, as avdmanager has it.
    pub fn config(&self) -> Vec<(String, String)> {
        let yes_no = |b: bool| if b { "yes" } else { "no" }.to_owned();
        let has = |s: Sensor| yes_no(self.sensors.contains(&s));
        let mut out = vec![("hw.device.name".to_owned(), self.id.to_owned())];
        if !self.touch {
            out.push(("hw.screen".to_owned(), "no-touch".to_owned()));
        }
        out.extend(
            [
                ("hw.mainKeys", yes_no(self.hard_buttons)),
                ("hw.trackBall", yes_no(self.nav == Nav::Trackball)),
                ("hw.dPad", yes_no(self.nav == Nav::Dpad)),
                ("hw.gps", has(Gps)),
                ("hw.battery", yes_no(self.battery)),
                ("hw.accelerometer", has(Accelerometer)),
                ("hw.sensors.orientation", has(Gyroscope)),
                ("hw.gyroscope", has(Gyroscope)),
                ("hw.sensors.magnetic_field", has(Compass)),
                ("hw.sensors.pressure", has(Barometer)),
                ("hw.sensors.light", has(Light)),
                ("hw.audioInput", yes_no(true)),
                ("hw.sensors.proximity", has(Proximity)),
            ]
            .map(|(k, v)| (k.to_owned(), v)),
        );
        if let Some((w, h)) = self.touchpad {
            out.extend(
                [("hw.touchpad0", "yes".to_owned()), ("hw.touchpad0.width", w.to_string()), ("hw.touchpad0.height", h.to_string())]
                    .map(|(k, v)| (k.to_owned(), v)),
            );
        }
        out
    }
}
