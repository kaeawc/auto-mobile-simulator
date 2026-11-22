//! Gesture planning: turns a high-level gesture into timed touch samples.

use std::time::Duration;

use crate::geometry::Point;

/// Phase of one touch sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

/// One touch sample: where, which phase, and how long to wait before sending it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub point: Point,
    pub phase: Phase,
    pub delay: Duration,
}

/// A tap: down, hold, up.
pub fn tap(at: Point, hold: Duration) -> Vec<Sample> {
    vec![Sample { point: at, phase: Phase::Down, delay: Duration::ZERO }, Sample { point: at, phase: Phase::Up, delay: hold }]
}

/// A straight swipe over `duration`, sampled every `step` (at least two moves so the system sees
/// motion). The finger lingers one step at the end so the gesture reads as a drag, not a flick,
/// only when `settle` is true.
pub fn swipe(from: Point, to: Point, duration: Duration, step: Duration, settle: bool) -> Vec<Sample> {
    let step = if step.is_zero() { Duration::from_millis(16) } else { step };
    let moves = ((duration.as_secs_f64() / step.as_secs_f64()).round() as usize).max(2);
    let delay = duration.div_f64(moves as f64);
    let mut out = Vec::with_capacity(moves + 3);
    out.push(Sample { point: from, phase: Phase::Down, delay: Duration::ZERO });
    for i in 1..=moves {
        out.push(Sample { point: from.lerp(to, i as f64 / moves as f64), phase: Phase::Move, delay });
    }
    if settle {
        out.push(Sample { point: to, phase: Phase::Move, delay: Duration::from_millis(150) });
    }
    out.push(Sample { point: to, phase: Phase::Up, delay });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_is_down_then_up() {
        let s = tap(Point::new(1.0, 2.0), Duration::from_millis(50));
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].phase, s[1].phase), (Phase::Down, Phase::Up));
        assert_eq!(s[1].delay, Duration::from_millis(50));
    }

    #[test]
    fn swipe_reaches_the_end_and_takes_the_duration() {
        let s = swipe(Point::new(0.0, 0.0), Point::new(100.0, 0.0), Duration::from_millis(160), Duration::from_millis(16), false);
        assert_eq!(s.first().map(|x| x.phase), Some(Phase::Down));
        assert_eq!(s.last().map(|x| (x.phase, x.point)), Some((Phase::Up, Point::new(100.0, 0.0))));
        assert_eq!(s.iter().filter(|x| x.phase == Phase::Move).count(), 10);
        let total: Duration = s.iter().filter(|x| x.phase == Phase::Move).map(|x| x.delay).sum();
        assert_eq!(total, Duration::from_millis(160));
    }

    #[test]
    fn swipe_always_moves_at_least_twice() {
        let s = swipe(Point::default(), Point::new(1.0, 1.0), Duration::ZERO, Duration::ZERO, true);
        assert!(s.iter().filter(|x| x.phase == Phase::Move).count() >= 3);
    }
}
