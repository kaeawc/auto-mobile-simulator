//! Hardware buttons and keyboard usages (USB HID usage page 0x07).

/// A hardware button on the simulated device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Home,
    Lock,
    Side,
    Siri,
    ApplePay,
}

impl Button {
    /// Parses a CLI name.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "home" => Self::Home,
            "lock" | "power" => Self::Lock,
            "side" => Self::Side,
            "siri" => Self::Siri,
            "apple-pay" | "applepay" => Self::ApplePay,
            _ => return None,
        })
    }

    /// The event source code the HID service uses for this button.
    pub fn source(self) -> u32 {
        match self {
            Self::Home => 0x0,
            Self::Lock => 0x1,
            Self::Side => 0xbb8,
            Self::Siri => 0x40_0002,
            Self::ApplePay => 0x1f4,
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
        assert_eq!(Button::Side.source(), 0xbb8);
    }
}
