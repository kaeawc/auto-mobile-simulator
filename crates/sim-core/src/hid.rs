//! Hardware buttons, keyboard usages (USB HID usage page 0x07), and the per-form-factor inputs
//! that aren't buttons: the Digital Crown, watch gestures and device orientation.

use crate::form_factor::FormFactor;

/// A hardware button, on either platform. Each platform has a subset: [`Button::apple_press`] and
/// [`Button::android_key`] say whether (and how) a platform presses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Home,
    /// Lock / power / sleep-wake.
    Lock,
    Side,
    Siri,
    ApplePay,
    Back,
    VolumeUp,
    VolumeDown,
    /// Android's overview (recent apps).
    Recents,
    Menu,
    /// TV remotes.
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
    DpadCenter,
    /// TV remotes' play/pause.
    PlayPause,
    /// The iPhone's (and Apple Watch Ultra's) Action button.
    Action,
}

impl Button {
    pub const ALL: [Button; 17] = [
        Self::Home,
        Self::Lock,
        Self::Side,
        Self::Siri,
        Self::ApplePay,
        Self::Back,
        Self::VolumeUp,
        Self::VolumeDown,
        Self::Recents,
        Self::Menu,
        Self::DpadUp,
        Self::DpadDown,
        Self::DpadLeft,
        Self::DpadRight,
        Self::DpadCenter,
        Self::PlayPause,
        Self::Action,
    ];

    /// Parses a CLI name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "home" => Self::Home,
            "lock" | "power" => Self::Lock,
            "side" => Self::Side,
            "siri" => Self::Siri,
            "apple-pay" | "applepay" => Self::ApplePay,
            "back" => Self::Back,
            "volume-up" | "volumeup" => Self::VolumeUp,
            "volume-down" | "volumedown" => Self::VolumeDown,
            "recents" | "overview" | "app-switch" => Self::Recents,
            "menu" => Self::Menu,
            "up" | "dpad-up" => Self::DpadUp,
            "down" | "dpad-down" => Self::DpadDown,
            "left" | "dpad-left" => Self::DpadLeft,
            "right" | "dpad-right" => Self::DpadRight,
            "select" | "center" | "dpad-center" => Self::DpadCenter,
            "play-pause" | "playpause" | "play" => Self::PlayPause,
            "action" => Self::Action,
            _ => return None,
        })
    }

    /// The CLI name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Lock => "lock",
            Self::Side => "side",
            Self::Siri => "siri",
            Self::ApplePay => "apple-pay",
            Self::Back => "back",
            Self::VolumeUp => "volume-up",
            Self::VolumeDown => "volume-down",
            Self::Recents => "recents",
            Self::Menu => "menu",
            Self::DpadUp => "up",
            Self::DpadDown => "down",
            Self::DpadLeft => "left",
            Self::DpadRight => "right",
            Self::DpadCenter => "select",
            Self::PlayPause => "play-pause",
            Self::Action => "action",
        }
    }

    /// How the Apple simulator's HID service presses this button on a device of `form_factor`, if
    /// that device has it. Apple TV's buttons are the Siri Remote's (its own target, released with
    /// 0); its arrows aren't buttons but keys ([`Button::apple_remote_key`]). Everywhere else they
    /// are hardware buttons: on a watch, home is the Digital Crown's press.
    pub fn apple_press(self, form_factor: Option<FormFactor>) -> Option<ApplePress> {
        if form_factor == Some(FormFactor::Tv) {
            let code = match self {
                Self::DpadCenter => 0x190,
                Self::Menu | Self::Back => 0x191,
                Self::Home => 0x192,
                Self::PlayPause => 0x194,
                Self::VolumeUp => 0x195,
                Self::VolumeDown => 0x196,
                Self::Siri => 0x3f2,
                _ => return None,
            };
            return Some(ApplePress { source: code, target: TARGET_REMOTE, down: 1, up: 0 });
        }
        let source = match (self, form_factor) {
            (Self::Home, _) => 0x0,
            (Self::Lock, _) => 0x1,
            (Self::Side, _) => 0xbb8,
            (Self::Siri, _) => 0x40_0002,
            (Self::ApplePay, _) => 0x1f4,
            (Self::Action, Some(FormFactor::Watch)) => 3001,
            (Self::Action, _) => 3002,
            _ => return None,
        };
        Some(ApplePress { source, target: TARGET_HARDWARE, down: 1, up: 2 })
    }

    /// The keyboard usage the Apple TV simulator takes for a Siri Remote arrow (it maps the
    /// keyboard's arrows to the remote's D-pad).
    pub fn apple_remote_key(self) -> Option<u32> {
        Some(match self {
            Self::DpadRight => 0x4f,
            Self::DpadLeft => 0x50,
            Self::DpadDown => 0x51,
            Self::DpadUp => 0x52,
            _ => return None,
        })
    }

    /// Whether some Apple device has this button.
    pub fn on_apple(self) -> bool {
        [None, Some(FormFactor::Tv), Some(FormFactor::Watch)].into_iter().any(|f| self.apple_press(f).is_some())
            || self.apple_remote_key().is_some()
    }

    /// The Android emulator console's key for this button (`event send EV_KEY:<key>:1`), if
    /// Android has it: a stock console name, so stock emulators take it too, or a keycode where
    /// stock has no name. The codes are what goldfish's `qwerty.kl` maps (the layout its `qwerty2`
    /// keyboard's `.idc` names; checked against TV 34's vendor image): `KEY_CENTER` (232) is
    /// DPAD_CENTER, 580 (Linux `KEY_APPSELECT`) is APP_SWITCH, `KEY_PLAYPAUSE` (164) is
    /// MEDIA_PLAY_PAUSE.
    pub fn android_key(self) -> Option<&'static str> {
        Some(match self {
            Self::Home => "KEY_HOME",
            Self::Lock | Self::Side => "KEY_POWER",
            Self::Back => "KEY_BACK",
            Self::VolumeUp => "KEY_VOLUMEUP",
            Self::VolumeDown => "KEY_VOLUMEDOWN",
            Self::Recents => "580",
            Self::Menu => "KEY_MENU",
            Self::DpadUp => "KEY_UP",
            Self::DpadDown => "KEY_DOWN",
            Self::DpadLeft => "KEY_LEFT",
            Self::DpadRight => "KEY_RIGHT",
            Self::DpadCenter => "KEY_CENTER",
            Self::PlayPause => "KEY_PLAYPAUSE",
            Self::Siri | Self::ApplePay | Self::Action => return None,
        })
    }
}

/// The Apple simulator HID target for hardware buttons.
pub const TARGET_HARDWARE: u32 = 0x33;
/// The Apple simulator HID target for the Siri Remote's buttons.
pub const TARGET_REMOTE: u32 = 0x15;

/// One Apple button press: the event source, the HID target, and the direction values meaning
/// down and up for that target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplePress {
    pub source: u32,
    pub target: u32,
    pub down: u32,
    pub up: u32,
}

impl ApplePress {
    /// The direction value for `direction`.
    pub fn value(&self, direction: Direction) -> u32 {
        match direction {
            Direction::Down => self.down,
            Direction::Up => self.up,
        }
    }
}

/// Key or button direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Down,
    Up,
}

/// Left shift usage.
pub const KEY_LEFT_SHIFT: u32 = 0xe1;
/// Return usage.
pub const KEY_RETURN: u32 = 0x28;

/// The Linux input keycode (`KEY_*`, what Android's virtio keyboard takes) for a HID keyboard usage,
/// so one key map ([`keystroke`]) types on both platforms. `None` for usages without one.
///
/// The guest reads them through goldfish's `qwerty2.kl`, where grave is 399 (`KEY_GREEN`), 102
/// and 107 are HOME and ENDCALL, and the text cursor's Home and End are 452 and 453.
pub fn linux_keycode(usage: u32) -> Option<u16> {
    // a..z in usage order (0x04..=0x1d).
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    Some(match usage {
        0x04..=0x1d => LETTERS[(usage - 0x04) as usize],
        0x1e..=0x26 => 2 + (usage - 0x1e) as u16,
        0x27 => 11,
        0x28 => 28,
        0x29 => 1,
        0x2a => 14,
        0x2b => 15,
        0x2c => 57,
        0x2d => 12,
        0x2e => 13,
        0x2f => 26,
        0x30 => 27,
        0x31 => 43,
        0x33 => 39,
        0x34 => 40,
        0x35 => 399,
        0x36 => 51,
        0x37 => 52,
        0x38 => 53,
        0x39 => 58,
        0x3a..=0x43 => 59 + (usage - 0x3a) as u16,
        0x44 => 87,
        0x45 => 88,
        0x4a => 452,
        0x4b => 104,
        0x4c => 111,
        0x4d => 453,
        0x4e => 109,
        0x4f => 106,
        0x50 => 105,
        0x51 => 108,
        0x52 => 103,
        0xe0 => 29,
        0xe1 => 42,
        0xe2 => 56,
        0xe3 => 125,
        0xe4 => 97,
        0xe5 => 54,
        0xe6 => 100,
        0xe7 => 126,
        _ => return None,
    })
}

/// One key press: the usage and whether shift is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyStroke {
    pub usage: u32,
    pub shift: bool,
}

/// Maps a character to a US-layout key stroke. `None` for characters with no key.
pub fn keystroke(c: char) -> Option<KeyStroke> {
    let plain = |usage| Some(KeyStroke { usage, shift: false });
    let shifted = |usage| Some(KeyStroke { usage, shift: true });
    match c {
        'a'..='z' => plain(0x04 + (c as u32 - 'a' as u32)),
        'A'..='Z' => shifted(0x04 + (c as u32 - 'A' as u32)),
        '1'..='9' => plain(0x1e + (c as u32 - '1' as u32)),
        '0' => plain(0x27),
        '\n' | '\r' => plain(KEY_RETURN),
        '\u{1b}' => plain(0x29),
        '\u{8}' | '\u{7f}' => plain(0x2a),
        '\t' => plain(0x2b),
        ' ' => plain(0x2c),
        _ => {
            const UNSHIFTED: &str = "-=[]\\#;'`,./";
            const SHIFTED: &str = "_+{}|~:\"~<>?";
            const DIGIT_SHIFTED: &str = "!@#$%^&*()";
            if let Some(i) = DIGIT_SHIFTED.find(c) {
                return shifted(0x1e + i as u32);
            }
            // 0x32 (non-US #) is skipped on input; `#` and the first `~` resolve above / below.
            if let Some(i) = UNSHIFTED.find(c).filter(|&i| i != 5) {
                return plain(0x2d + i as u32);
            }
            SHIFTED.char_indices().find(|&(i, s)| s == c && i != 5).and_then(|(i, _)| shifted(0x2d + i as u32))
        }
    }
}

/// The largest Digital Crown delta one HID message carries; bigger turns are split. Simulator.app
/// sends the scroll wheel's delta per event, and larger single deltas are dropped by the guest.
pub const CROWN_MAX_STEP: f64 = 200.0;
/// Crown units per rotary tick, so `rotary <ticks>` means about the same turn on a Wear OS watch
/// (one detent per tick) and an Apple Watch. A calibration, not an Apple constant.
pub const CROWN_UNITS_PER_TICK: f64 = 10.0;

/// Splits a Digital Crown turn into per-message deltas of at most [`CROWN_MAX_STEP`], keeping the
/// sign (positive turns scroll content up, like turning the crown away from you).
pub fn crown_steps(delta: f64) -> Vec<f64> {
    if !delta.is_finite() || delta == 0.0 {
        return Vec::new();
    }
    let n = (delta.abs() / CROWN_MAX_STEP).ceil().max(1.0) as usize;
    vec![delta / n as f64; n]
}

/// The Apple Watch gestures the simulator sends as one message each (watchOS 11+; Wrist Flick
/// needs watchOS 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchGesture {
    DoubleTap,
    WristFlick,
}

impl WatchGesture {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "double-tap" | "doubletap" => Some(Self::DoubleTap),
            "wrist-flick" | "wristflick" => Some(Self::WristFlick),
            _ => None,
        }
    }

    /// The simulator's gesture code.
    pub fn apple_code(self) -> u32 {
        match self {
            Self::DoubleTap => 0,
            Self::WristFlick => 1,
        }
    }
}

/// A device orientation, named as the device reports it (UIDeviceOrientation): landscape-left has
/// the top of the device on the left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    LandscapeRight,
    PortraitUpsideDown,
    LandscapeLeft,
}

impl Orientation {
    /// In the order turning the device clockwise visits them, from portrait (turning it clockwise
    /// puts its top on the right: landscape-right).
    pub const ALL: [Orientation; 4] = [Self::Portrait, Self::LandscapeRight, Self::PortraitUpsideDown, Self::LandscapeLeft];

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "portrait" => Some(Self::Portrait),
            "landscape-left" | "landscapeleft" | "landscape" => Some(Self::LandscapeLeft),
            "portrait-upside-down" | "upside-down" | "pud" => Some(Self::PortraitUpsideDown),
            "landscape-right" | "landscaperight" => Some(Self::LandscapeRight),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::LandscapeLeft => "landscape-left",
            Self::PortraitUpsideDown => "portrait-upside-down",
            Self::LandscapeRight => "landscape-right",
        }
    }

    /// The next orientation after rotating the device 90° clockwise (what Android's `rotate` does).
    pub fn clockwise(self) -> Self {
        let i = Self::ALL.iter().position(|o| *o == self).unwrap_or(0);
        Self::ALL[(i + 1) % 4]
    }

    /// The value the iOS simulator's orientation event (a GSEvent to SpringBoard's
    /// PurpleWorkspacePort) carries. It numbers landscape from the interface's point of view, so the
    /// device's landscape-left is its landscape-right (3) and vice versa.
    pub fn purple_value(self) -> u32 {
        match self {
            Self::Portrait => 1,
            Self::PortraitUpsideDown => 2,
            Self::LandscapeLeft => 3,
            Self::LandscapeRight => 4,
        }
    }

    /// The whole orientation event Simulator.app sends to `port` (SpringBoard's
    /// PurpleWorkspacePort): a 108-byte mach message (msgh_bits COPY_SEND, msgh_id 0x7B) in a
    /// 112-byte buffer, whose GSEvent type is DeviceOrientationChanged (50) with the host flag,
    /// carrying one 4-byte record: [`Orientation::purple_value`].
    pub fn purple_message(self, port: u32) -> [u8; 112] {
        let mut buf = [0u8; 112];
        let mut put = |offset: usize, value: u32| buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        put(0x00, 0x13);
        put(0x04, 108);
        put(0x08, port);
        put(0x14, 0x7b);
        put(0x18, 50 | 0x2_0000);
        put(0x48, 4);
        put(0x4c, self.purple_value());
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_symbols() {
        assert_eq!(keystroke('a'), Some(KeyStroke { usage: 0x04, shift: false }));
        assert_eq!(keystroke('Z'), Some(KeyStroke { usage: 0x1d, shift: true }));
        assert_eq!(keystroke('1'), Some(KeyStroke { usage: 0x1e, shift: false }));
        assert_eq!(keystroke('0'), Some(KeyStroke { usage: 0x27, shift: false }));
        assert_eq!(keystroke('!'), Some(KeyStroke { usage: 0x1e, shift: true }));
        assert_eq!(keystroke('#'), Some(KeyStroke { usage: 0x20, shift: true }));
        assert_eq!(keystroke(')'), Some(KeyStroke { usage: 0x27, shift: true }));
        assert_eq!(keystroke('-'), Some(KeyStroke { usage: 0x2d, shift: false }));
        assert_eq!(keystroke('/'), Some(KeyStroke { usage: 0x38, shift: false }));
        assert_eq!(keystroke('?'), Some(KeyStroke { usage: 0x38, shift: true }));
        assert_eq!(keystroke('`'), Some(KeyStroke { usage: 0x35, shift: false }));
        assert_eq!(keystroke('~'), Some(KeyStroke { usage: 0x35, shift: true }));
        assert_eq!(keystroke(';'), Some(KeyStroke { usage: 0x33, shift: false }));
        assert_eq!(keystroke('"'), Some(KeyStroke { usage: 0x34, shift: true }));
        assert_eq!(keystroke(' '), Some(KeyStroke { usage: 0x2c, shift: false }));
        assert_eq!(keystroke('é'), None);
    }

    #[test]
    fn buttons_parse() {
        assert_eq!(Button::parse("HOME"), Some(Button::Home));
        assert_eq!(Button::parse("power"), Some(Button::Lock));
        assert_eq!(Button::parse("x"), None);
        assert_eq!(Button::Side.apple_press(None).map(|p| p.source), Some(0xbb8));
        for b in Button::ALL {
            assert_eq!(Button::parse(b.as_str()), Some(b));
            assert!(b.on_apple() || b.android_key().is_some(), "{b:?} exists somewhere");
        }
    }

    #[test]
    fn buttons_map_per_platform() {
        assert_eq!((Button::Back.apple_press(None), Button::Back.android_key()), (None, Some("KEY_BACK")));
        assert_eq!((Button::Siri.apple_press(None).map(|p| p.source), Button::Siri.android_key()), (Some(0x40_0002), None));
        assert_eq!(Button::PlayPause.android_key(), Some("KEY_PLAYPAUSE"));
        assert_eq!(Button::Lock.android_key(), Some("KEY_POWER"));
        assert_eq!(Button::DpadCenter.android_key(), Some("KEY_CENTER"));
        assert_eq!(Button::Recents.android_key(), Some("580"));
    }

    #[test]
    fn apple_buttons_depend_on_the_form_factor() {
        let tv = Some(FormFactor::Tv);
        // The Siri Remote: its own target, released with 0.
        let select = Button::DpadCenter.apple_press(tv).expect("select");
        assert_eq!((select.source, select.target, select.value(Direction::Down), select.value(Direction::Up)), (0x190, 0x15, 1, 0));
        assert_eq!(Button::Back.apple_press(tv).map(|p| p.source), Some(0x191));
        assert_eq!(Button::Menu.apple_press(tv).map(|p| p.source), Some(0x191));
        assert_eq!(Button::Home.apple_press(tv).map(|p| p.source), Some(0x192));
        assert_eq!(Button::PlayPause.apple_press(tv).map(|p| p.source), Some(0x194));
        assert_eq!(Button::Lock.apple_press(tv), None);
        assert_eq!((Button::DpadUp.apple_press(tv), Button::DpadUp.apple_remote_key()), (None, Some(0x52)));
        // Hardware buttons elsewhere, released with 2.
        let home = Button::Home.apple_press(Some(FormFactor::Watch)).expect("crown press");
        assert_eq!((home.source, home.target, home.value(Direction::Up)), (0, 0x33, 2));
        assert_eq!(Button::Action.apple_press(Some(FormFactor::Watch)).map(|p| p.source), Some(3001));
        assert_eq!(Button::Action.apple_press(Some(FormFactor::Phone)).map(|p| p.source), Some(3002));
        assert_eq!(Button::PlayPause.apple_press(Some(FormFactor::Phone)), None);
        assert!(Button::DpadLeft.on_apple() && !Button::Recents.on_apple());
    }

    #[test]
    fn crown_turns_split_into_bounded_steps() {
        assert_eq!(crown_steps(0.0), Vec::<f64>::new());
        assert_eq!(crown_steps(f64::NAN), Vec::<f64>::new());
        assert_eq!(crown_steps(50.0), vec![50.0]);
        assert_eq!(crown_steps(-200.0), vec![-200.0]);
        let steps = crown_steps(-450.0);
        assert_eq!(steps.len(), 3);
        assert!(steps.iter().all(|s| s.abs() <= CROWN_MAX_STEP && *s < 0.0));
        assert!((steps.iter().sum::<f64>() + 450.0).abs() < 1e-9);
        assert_eq!(WatchGesture::parse("double-tap").map(WatchGesture::apple_code), Some(0));
        assert_eq!(WatchGesture::parse("wrist-flick").map(WatchGesture::apple_code), Some(1));
        assert_eq!(WatchGesture::parse("wave"), None);
    }

    #[test]
    fn orientations_rotate_and_map_to_purple_values() {
        let mut o = Orientation::Portrait;
        for expected in [Orientation::LandscapeRight, Orientation::PortraitUpsideDown, Orientation::LandscapeLeft, Orientation::Portrait] {
            o = o.clockwise();
            assert_eq!(o, expected);
        }
        // The device's landscape-left is the interface's landscape-right (3).
        let values: Vec<u32> = Orientation::ALL.iter().map(|o| o.purple_value()).collect();
        assert_eq!(values, [1, 4, 2, 3]);
        for o in Orientation::ALL {
            assert_eq!(Orientation::parse(o.as_str()), Some(o));
        }
        assert_eq!(Orientation::parse("sideways"), None);
        let m = Orientation::LandscapeLeft.purple_message(0x1234);
        let word = |o: usize| u32::from_le_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]);
        assert_eq!([word(0), word(4), word(8), word(0x14), word(0x18), word(0x48), word(0x4c)], [0x13, 108, 0x1234, 0x7b, 0x2_0032, 4, 3]);
        assert!(m[0x50..].iter().all(|b| *b == 0));
    }

    #[test]
    fn hid_usages_map_to_linux_keycodes() {
        let code = |c: char| keystroke(c).and_then(|k| linux_keycode(k.usage));
        // The Linux codes the Android console names (`goldfish::keycode`): Q=16, A=30, Z=44, 1=2, 0=11.
        assert_eq!([code('q'), code('a'), code('z'), code('1'), code('0')], [Some(16), Some(30), Some(44), Some(2), Some(11)]);
        assert_eq!([code('m'), code(' '), code('\n'), code('/')], [Some(50), Some(57), Some(28), Some(53)]);
        assert_eq!(linux_keycode(KEY_LEFT_SHIFT), Some(42));
        assert_eq!(linux_keycode(0x50), Some(105));
        assert_eq!(linux_keycode(0x32), None);
        // What qwerty2.kl expects for grave and the cursor's Home and End.
        assert_eq!([code('`'), code('~'), linux_keycode(0x4a), linux_keycode(0x4d)], [Some(399), Some(399), Some(452), Some(453)]);
        // Every key a US-layout character can need has a Linux code.
        for c in (' '..='~').chain(['\n', '\t']) {
            if let Some(k) = keystroke(c) {
                assert!(linux_keycode(k.usage).is_some(), "{c:?}");
            }
        }
    }
}
