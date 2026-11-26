//! Which of an Apple simulator's framebuffer ports is its main display.
//!
//! A device's I/O ports include one framebuffer per screen. Most devices have a single class-0
//! (built-in) framebuffer; Apple TV renders only to its TVOut port (class 1). The iPhone Duo
//! (iOS 27.1 runtimes on) has two built-in screens, the outer one (`LCD`, the device type's main
//! screen, which `simctl io` calls `primary`) and the inner one (`LCD-1`), and the inner one comes
//! first in port order; folded, it stays black.

/// What is known of one framebuffer port, in port order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Port {
    /// The display class: 0 built-in, 1 external (TVOut). `None` when the port doesn't say.
    pub class: Option<u16>,
    /// The surface's size in pixels, once it has rendered one.
    pub size: Option<(u32, u32)>,
}

/// The main display's index: the built-in (class-0) port whose surface is the device type's main
/// screen (`screen`, in pixels, either orientation) when there are several, else the first
/// built-in port, else the first port. `None` without ports.
pub fn main_port(ports: &[Port], screen: Option<(u32, u32)>) -> Option<usize> {
    let builtin: Vec<usize> = (0..ports.len()).filter(|&i| ports[i].class == Some(0)).collect();
    if builtin.len() > 1
        && let Some((w, h)) = screen
        && let Some(&i) = builtin.iter().find(|&&i| ports[i].size.is_some_and(|s| s == (w, h) || s == (h, w)))
    {
        return Some(i);
    }
    builtin.first().copied().or(if ports.is_empty() { None } else { Some(0) })
}

/// The second built-in screen's index (the iPhone Duo's inner screen): the built-in port that
/// isn't [`main_port`]. `None` for a device with one built-in screen.
pub fn inner_port(ports: &[Port], screen: Option<(u32, u32)>) -> Option<usize> {
    let main = main_port(ports, screen)?;
    (0..ports.len()).find(|&i| i != main && ports[i].class == Some(0))
}

/// Whether the main display can only be chosen by its size: more than one built-in port.
pub fn needs_sizes(classes: &[Option<u16>]) -> bool {
    classes.iter().filter(|c| **c == Some(0)).count() > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(class: u16, size: Option<(u32, u32)>) -> Port {
        Port { class: Some(class), size }
    }

    #[test]
    fn a_single_built_in_screen_is_main_wherever_it_is() {
        let ports = [port(1, None), port(0, Some((1179, 2556))), port(1, None)];
        assert_eq!(main_port(&ports, Some((1179, 2556))), Some(1));
        assert_eq!(main_port(&ports, None), Some(1));
        // The size isn't checked with one built-in screen: a rotated or odd surface still wins.
        assert_eq!(main_port(&ports, Some((1, 1))), Some(1));
    }

    #[test]
    fn the_iphone_duo_s_main_screen_is_the_outer_one_not_the_first() {
        // As an iPhone Duo on iOS 27.1 lists them: the inner screen (2007x2853) comes first.
        let ports = [port(1, None), port(1, None), port(0, Some((2007, 2853))), port(1, None), port(0, Some((1398, 2034)))];
        assert_eq!(main_port(&ports, Some((1398, 2034))), Some(4));
        assert_eq!(main_port(&ports, Some((2034, 1398))), Some(4), "either orientation");
        // Without the device type's size, or with no match, the first built-in port, as before.
        assert_eq!(main_port(&ports, None), Some(2));
        assert_eq!(main_port(&ports, Some((100, 100))), Some(2));
        let unrendered = [port(0, None), port(0, None)];
        assert_eq!(main_port(&unrendered, Some((1398, 2034))), Some(0));
    }

    #[test]
    fn the_iphone_duo_s_inner_screen_is_the_other_built_in_port() {
        let ports = [port(1, None), port(1, None), port(0, Some((2007, 2853))), port(1, None), port(0, Some((1398, 2034)))];
        assert_eq!(inner_port(&ports, Some((1398, 2034))), Some(2));
        // Unrendered or unmatched, the main port is the first built-in one and the inner the next.
        assert_eq!(inner_port(&ports, None), Some(4));
        // One built-in screen (an iPhone 17 Pro, an iPad, Apple TV's TVOut): no inner screen.
        assert_eq!(inner_port(&[port(1, None), port(0, Some((1206, 2622))), port(1, None)], Some((1206, 2622))), None);
        assert_eq!(inner_port(&[port(1, Some((3840, 2160)))], None), None);
        assert_eq!(inner_port(&[], None), None);
    }

    #[test]
    fn apple_tv_renders_to_its_only_port() {
        assert_eq!(main_port(&[port(1, Some((3840, 2160)))], Some((3840, 2160))), Some(0));
        assert_eq!(main_port(&[Port::default()], None), Some(0));
        assert_eq!(main_port(&[], None), None);
    }

    #[test]
    fn sizes_are_read_only_with_several_built_in_screens() {
        assert!(!needs_sizes(&[Some(1), Some(0), Some(1)]));
        assert!(!needs_sizes(&[Some(1)]));
        assert!(needs_sizes(&[Some(1), Some(0), Some(1), Some(0)]));
        assert!(!needs_sizes(&[None, None]));
    }
}
