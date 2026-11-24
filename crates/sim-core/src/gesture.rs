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

/// Plays `delays` (each the wait before its step, as in [`Sample::delay`]) against absolute
/// deadlines measured from the start of the gesture: step `i` is due at the sum of the first
/// `i + 1` delays, however long the earlier sends and sleeps actually took. Sleeping each delay
/// in turn lets every send's cost and every oversleep (a timer's slack, a busy host) pile up on
/// the gesture; against deadlines a late step is sent at once and the next one still lands on
/// schedule. Steps are never skipped or merged, and the planned total is unchanged.
///
/// `elapsed` is the time since the gesture started and `sleep` waits that long; both are
/// injected, so a fake clock tests this on any host. `send` gets the step's index.
pub fn pace<E>(
    delays: &[Duration],
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
    mut send: impl FnMut(usize) -> Result<(), E>,
) -> Result<(), E> {
    let mut due = Duration::ZERO;
    for (i, delay) in delays.iter().enumerate() {
        due += *delay;
        if let Some(wait) = due.checked_sub(elapsed()).filter(|w| !w.is_zero()) {
            sleep(wait);
        }
        send(i)?;
    }
    Ok(())
}

/// [`pace`] over a gesture plan: `send` gets each [`Sample`] at its deadline.
pub fn pace_samples<E>(
    samples: &[Sample],
    elapsed: impl FnMut() -> Duration,
    sleep: impl FnMut(Duration),
    mut send: impl FnMut(&Sample) -> Result<(), E>,
) -> Result<(), E> {
    let delays: Vec<Duration> = samples.iter().map(|s| s.delay).collect();
    pace(&delays, elapsed, sleep, |i| send(&samples[i]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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

    /// A clock the test advances: `sleep` adds what it was asked (plus `slack`), `send` costs `cost`.
    fn run(samples: &[Sample], slack: Duration, cost: Duration) -> Vec<(Phase, Duration)> {
        let now = Cell::new(Duration::ZERO);
        let mut sent = Vec::new();
        pace_samples::<()>(
            samples,
            || now.get(),
            |d| now.set(now.get() + d + slack),
            |s| {
                sent.push((s.phase, now.get()));
                now.set(now.get() + cost);
                Ok(())
            },
        )
        .expect("sends");
        sent
    }

    #[test]
    fn pacing_keeps_the_planned_samples_and_total() {
        let s = swipe(Point::new(0.0, 0.0), Point::new(100.0, 0.0), Duration::from_millis(160), Duration::from_millis(16), true);
        let planned: Duration = s.iter().map(|x| x.delay).sum();
        assert_eq!(planned, Duration::from_millis(160 + 150 + 16));
        assert_eq!(s.len(), 1 + 10 + 1 + 1, "down, ten moves, the settle, up");
        let sent = run(&s, Duration::ZERO, Duration::ZERO);
        assert_eq!(sent.len(), s.len());
        assert_eq!(sent.last().map(|x| x.1), Some(planned), "the last sample lands on the planned total");
        let t = tap(Point::new(1.0, 1.0), Duration::from_millis(80));
        assert_eq!(run(&t, Duration::ZERO, Duration::ZERO), [(Phase::Down, Duration::ZERO), (Phase::Up, Duration::from_millis(80))]);
    }

    #[test]
    fn oversleeps_and_send_costs_do_not_accumulate() {
        let s = swipe(Point::new(0.0, 0.0), Point::new(100.0, 0.0), Duration::from_millis(160), Duration::from_millis(16), false);
        let planned: Duration = s.iter().map(|x| x.delay).sum();
        // Each sleep overshoots by 2 ms and each send takes 1 ms: relative sleeping drifts by
        // 3 ms a step; deadlines absorb it (a late sample goes out at once, then it catches up).
        let sent = run(&s, Duration::from_millis(2), Duration::from_millis(1));
        let last = sent.last().expect("sent").1;
        assert!(last <= planned + Duration::from_millis(3), "{last:?} vs {planned:?}");
        let drifting: Duration = s.iter().map(|x| x.delay + Duration::from_millis(3)).sum();
        assert!(last < drifting - Duration::from_millis(20), "relative sleeps would end at {drifting:?}");
    }

    #[test]
    fn late_steps_are_sent_without_sleeping() {
        let slept = Cell::new(0);
        let now = Cell::new(Duration::ZERO);
        let delays = [Duration::from_millis(10); 3];
        let mut sent = 0;
        pace::<()>(
            &delays,
            || now.get(),
            |d| {
                slept.set(slept.get() + 1);
                now.set(now.get() + d);
            },
            |_| {
                sent += 1;
                now.set(now.get() + Duration::from_millis(50)); // every send stalls past the next deadline
                Ok(())
            },
        )
        .expect("sends");
        assert_eq!((sent, slept.get()), (3, 1), "only the first deadline was in the future");
    }

    #[test]
    fn a_failed_send_stops_the_gesture() {
        let delays = [Duration::ZERO; 3];
        let mut n = 0;
        let r = pace(
            &delays,
            || Duration::ZERO,
            |_| {},
            |i| {
                n += 1;
                if i == 1 { Err("gone") } else { Ok(()) }
            },
        );
        assert_eq!((r, n), (Err("gone"), 2));
    }
}
