//! The iPad pointer (a mouse): how to bring it to a point when the guest takes only relative
//! motion. The pointer is homed against the screen's edges first: pushed into the left edge, a
//! little right, into the bottom edge, up a little and into the left edge again, which leaves it
//! at a known point whatever its start. Never against the top edge (iPadOS opens Notification or
//! Control Center for a pointer pushed there) nor into a corner (the screen's rounded corners stop
//! it short, by a different amount per device). Pushing into the bottom shows an app's Dock until
//! the pointer leaves it, so homing rises [`LIFT`] points, above the Dock, and the caller waits
//! [`SETTLE`] for it to hide: a click on an app's bottom bar under the Dock then reaches the app
//! (checked on iOS 26.5, where rising 100 pt left the Dock up and the click opened a Dock app). It
//! then moves in steps no longer than [`MAX_STEP`].
//!
//! iOS 26.5 takes each report one to one. iOS 27.2 carries the pointer on past the end of a run of
//! reports by half its last report (persistently: the next move starts from there), so a run of
//! 10 pt steps lands 5 pt long, a single isolated report 1.5 times as far, and a push into an edge
//! keeps going after the next move starts. Neither the device's pointer acceleration
//! (`HIDMouseAcceleration`, 1.0 on both) nor backboardd's pointer
//! preferences change it (measured 2026-11-26). So every run ends with [`TAIL`] halving steps
//! (the overrun of the last, 1/64 of a 10 pt step, is 0.16 pt) and homing pauses after the push
//! into the bottom edge before it rises: exact on both.

use std::time::Duration;

use crate::geometry::{Point, Size};
use crate::reuse::BootKey;

/// The longest step, in points, sent in one report.
pub const MAX_STEP: f64 = 10.0;
/// How many halving steps end a run in place of its last step.
pub const TAIL: usize = 6;
/// Reports of no motion after the push into the bottom edge: a pause (~100 ms) in which iOS 27.2's
/// overrun of the push goes into the edge instead of into the rise that follows.
const PAUSE: usize = 6;
/// Time between reports.
pub const INTERVAL: Duration = Duration::from_millis(16);
/// One homing push, and how many: further than any iPad screen is wide or tall (1376 pt).
const PUSH: f64 = 120.0;
const PUSHES: usize = 14;
/// How far right of the left edge homing pushes into the bottom: clear of the rounded corner.
const CLEAR: f64 = 100.0;
/// How far above the bottom homing leaves the pointer: above the Dock (~120 pt tall on a 13-inch
/// iPad) that the push into the bottom shows, so the Dock hides again.
pub const LIFT: f64 = 300.0;
/// How long the Dock takes to hide once the pointer has left it.
pub const SETTLE: Duration = Duration::from_millis(400);

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

/// What simee knows of a device's pointer buttons between `pointer` calls.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Held {
    /// No button held.
    #[default]
    Nothing,
    /// `buttons` (a mask) held with the pointer at `at`, in points: the next moves start there.
    Down { at: Point, buttons: u32 },
    /// A call failed while a button was or may have been held: where the pointer is and what the
    /// guest holds are unknown.
    Lost,
}

/// A device's [`Held`] state, kept for the boot it was made in: a reboot lets every button go.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeldPointer {
    boot: Option<BootKey>,
    held: Held,
}

impl HeldPointer {
    /// What is held in `boot`: what the last call there left, else nothing (another boot, or one
    /// that can't be told from the next).
    pub fn get(&self, boot: Option<BootKey>) -> Held {
        if boot.is_some() && boot == self.boot { self.held } else { Held::Nothing }
    }

    /// Records what a call left held in `boot`.
    pub fn set(&mut self, boot: Option<BootKey>, held: Held) {
        *self = Self { boot, held };
    }

    /// Records a call that failed in `boot`: what it held is lost when anything was or may have
    /// been held (a press it may have sent, or a button held before it).
    pub fn failed(&mut self, boot: Option<BootKey>, before: Held, pressing: bool) {
        let held = if pressing || before != Held::Nothing { Held::Lost } else { Held::Nothing };
        self.set(boot, held);
    }
}

/// Whether a `pointer` call (`action`, or a drag when `drag`) can run with `held`, when the
/// process keeps state between calls (`lasting`: a server) or not (an in-process CLI run, which
/// ends with the call). Refuses a `down` no later call could release, an `up` with nothing held,
/// and a `move` or `up` once a failed call has lost what was held.
pub fn check(action: PointerAction, drag: bool, held: Held, lasting: bool) -> Result<(), &'static str> {
    if drag {
        return Ok(());
    }
    match (action, held) {
        (PointerAction::Down, _) if !lasting => Err("`down` keeps the button held for later calls, which only a server \
             (`simee serve`) remembers; this call runs in its own process. Use `click`, or a drag (`to_x`, `to_y`)"),
        (PointerAction::Up, Held::Nothing) => {
            Err("no pointer button is held: a `down` on this server holds one (a reboot or a failed call lets it go)")
        }
        (PointerAction::Move | PointerAction::Up, Held::Lost) => Err("a pointer call failed while a button was held, so \
             where the pointer is and what it holds are unknown: `click`, `down` or a drag lets go of every button and \
             starts over"),
        _ => Ok(()),
    }
}

/// Splits a move by `(dx, dy)` points into equal steps of at most [`MAX_STEP`] on either axis, the
/// last of them replaced by [`TAIL`] steps that halve it (the last two equal, so they add up to
/// it): the pointer slows to a stop, which iOS 27.2 needs to stop where it's sent (see the module).
pub fn steps(dx: f64, dy: f64) -> Vec<(f64, f64)> {
    let n = (dx.abs().max(dy.abs()) / MAX_STEP).ceil().max(1.0) as usize;
    let step = (dx / n as f64, dy / n as f64);
    let mut out = vec![step; n - 1];
    let mut f = 1.0;
    for i in 0..TAIL {
        if i + 1 < TAIL {
            f *= 0.5;
        }
        out.push((step.0 * f, step.1 * f));
    }
    out
}

/// The relative moves that bring the pointer to `to`, in points, on a screen of `screen` points:
/// from `from` when simee knows it is there (a held button keeps it, so a drag never homes), else
/// homed first (see the module; a caller that homes waits [`SETTLE`] after [`home`] and then
/// moves from [`homed`] instead).
pub fn path(from: Option<Point>, to: Point, screen: Size) -> Vec<(f64, f64)> {
    let (mut out, start) = match from {
        Some(p) => (Vec::new(), p),
        None => (home(), homed(screen)),
    };
    let (dx, dy) = (to.x - start.x, to.y - start.y);
    if dx != 0.0 || dy != 0.0 {
        out.extend(steps(dx, dy));
    }
    out
}

/// Homing: into the left edge, clear of it, into the bottom edge, up above the Dock and into the
/// left edge again, which leaves the pointer at [`homed`].
pub fn home() -> Vec<(f64, f64)> {
    let mut out = vec![(-PUSH, 0.0); PUSHES];
    out.extend([(CLEAR, 0.0); 2]);
    out.extend(vec![(0.0, PUSH); PUSHES]);
    out.extend(vec![(0.0, 0.0); PAUSE]);
    out.extend(steps(0.0, -LIFT));
    out.extend(vec![(-PUSH, 0.0); PUSHES]);
    out
}

/// Where [`home`] leaves the pointer on a screen of `screen` points: on the left edge, [`LIFT`]
/// above the bottom.
pub fn homed(screen: Size) -> Point {
    Point::new(0.0, screen.height - LIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum(moves: &[(f64, f64)]) -> (f64, f64) {
        moves.iter().fold((0.0, 0.0), |(x, y), (dx, dy)| (x + dx, y + dy))
    }

    /// Homing is 85 reports (74 before its pause and the halving tail): each costs 16 ms.
    #[test]
    fn homing_is_85_reports() {
        assert_eq!(home().len(), 85);
        assert_eq!(steps(0.0, 100.0).len(), 9 + TAIL, "a move costs its steps plus the tail");
    }

    #[test]
    fn steps_are_short_and_add_up() {
        let s = steps(100.0, -45.0);
        assert_eq!(s.len(), 9 + TAIL);
        assert!(s.iter().all(|(dx, dy)| dx.abs() <= MAX_STEP && dy.abs() <= MAX_STEP));
        let (x, y) = sum(&s);
        assert!((x - 100.0).abs() < 1e-9 && (y + 45.0).abs() < 1e-9);
        assert_eq!(steps(0.0, 0.0), vec![(0.0, 0.0); TAIL], "a move to where it is still reports");
    }

    /// iOS 27.2 carries the pointer on by half the last report of a run: the run ends slowing
    /// down, so that overrun is at most 0.16 pt.
    #[test]
    fn runs_end_slowing_to_a_stop() {
        let s = steps(500.0, -376.0);
        assert_eq!(&s[..49], vec![(10.0, -7.52); 49].as_slice(), "full steps first");
        let tail = &s[49..];
        assert!(tail.windows(2).all(|w| w[1].0 <= w[0].0), "{tail:?}");
        assert_eq!(tail[0], (5.0, -3.76));
        let (last_x, last_y) = *s.last().expect("steps");
        assert!(last_x * 0.5 <= 0.16 && (last_y * 0.5).abs() <= 0.16, "overrun {last_x}, {last_y}");
        assert_eq!(steps(3.0, 0.0)[0], (1.5, 0.0), "a short move is all tail");
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
        let end = play(Point::new(500.0, 500.0), &home(), screen);
        assert_eq!(end, homed(screen), "homing ends where `homed` says");
        assert!(homed(screen).y <= screen.height - 200.0, "left above the Dock");
        assert!(homed(Size::new(1133.0, 744.0)).y > 0.0, "an iPad mini in landscape still has room");
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

    #[test]
    fn held_buttons_are_checked_and_kept_per_boot() {
        use PointerAction::*;
        let down = Held::Down { at: Point::new(10.0, 20.0), buttons: 1 };
        assert!(check(Down, false, Held::Nothing, false).expect_err("in-process").contains("simee serve"));
        assert_eq!(check(Down, false, Held::Nothing, true), Ok(()));
        assert_eq!(check(Down, true, Held::Nothing, false), Ok(()), "a drag holds nothing past its call");
        assert_eq!(check(Click, false, Held::Nothing, false), Ok(()));
        assert_eq!(check(Move, false, Held::Nothing, false), Ok(()), "a hover needs nothing held");
        assert!(check(Up, false, Held::Nothing, true).expect_err("nothing held").contains("no pointer button is held"));
        assert_eq!(check(Up, false, down, true), Ok(()));
        assert_eq!(check(Move, false, down, true), Ok(()));
        for action in [Move, Up] {
            assert!(check(action, false, Held::Lost, true).expect_err("lost").contains("unknown"));
        }
        for (action, drag) in [(Click, false), (Down, false), (Move, true)] {
            assert_eq!(check(action, drag, Held::Lost, true), Ok(()), "{action:?} starts over");
        }

        let boot = BootKey::of(true, Some(1), None);
        let reboot = BootKey::of(true, Some(3), None);
        let mut state = HeldPointer::default();
        assert_eq!(state.get(boot), Held::Nothing);
        state.set(boot, down);
        assert_eq!(state.get(boot), down);
        assert_eq!(state.get(reboot), Held::Nothing, "a reboot lets go");
        assert_eq!(state.get(None), Held::Nothing, "a boot that can't be told apart keeps nothing");
        state.failed(boot, down, false);
        assert_eq!(state.get(boot), Held::Lost);
        state.failed(boot, Held::Nothing, true);
        assert_eq!(state.get(boot), Held::Lost, "a press may have landed");
        state.failed(boot, Held::Nothing, false);
        assert_eq!(state.get(boot), Held::Nothing, "a failed hover holds nothing");
    }
}
