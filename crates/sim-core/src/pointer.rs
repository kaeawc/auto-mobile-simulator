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
//! preferences change it (measured 2026-11-26). So every run ends with halving steps down to a
//! last report of at most [`LAST`] (an overrun of 0.04 pt) and homing pauses after the push into
//! the bottom edge before it rises: exact on both.
//!
//! Homing costs about 1.4 s (64 reports and the Dock's settle), so a server homes only when it
//! must ([`HeldPointer::start`]): the first call in a boot, after a failed call or a rotation, after
//! a call whose reports went out late ([`send_paced`]) or that ended on an edge or in a corner
//! ([`lands_at`]), and every [`REHOME_EVERY`] calls. Other calls move on from where the last one
//! left the pointer. Touches (finger and Pencil), idle
//! time (the pointer hides) and relaunching the app don't move it, and its error doesn't add up:
//! 120 calls to random points after one homing all landed within 0.32 pt on iOS 27.2 (2026-11-27).
//! A last report of 0.31 pt (an overrun of 0.16 pt) did add up, to 0.78 pt within 80 calls.

use std::time::Duration;

use crate::geometry::{Point, Size};
use crate::reuse::BootKey;

/// The longest step, in points, sent in one report: a homing push's length, which iPadOS also
/// takes one to one (placement with 120 pt steps matched 10 pt ones on iOS 27.2, in a twelfth of
/// the reports).
pub const MAX_STEP: f64 = 120.0;
/// How many halving steps end a run in place of its last step, at least.
pub const TAIL: usize = 6;
/// The longest last report of a run, in points: iOS 27.2 carries the pointer on by half of it.
pub const LAST: f64 = 0.08;
/// How many calls move the pointer on from where the last one left it before one homes it again:
/// a bound on error nothing has shown yet (no drift over 120 calls; see the module).
pub const REHOME_EVERY: u32 = 100;
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
/// How close to an edge, in points, a point counts as on it: a target outside the screen is
/// clamped there, and the guest stops the pointer against the edge instead.
const EDGE: f64 = 1.0;
/// The square at each corner, in points, in which the screen's rounded corner may stop the
/// pointer short of its target (iPad corner radii are ~18-30 pt).
const CORNER: f64 = 40.0;
/// How late a report may go out past its deadline before the run counts as stalled: a gap of
/// two report intervals, which the guest may take for the end of a run and carry the pointer on
/// by half the report before it (up to 60 pt at [`MAX_STEP`]).
pub const LATE: Duration = INTERVAL;

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

/// A device's [`Held`] state and where its pointer is, kept for the boot they were made in: a
/// reboot lets every button go and puts the pointer back where simee can't see it.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeldPointer {
    boot: Option<BootKey>,
    held: Held,
    place: Option<Place>,
}

/// Where the last call left the pointer: the point, how many calls have moved it since it was
/// homed, and the count of rotations the caller had seen ([`HeldPointer::start`]'s `turns`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Place {
    at: Point,
    moves: u32,
    turns: u64,
}

impl HeldPointer {
    /// What is held in `boot`: what the last call there left, else nothing (another boot, or one
    /// that can't be told from the next).
    pub fn get(&self, boot: Option<BootKey>) -> Held {
        if boot.is_some() && boot == self.boot { self.held } else { Held::Nothing }
    }

    /// Where a call in `boot` moves the pointer from, or `None` to home it first: where a held
    /// button keeps it (a drag never homes), else where the last call in this boot left it,
    /// unless a call failed since, the device turned (`turns`, the caller's count of rotations and
    /// folds, changed: the screen's axes did), or [`REHOME_EVERY`] calls have moved it since it
    /// was last homed.
    pub fn start(&self, boot: Option<BootKey>, turns: u64) -> Option<Point> {
        if let Held::Down { at, .. } = self.get(boot) {
            return Some(at);
        }
        let place = self.place.filter(|_| boot.is_some() && boot == self.boot)?;
        (place.turns == turns && place.moves < REHOME_EVERY).then_some(place.at)
    }

    /// Records what a call left held in `boot` and where it left the pointer (`at`; `None` when
    /// that isn't known for sure, so the next call homes: [`lands_at`], a stalled run), having
    /// homed it first (`homed`) or moved on from [`HeldPointer::start`], with `turns` rotations seen.
    pub fn set(&mut self, boot: Option<BootKey>, held: Held, at: Option<Point>, homed: bool, turns: u64) {
        let moves = match self.place.filter(|_| !homed && boot == self.boot) {
            Some(place) => place.moves + 1,
            None => 0,
        };
        *self = Self { boot, held, place: at.map(|at| Place { at, moves, turns }) };
    }

    /// Records a call that failed in `boot`: what it held is lost when anything was or may have
    /// been held (a press it may have sent, or a button held before it), and where the pointer
    /// is is unknown.
    pub fn failed(&mut self, boot: Option<BootKey>, before: Held, pressing: bool) {
        let held = if pressing || before != Held::Nothing { Held::Lost } else { Held::Nothing };
        *self = Self { boot, held, place: None };
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
/// last of them replaced by halving steps (the last two equal, so they add up to it), at least
/// [`TAIL`] and as many as bring the last down to [`LAST`]: the pointer slows to a stop, which iOS
/// 27.2 needs to stop where it's sent (see the module).
pub fn steps(dx: f64, dy: f64) -> Vec<(f64, f64)> {
    let n = (dx.abs().max(dy.abs()) / MAX_STEP).ceil().max(1.0) as usize;
    let step = (dx / n as f64, dy / n as f64);
    let size = step.0.abs().max(step.1.abs());
    let halvings = if size > LAST { (size / LAST).log2().ceil() as usize } else { 0 };
    let tail = halvings.max(TAIL - 1) + 1;
    let mut out = vec![step; n - 1];
    let mut f = 1.0;
    for i in 0..tail {
        if i + 1 < tail {
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

/// A call's reports, each with the wait before it: moves `from` a known point, else homing,
/// [`SETTLE`] for the Dock, then the moves from [`homed`] ([`path`]). The caller sends them
/// against deadlines (`sim_core::gesture::pace`), so a busy host doesn't stretch the call.
pub fn plan(from: Option<Point>, to: Point, screen: Size) -> Vec<(Duration, (f64, f64))> {
    let homing = if from.is_some() { Vec::new() } else { home() };
    let moves = path(Some(from.unwrap_or_else(|| homed(screen))), to, screen);
    let homed_at = homing.len();
    homing
        .into_iter()
        .chain(moves)
        .enumerate()
        .map(|(i, step)| {
            let wait = match i {
                0 => Duration::ZERO,
                i if i == homed_at => INTERVAL + SETTLE,
                _ => INTERVAL,
            };
            (wait, step)
        })
        .collect()
}

/// Whether a move to `to` on a screen of `screen` points leaves the pointer exactly there: not on
/// an edge (where a clamped target lands and the guest stops it) nor in a corner's [`CORNER`]
/// square (the rounded corner stops it short), so a later call can move on from `to`.
pub fn lands_at(to: Point, screen: Size) -> bool {
    let near = |v: f64, max: f64, margin: f64| v <= margin || v >= max - margin;
    let on_edge = near(to.x, screen.width, EDGE) || near(to.y, screen.height, EDGE);
    let in_corner = near(to.x, screen.width, CORNER) && near(to.y, screen.height, CORNER);
    !on_edge && !in_corner
}

/// Sends `plan` ([`plan`]) against deadlines from its first report (`crate::gesture::pace`), each
/// report through `send`; whether any went out more than [`LATE`] past its deadline (a stalled
/// host), after which where the pointer stopped isn't known. `elapsed` is the time since the
/// first report and `sleep` waits; both are injected, so a fake clock tests this anywhere.
pub fn send_paced<E>(
    plan: &[(Duration, (f64, f64))],
    elapsed: impl Fn() -> Duration,
    sleep: impl FnMut(Duration),
    mut send: impl FnMut((f64, f64)) -> Result<(), E>,
) -> Result<bool, E> {
    let delays: Vec<Duration> = plan.iter().map(|(wait, _)| *wait).collect();
    let (mut due, mut late) = (Duration::ZERO, false);
    crate::gesture::pace(&delays, &elapsed, sleep, |i| {
        due += delays[i];
        late |= elapsed() > due + LATE;
        send(plan[i].1)
    })?;
    Ok(late)
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

    /// Homing is 64 reports (30 before its pause and the rise): each costs 16 ms.
    #[test]
    fn homing_is_64_reports() {
        assert_eq!(home().len(), 64);
        assert_eq!(steps(0.0, 100.0).len(), 12, "a move costs its steps plus the tail down to `LAST`");
    }

    /// A later call moves on from where the last left the pointer: even across the whole screen
    /// that is 23 reports (~370 ms), where 10 pt steps took 143.
    #[test]
    fn a_move_across_the_screen_is_a_few_reports() {
        let across = steps(1032.0, -1376.0);
        assert_eq!(across.len(), 23);
        let (x, y) = sum(&across);
        assert!((x - 1032.0).abs() < 1e-9 && (y + 1376.0).abs() < 1e-9);
    }

    #[test]
    fn steps_are_short_and_add_up() {
        let s = steps(300.0, -45.0);
        assert_eq!(s.len(), 2 + 12);
        assert!(s.iter().all(|(dx, dy)| dx.abs() <= MAX_STEP && dy.abs() <= MAX_STEP));
        let (x, y) = sum(&s);
        assert!((x - 300.0).abs() < 1e-9 && (y + 45.0).abs() < 1e-9);
        assert_eq!(steps(0.0, 0.0), vec![(0.0, 0.0); TAIL], "a move to where it is still reports");
    }

    /// iOS 27.2 carries the pointer on by half the last report of a run: the run ends slowing
    /// down, so that overrun is at most 0.04 pt (0.16 pt added up over calls that no longer home).
    #[test]
    fn runs_end_slowing_to_a_stop() {
        let s = steps(500.0, -376.0);
        assert_eq!(&s[..4], vec![(100.0, -75.2); 4].as_slice(), "full steps first");
        let tail = &s[4..];
        assert!(tail.windows(2).all(|w| w[1].0 <= w[0].0), "{tail:?}");
        assert_eq!(tail[0], (50.0, -37.6));
        let (last_x, last_y) = *s.last().expect("steps");
        assert!(last_x * 0.5 <= 0.04 && (last_y * 0.5).abs() <= 0.04, "overrun {last_x}, {last_y}");
        assert_eq!(steps(3.0, 0.0)[0], (1.5, 0.0), "a short move is all tail");
        assert!(steps(3.0, 0.0).last().expect("steps").0 <= LAST);
        assert_eq!(steps(0.1, 0.0).len(), TAIL, "never fewer than `TAIL`");
    }

    /// Homing's reports are paced 16 ms apart, then the Dock's settle before the move that
    /// follows; a known start sends only the move.
    #[test]
    fn plans_pace_homing_and_its_settle() {
        let screen = Size::new(1032.0, 1376.0);
        let to = Point::new(300.0, 400.0);
        let homing = plan(None, to, screen);
        assert_eq!(homing.len(), home().len() + steps(300.0, 400.0 - homed(screen).y).len());
        assert_eq!(homing[0].0, Duration::ZERO, "the first report goes at once");
        assert_eq!(homing[home().len()].0, INTERVAL + SETTLE, "the move waits for the Dock to hide");
        assert!(homing.iter().enumerate().all(|(i, (wait, _))| i == 0 || i == home().len() || *wait == INTERVAL));
        let moves: Vec<_> = homing[home().len()..].iter().map(|(_, m)| *m).collect();
        assert_eq!(moves, steps(300.0, 400.0 - homed(screen).y));

        let on = plan(Some(Point::new(700.0, 900.0)), to, screen);
        assert_eq!(on.iter().map(|(_, m)| *m).collect::<Vec<_>>(), steps(-400.0, -500.0), "no homing");
        let total: Duration = on.iter().map(|(wait, _)| *wait).sum();
        assert!(total <= Duration::from_millis(250), "{total:?}");
        assert!(plan(Some(to), to, screen).is_empty(), "already there");
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
        state.set(boot, down, Some(Point::new(10.0, 20.0)), true, 0);
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

    /// Where the last call left the pointer is where the next one starts, in the same boot and
    /// orientation, until a failure or [`REHOME_EVERY`] calls on from one homing.
    #[test]
    fn the_pointer_moves_on_from_where_the_last_call_left_it() {
        let boot = BootKey::of(true, Some(1), None);
        let reboot = BootKey::of(true, Some(3), None);
        let (a, b) = (Point::new(100.0, 200.0), Point::new(300.0, 50.0));
        let mut state = HeldPointer::default();
        assert_eq!(state.start(boot, 0), None, "nothing known: home");
        state.set(boot, Held::Nothing, Some(a), true, 0);
        assert_eq!(state.start(boot, 0), Some(a));
        state.set(boot, Held::Nothing, Some(b), false, 0);
        assert_eq!(state.start(boot, 0), Some(b), "a later call moves on from the last one");
        assert_eq!(state.start(reboot, 0), None, "a reboot puts the pointer where simee can't see it");
        assert_eq!(state.start(None, 0), None, "a boot that can't be told apart keeps nothing");
        assert_eq!(state.start(boot, 1), None, "a rotation turned the screen's axes");

        state.failed(boot, Held::Nothing, false);
        assert_eq!(state.start(boot, 0), None, "a failed call may have left it anywhere");
        state.set(boot, Held::Nothing, Some(a), true, 0);
        state.set(boot, Held::Nothing, None, false, 0);
        assert_eq!(state.start(boot, 0), None, "a call that can't say where it left the pointer");

        state.set(boot, Held::Nothing, Some(a), true, 0);
        for _ in 0..REHOME_EVERY {
            assert_eq!(state.start(boot, 0), Some(a));
            state.set(boot, Held::Nothing, Some(a), false, 0);
        }
        assert_eq!(state.start(boot, 0), None, "homed again every REHOME_EVERY calls");
        state.set(boot, Held::Nothing, Some(b), true, 0);
        assert_eq!(state.start(boot, 0), Some(b), "and the count starts over");

        let down = Held::Down { at: b, buttons: 1 };
        state.set(boot, down, None, false, REHOME_EVERY.into());
        assert_eq!(state.start(boot, 5), Some(b), "a held button keeps its place: a drag never homes");
    }

    /// A target on an edge (where a target off the screen is clamped) or in a rounded corner may
    /// stop the pointer short of it: the next call homes instead of moving on from there.
    #[test]
    fn edges_and_corners_are_not_where_the_pointer_is_known_to_be() {
        let screen = Size::new(1032.0, 1376.0);
        for to in [
            Point::new(0.0, 0.0),
            Point::new(1032.0, 1376.0),
            Point::new(20.0, 1360.0),
            Point::new(1010.0, 30.0),
            Point::new(0.0, 700.0),
            Point::new(500.0, 1376.0),
            Point::new(1031.5, 600.0),
        ] {
            assert!(!lands_at(to, screen), "{to:?}");
        }
        for to in [Point::new(500.0, 700.0), Point::new(20.0, 700.0), Point::new(500.0, 10.0), Point::new(41.0, 41.0)] {
            assert!(lands_at(to, screen), "{to:?}");
        }
    }

    /// A report that goes out more than `LATE` past its deadline marks the run stalled; reports
    /// on time, or a little late, don't.
    #[test]
    fn a_stalled_run_is_reported() {
        use std::cell::Cell;
        let plan = plan(Some(Point::new(100.0, 100.0)), Point::new(600.0, 900.0), Size::new(1032.0, 1376.0));
        let run = |stall_at: usize, stall: Duration| {
            let now = Cell::new(Duration::ZERO);
            let mut sent = Vec::new();
            let late = send_paced(
                &plan,
                || now.get(),
                |d| now.set(now.get() + d),
                |step| {
                    if sent.len() == stall_at {
                        now.set(now.get() + stall);
                    }
                    sent.push(step);
                    Ok::<(), ()>(())
                },
            );
            assert_eq!(sent, plan.iter().map(|(_, s)| *s).collect::<Vec<_>>(), "every report is sent, in order");
            late.expect("sent")
        };
        assert!(!run(usize::MAX, Duration::ZERO), "on time");
        assert!(!run(3, LATE / 2), "a little late");
        assert!(run(3, LATE * 3), "a stall");
        assert!(run(plan.len() - 2, LATE * 3), "a stall before the last report");
    }
}
