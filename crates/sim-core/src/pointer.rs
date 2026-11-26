//! The iPad pointer (a mouse): how to bring it to a point when the guest takes only relative
//! motion. The pointer is homed against the screen's edges first: pushed into the left edge, a
//! little right, into the bottom edge, up a little and into the left edge again, which leaves it
//! at a known point whatever its start. Never against the top edge (iPadOS opens Notification or
//! Control Center for a pointer pushed there) nor into a corner (the screen's rounded corners stop
//! it short, by a different amount per device). Pushing into the bottom shows an app's Dock until
//! the pointer leaves it. It then moves in steps no longer than [`MAX_STEP`], which iOS 26.5 takes
//! one to one (exactly where asked) and iOS 27.2 with a little acceleration (within ~10 pt).

use std::time::Duration;

use crate::geometry::{Point, Size};

/// The longest step, in points, sent in one report.
pub const MAX_STEP: f64 = 10.0;
/// Time between reports.
pub const INTERVAL: Duration = Duration::from_millis(16);
/// One homing push, and how many: further than any iPad screen is wide or tall (1376 pt).
const PUSH: f64 = 120.0;
const PUSHES: usize = 14;
/// How far from the bottom-left corner homing leaves the pointer: clear of the rounded corner.
const CLEAR: f64 = 100.0;

/// What a `pointer` call does once the pointer is at its point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerAction {
    /// Hover there.
    Move,
    /// Press and release a button there.
    Click,
    /// Press a button there and keep it held: the moves that follow drag.
    Down,
    /// Release the held button there.
    Up,
}

impl PointerAction {
    pub const NAMES: &'static str = "move, click, down, up";

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "move" | "hover" => Self::Move,
            "click" => Self::Click,
            "down" => Self::Down,
            "up" => Self::Up,
            _ => return None,
        })
    }
}

/// Splits a move by `(dx, dy)` points into equal steps of at most [`MAX_STEP`] on either axis.
pub fn steps(dx: f64, dy: f64) -> Vec<(f64, f64)> {
    let n = (dx.abs().max(dy.abs()) / MAX_STEP).ceil().max(1.0) as usize;
    vec![(dx / n as f64, dy / n as f64); n]
}

/// The relative moves that bring the pointer to `to`, in points, on a screen of `screen` points:
/// from `from` when simee knows it is there (a held button keeps it, so a drag never homes), else
/// homed first (see the module).
pub fn path(from: Option<Point>, to: Point, screen: Size) -> Vec<(f64, f64)> {
    let (mut out, start) = match from {
        Some(p) => (Vec::new(), p),
        None => (home(), Point::new(0.0, screen.height - CLEAR)),
    };
    let (dx, dy) = (to.x - start.x, to.y - start.y);
    if dx != 0.0 || dy != 0.0 {
        out.extend(steps(dx, dy));
    }
    out
}

/// Homing: into the left edge, clear of it, into the bottom edge, up from it and into the left
/// edge again, which leaves the pointer [`CLEAR`] above the bottom-left corner.
fn home() -> Vec<(f64, f64)> {
    let mut out = vec![(-PUSH, 0.0); PUSHES];
    out.extend([(CLEAR, 0.0); 2]);
    out.extend(vec![(0.0, PUSH); PUSHES]);
    out.extend(steps(0.0, -CLEAR));
    out.extend(vec![(-PUSH, 0.0); PUSHES]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(moves: &[(f64, f64)]) -> (f64, f64) {
        moves.iter().fold((0.0, 0.0), |(x, y), (dx, dy)| (x + dx, y + dy))
    }

    #[test]
    fn steps_are_short_and_add_up() {
        let s = steps(100.0, -45.0);
        assert_eq!(s.len(), 10);
        assert!(s.iter().all(|(dx, dy)| dx.abs() <= MAX_STEP && dy.abs() <= MAX_STEP));
        let (x, y) = sum(&s);
        assert!((x - 100.0).abs() < 1e-9 && (y + 45.0).abs() < 1e-9);
        assert_eq!(steps(0.0, 0.0), [(0.0, 0.0)], "a move to where it is still reports once");
    }

    /// Where the moves leave a pointer that starts at `at`, clamped to the screen as iPadOS does.
    fn play(at: Point, moves: &[(f64, f64)], screen: Size) -> Point {
        moves.iter().fold(at, |p, (dx, dy)| Point::new((p.x + dx).clamp(0.0, screen.width), (p.y + dy).clamp(0.0, screen.height)))
    }

    #[test]
    fn homing_reaches_the_point_from_anywhere_without_touching_the_top() {
        let screen = Size::new(1032.0, 1376.0);
        let to = Point::new(300.0, 400.0);
        for start in [Point::new(0.0, 0.0), Point::new(1032.0, 1376.0), Point::new(500.0, 20.0), Point::new(1032.0, 0.0)] {
            let moves = path(None, to, screen);
            let end = play(start, &moves, screen);
            assert!((end.x - to.x).abs() < 1e-6 && (end.y - to.y).abs() < 1e-6, "from {start:?}: {end:?}");
            let mut p = start;
            for (dx, dy) in &moves {
                p = play(p, &[(*dx, *dy)], screen);
                assert!(*dy >= 0.0 || p.y > 0.0, "pushed into the top edge from {start:?}");
            }
        }
        assert!(PUSH * PUSHES as f64 > 1376.0, "further than the largest iPad screen");
        let homing = home();
        assert!(homing.iter().all(|(dx, dy)| dx.abs() <= PUSH && dy.abs() <= PUSH));
    }

    #[test]
    fn a_known_start_moves_from_there() {
        let screen = Size::new(1032.0, 1376.0);
        let p = path(Some(Point::new(300.0, 500.0)), Point::new(320.0, 460.0), screen);
        assert_eq!(sum(&p), (20.0, -40.0));
        assert!(p.iter().all(|(dx, dy)| dx.abs() <= MAX_STEP && dy.abs() <= MAX_STEP));
        assert!(path(Some(Point::new(1.0, 1.0)), Point::new(1.0, 1.0), screen).is_empty());
    }

    #[test]
    fn actions_parse() {
        assert_eq!(PointerAction::parse("hover"), Some(PointerAction::Move));
        assert_eq!(PointerAction::parse("click"), Some(PointerAction::Click));
        assert_eq!(PointerAction::parse("drag"), None);
    }
}
