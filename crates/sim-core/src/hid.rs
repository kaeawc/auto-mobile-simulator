//! Hardware buttons and keyboard usages (USB HID usage page 0x07).

/// A hardware button, on either platform. Each platform has a subset: [`Button::apple_source`] and
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
}

impl Button {
    pub const ALL: [Button; 15] = [
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
        }
    }

    /// The event source code the Apple simulator's HID service uses for this button, if it has one.
    pub fn apple_source(self) -> Option<u32> {
        Some(match self {
            Self::Home => 0x0,
            Self::Lock => 0x1,
            Self::Side => 0xbb8,
            Self::Siri => 0x40_0002,
            Self::ApplePay => 0x1f4,
            _ => return None,
        })
    }

    /// The Android emulator console's key name for this button (`event send EV_KEY:<name>:1`), if
    /// Android has it. D-pad center is `KEY_SELECT` (353), which Android maps to DPAD_CENTER.
    pub fn android_key(self) -> Option<&'static str> {
        Some(match self {
            Self::Home => "HOME",
            Self::Lock | Self::Side => "POWER",
            Self::Back => "BACK",
            Self::VolumeUp => "VOLUMEUP",
            Self::VolumeDown => "VOLUMEDOWN",
            Self::Recents => "APPSELECT",
            Self::Menu => "MENU",
            Self::DpadUp => "UP",
            Self::DpadDown => "DOWN",
            Self::DpadLeft => "LEFT",
            Self::DpadRight => "RIGHT",
            Self::DpadCenter => "353",
            Self::Siri | Self::ApplePay => return None,
        })
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
        0x35 => 41,
        0x36 => 51,
        0x37 => 52,
        0x38 => 53,
        0x39 => 58,
        0x3a..=0x43 => 59 + (usage - 0x3a) as u16,
        0x44 => 87,
        0x45 => 88,
        0x4a => 102,
        0x4b => 104,
        0x4c => 111,
        0x4d => 107,
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
        assert_eq!(Button::Side.apple_source(), Some(0xbb8));
        for b in Button::ALL {
            assert_eq!(Button::parse(b.as_str()), Some(b));
            assert!(b.apple_source().is_some() || b.android_key().is_some(), "{b:?} exists somewhere");
        }
    }

    #[test]
    fn buttons_map_per_platform() {
        assert_eq!((Button::Back.apple_source(), Button::Back.android_key()), (None, Some("BACK")));
        assert_eq!((Button::Siri.apple_source(), Button::Siri.android_key()), (Some(0x40_0002), None));
        assert_eq!(Button::Lock.android_key(), Some("POWER"));
        assert_eq!(Button::DpadCenter.android_key(), Some("353"));
    }

    #[test]
    fn hid_usages_map_to_linux_keycodes() {
        let code = |c: char| keystroke(c).and_then(|k| linux_keycode(k.usage));
        // The Linux codes the Android console names (emu-vmm's `keycode`): Q=16, A=30, Z=44, 1=2, 0=11.
        assert_eq!([code('q'), code('a'), code('z'), code('1'), code('0')], [Some(16), Some(30), Some(44), Some(2), Some(11)]);
        assert_eq!([code('m'), code(' '), code('\n'), code('/')], [Some(50), Some(57), Some(28), Some(53)]);
        assert_eq!(linux_keycode(KEY_LEFT_SHIFT), Some(42));
        assert_eq!(linux_keycode(0x50), Some(105));
        assert_eq!(linux_keycode(0x32), None);
        // Every key a US-layout character can need has a Linux code.
        for c in (' '..='~').chain(['\n', '\t']) {
            if let Some(k) = keystroke(c) {
                assert!(linux_keycode(k.usage).is_some(), "{c:?}");
            }
        }
    }
}
