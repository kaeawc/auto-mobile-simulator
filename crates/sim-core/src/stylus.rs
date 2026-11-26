//! A stylus on an Android touch screen: what the stock emulator's gRPC `PenEvent` carries (the
//! pen's position, pressure, barrel button and eraser end) as samples, and simee's console `pen`
//! command that sends them.
//!
//! Stock's touch screens (`virtio-input-multi-touch.c`) take a pen on the slot after the fingers'
//! with `ABS_MT_TOOL_TYPE` pen (eraser: `MT_TOOL_MAX` and `BTN_TOOL_RUBBER`), the barrel button
//! as `BTN_STYLUS`, and a contact with no pressure is hovering. Stock reaches it only through
//! gRPC (`sendPen`/`streamInputEvent`'s `pen_event`); simee adds the console command, whose words
//! [`PenSample::parse`] reads and [`PenSample::console`] writes.

use serde::{Deserialize, Serialize};

/// What the pen does at a sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PenPhase {
    /// In range above the screen, not touching it (apps see hover events).
    #[default]
    Hover,
    /// Touches the screen.
    Down,
    /// Moves, touching or hovering as it was.
    Move,
    /// Lifts off the screen but stays in range (hovering).
    Up,
    /// Leaves the screen's range.
    Leave,
}

impl PenPhase {
    pub const NAMES: &'static str = "hover, down, move, up, leave";

    pub fn parse(s: &str) -> Option<PenPhase> {
        Some(match s {
            "hover" => Self::Hover,
            "down" => Self::Down,
            "move" => Self::Move,
            "up" => Self::Up,
            "leave" => Self::Leave,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hover => "hover",
            Self::Down => "down",
            Self::Move => "move",
            Self::Up => "up",
            Self::Leave => "leave",
        }
    }
}

/// One pen sample at device pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PenSample {
    pub x: i32,
    pub y: i32,
    pub phase: PenPhase,
    /// 1..=255 while touching; `None` is the screen's ordinary press.
    pub pressure: Option<u8>,
    /// The barrel button is held (Android's `BUTTON_STYLUS_PRIMARY`).
    pub button: bool,
    /// The eraser end (Android's `TOOL_TYPE_ERASER`).
    pub eraser: bool,
}

/// The console command's syntax.
pub const USAGE: &str = "pen <x> <y> [hover|down|move|up|leave] [pressure <1-255>] [button] [eraser]";

impl PenSample {
    pub fn new(x: i32, y: i32, phase: PenPhase) -> PenSample {
        PenSample { x, y, phase, ..PenSample::default() }
    }

    /// The console line (`pen ...`) that sends this sample.
    pub fn console(&self) -> String {
        let mut line = format!("pen {} {} {}", self.x, self.y, self.phase.as_str());
        if let Some(p) = self.pressure {
            line.push_str(&format!(" pressure {p}"));
        }
        if self.button {
            line.push_str(" button");
        }
        if self.eraser {
            line.push_str(" eraser");
        }
        line
    }

    /// Reads the words after `pen`.
    pub fn parse(words: &[&str]) -> Result<PenSample, String> {
        let coord = |i: usize, name: &str| -> Result<i32, String> {
            let w = words.get(i).ok_or_else(|| format!("missing {name} ({USAGE})"))?;
            w.parse::<i32>().ok().filter(|v| *v >= 0).ok_or_else(|| format!("bad {name} '{w}'"))
        };
        let mut s = PenSample::new(coord(0, "x")?, coord(1, "y")?, PenPhase::Hover);
        let mut rest = words.iter().skip(2).copied().peekable();
        if let Some(w) = rest.peek()
            && let Some(phase) = PenPhase::parse(w)
        {
            s.phase = phase;
            rest.next();
        }
        while let Some(w) = rest.next() {
            match w {
                "button" => s.button = true,
                "eraser" => s.eraser = true,
                "pressure" => {
                    let v = rest.next().ok_or("pressure needs a value (1-255)")?;
                    s.pressure = Some(v.parse::<u8>().ok().filter(|p| *p > 0).ok_or_else(|| format!("bad pressure '{v}' (1-255)"))?);
                }
                other => return Err(format!("unknown word '{other}' ({USAGE})")),
            }
        }
        Ok(s)
    }
}

/// A stroke from `from` to `to`: the pen comes into range and touches down at `from`, moves in
/// `steps` steps, lifts at `to` and leaves.
pub fn stroke(from: (i32, i32), to: (i32, i32), steps: u32, button: bool, eraser: bool) -> Vec<PenSample> {
    let steps = steps.max(1);
    let lerp = |a: i32, b: i32, i: u32| (f64::from(a) + f64::from(b - a) * f64::from(i) / f64::from(steps)).round() as i32;
    let at = |x, y, phase| PenSample { x, y, phase, pressure: None, button, eraser };
    let mut out = vec![at(from.0, from.1, PenPhase::Down)];
    out.extend((1..=steps).map(|i| at(lerp(from.0, to.0, i), lerp(from.1, to.1, i), PenPhase::Move)));
    out.push(at(to.0, to.1, PenPhase::Up));
    out.push(at(to.0, to.1, PenPhase::Leave));
    out
}

/// A tap with the pen at a point: down, up, out of range.
pub fn tap(x: i32, y: i32, button: bool, eraser: bool) -> Vec<PenSample> {
    [PenPhase::Down, PenPhase::Up, PenPhase::Leave]
        .into_iter()
        .map(|phase| PenSample { x, y, phase, pressure: None, button, eraser })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_lines_round_trip() {
        for s in [
            PenSample::new(10, 20, PenPhase::Hover),
            PenSample { x: 0, y: 5, phase: PenPhase::Down, pressure: Some(200), button: true, eraser: true },
            PenSample::new(7, 8, PenPhase::Leave),
        ] {
            let line = s.console();
            let words: Vec<&str> = line.split_whitespace().skip(1).collect();
            assert_eq!(PenSample::parse(&words), Ok(s), "{line}");
        }
        assert_eq!(
            PenSample { pressure: Some(9), button: true, ..PenSample::new(1, 2, PenPhase::Move) }.console(),
            "pen 1 2 move pressure 9 button"
        );
    }

    #[test]
    fn the_phase_defaults_to_hover_and_bad_words_are_named() {
        assert_eq!(PenSample::parse(&["3", "4"]), Ok(PenSample::new(3, 4, PenPhase::Hover)));
        assert_eq!(PenSample::parse(&["3", "4", "button"]).map(|s| (s.phase, s.button)), Ok((PenPhase::Hover, true)));
        assert!(PenSample::parse(&["3"]).expect_err("no y").starts_with("missing y"));
        assert_eq!(PenSample::parse(&["-1", "4"]), Err("bad x '-1'".into()));
        assert_eq!(PenSample::parse(&["1", "4", "down", "pressure", "0"]), Err("bad pressure '0' (1-255)".into()));
        assert!(PenSample::parse(&["1", "4", "press"]).expect_err("unknown").starts_with("unknown word 'press'"));
    }

    #[test]
    fn strokes_touch_down_move_lift_and_leave() {
        let s = stroke((0, 0), (100, 50), 4, false, true);
        let phases: Vec<PenPhase> = s.iter().map(|p| p.phase).collect();
        use PenPhase::*;
        assert_eq!(phases, [Down, Move, Move, Move, Move, Up, Leave]);
        assert_eq!((s[2].x, s[2].y), (50, 25));
        assert_eq!((s[5].x, s[5].y), (100, 50));
        assert!(s.iter().all(|p| p.eraser && !p.button));
        assert_eq!(tap(5, 6, true, false).iter().map(|p| p.phase).collect::<Vec<_>>(), [Down, Up, Leave]);
    }
}
