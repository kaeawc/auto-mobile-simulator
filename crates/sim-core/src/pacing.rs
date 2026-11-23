//! Pacing for a loop that watches a framebuffer for changes (the iOS surface's seed). When the
//! device announces its frames (a per-frame callback wakes the loop), the loop sleeps until the
//! next announcement, with a long safety poll; otherwise it polls fast while the screen changes or
//! input was just sent, and backs off geometrically while it stays still. Either way a still screen
//! costs a few wakeups a second instead of hundreds.

use std::time::{Duration, Instant};

/// How a [`Pacer`] spaces its polls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pacing {
    /// The interval while the screen changes: a quarter of a 60 Hz frame.
    pub fast: Duration,
    /// How long polling stays fast after the last change, announcement or input: animations have
    /// gaps between frames, and a tap's response starts a few frames after the touch.
    pub hot: Duration,
    /// The longest polling interval on a still screen: how late a change nothing announced is seen.
    pub slow: Duration,
    /// While frames are announced, the interval of the safety poll that checks they still are.
    pub safety: Duration,
    /// Changes seen in a row without an announcement before announcements stop being trusted.
    pub misses: u32,
}

impl Pacing {
    /// The desktop's iOS capture: 4 ms while changing, up to 32 ms (about two 60 Hz frames) when
    /// still; a 100 ms safety poll while frames are announced. Input it sends wakes it at once.
    pub const FRAMEBUFFER: Self = Self {
        fast: Duration::from_millis(4),
        hot: Duration::from_millis(250),
        slow: Duration::from_millis(32),
        safety: Duration::from_millis(100),
        misses: 3,
    };
}

/// Decides how long a watch loop waits before its next look at the framebuffer.
#[derive(Clone, Debug)]
pub struct Pacer {
    pacing: Pacing,
    interval: Duration,
    active: Instant,
    trusted: bool,
    /// An announcement whose change hasn't been seen yet.
    pending: bool,
    misses: u32,
}

impl Pacer {
    /// A polling pacer. It starts hot: the first frames come at once.
    pub fn new(pacing: Pacing, now: Instant) -> Self {
        Self { pacing, interval: pacing.fast, active: now, trusted: false, pending: false, misses: 0 }
    }

    /// Trusts announcements from now on: the loop is woken on every frame and passes `announced`.
    pub fn announcing(mut self) -> Self {
        self.trusted = true;
        self
    }

    /// Whether announcements are trusted (false once too many changes came unannounced).
    pub fn trusts_announcements(&self) -> bool {
        self.trusted
    }

    /// Records one look: whether the contents changed since the last, and whether a frame was
    /// announced since the last. Returns how long to wait before the next look (an announcement or
    /// [`Pacer::kick`] may end the wait early).
    pub fn polled(&mut self, changed: bool, announced: bool, now: Instant) -> Duration {
        if announced {
            self.pending = true;
            self.active = now;
        }
        if changed {
            if self.trusted {
                self.misses = if self.pending { 0 } else { self.misses + 1 };
                self.trusted = self.misses < self.pacing.misses;
            }
            self.pending = false;
            self.active = now;
        }
        let hot = now.saturating_duration_since(self.active) < self.pacing.hot;
        self.interval = if hot { self.pacing.fast } else { (self.interval * 2).clamp(self.pacing.fast, self.pacing.slow) };
        // Trusted, the next frame wakes the loop: wait long, unless an announced frame hasn't shown
        // up in the surface yet (the announcement can come first).
        if self.trusted && !(self.pending && hot) { self.pacing.safety } else { self.interval }
    }

    /// Input was just sent: a change is coming, so poll fast again from now.
    pub fn kick(&mut self, now: Instant) {
        self.active = now;
        self.interval = self.pacing.fast;
    }

    /// The polling interval the last look chose (what an untrusting pacer waits).
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn pacer() -> (Pacer, Instant) {
        let t0 = Instant::now();
        (Pacer::new(Pacing::FRAMEBUFFER, t0), t0)
    }

    /// Ten seconds of a still screen, waiting whatever the pacer asks; returns the number of looks.
    fn still_for_ten_seconds(mut p: Pacer, t0: Instant) -> u32 {
        let (mut now, mut looks) = (t0, 0);
        while now < t0 + Duration::from_secs(10) {
            now += p.polled(false, false, now);
            looks += 1;
        }
        looks
    }

    #[test]
    fn polling_stays_fast_while_changing_and_within_the_hot_window() {
        let (mut p, t0) = pacer();
        assert_eq!(p.interval(), 4 * MS);
        assert_eq!(p.polled(true, false, t0 + 4 * MS), 4 * MS);
        assert_eq!(p.polled(false, false, t0 + 200 * MS), 4 * MS);
        assert_eq!(p.polled(false, false, t0 + 253 * MS), 4 * MS);
    }

    #[test]
    fn polling_backs_off_geometrically_to_the_slow_interval_on_a_still_screen() {
        let (mut p, t0) = pacer();
        let waits: Vec<_> = (0..6).map(|i| p.polled(false, false, t0 + 300 * MS + i * MS).as_millis()).collect();
        assert_eq!(waits, [8, 16, 32, 32, 32, 32]);
    }

    #[test]
    fn a_change_or_a_kick_makes_polling_fast_again() {
        let (mut p, t0) = pacer();
        for i in 0..5 {
            p.polled(false, false, t0 + 300 * MS + i * MS);
        }
        assert_eq!(p.interval(), 32 * MS);
        assert_eq!(p.polled(true, false, t0 + 400 * MS), 4 * MS);
        assert_eq!(p.polled(false, false, t0 + 600 * MS), 4 * MS, "still within the hot window of the change");
        for i in 0..5 {
            p.polled(false, false, t0 + 700 * MS + i * MS);
        }
        p.kick(t0 + 800 * MS);
        assert_eq!(p.interval(), 4 * MS);
        assert_eq!(p.polled(false, false, t0 + 900 * MS), 4 * MS, "still within the hot window of the kick");
    }

    #[test]
    fn a_still_screen_costs_a_few_dozen_polls_a_second_not_hundreds() {
        let (p, t0) = pacer();
        let looks = still_for_ten_seconds(p, t0);
        // The hot window (~63 polls), then one poll per 32 ms (~305); polling every 2 ms was 5000.
        assert!(looks < 400, "{looks} polls in 10 s");
        assert!(looks > 300, "{looks} polls in 10 s: a change must still be seen within 32 ms");
    }

    #[test]
    fn a_clock_that_goes_backwards_counts_as_hot() {
        let (mut p, t0) = pacer();
        p.kick(t0 + 100 * MS);
        assert_eq!(p.polled(false, false, t0), 4 * MS);
    }

    #[test]
    fn announced_frames_wait_for_the_next_announcement() {
        let (p, t0) = pacer();
        let mut p = p.announcing();
        assert!(p.trusts_announcements());
        assert_eq!(p.polled(false, false, t0), 100 * MS, "nothing pending: the safety poll");
        for i in 1..=60 {
            assert_eq!(p.polled(true, true, t0 + i * 16 * MS), 100 * MS, "frame {i}: seen, wait for the next");
        }
        assert_eq!(p.polled(false, false, t0 + 2 * Duration::from_secs(1)), 100 * MS);
        assert!(p.trusts_announcements());
        assert_eq!(still_for_ten_seconds(p, t0), 100, "one safety poll per 100 ms");
    }

    #[test]
    fn an_announcement_ahead_of_its_frame_polls_fast_until_the_frame_shows() {
        let (p, t0) = pacer();
        let mut p = p.announcing();
        assert_eq!(p.polled(false, true, t0), 4 * MS);
        assert_eq!(p.polled(false, false, t0 + 4 * MS), 4 * MS);
        assert_eq!(p.polled(true, false, t0 + 8 * MS), 100 * MS, "the announced frame arrived");
        assert!(p.trusts_announcements(), "a frame that was announced isn't a miss");
        // An announcement whose frame never shows stops the fast polls after the hot window.
        assert_eq!(p.polled(false, true, t0 + Duration::from_secs(1)), 4 * MS);
        assert_eq!(p.polled(false, false, t0 + 1300 * MS), 100 * MS);
    }

    #[test]
    fn unannounced_changes_in_a_row_fall_back_to_polling() {
        let (p, t0) = pacer();
        let mut p = p.announcing();
        // A stray miss (a safety poll racing the announcement) is forgiven by the next announced frame.
        p.polled(true, false, t0);
        p.polled(true, false, t0 + 100 * MS);
        p.polled(true, true, t0 + 200 * MS);
        assert!(p.trusts_announcements());
        p.polled(true, false, t0 + 300 * MS);
        p.polled(true, false, t0 + 400 * MS);
        assert!(p.trusts_announcements());
        assert_eq!(p.polled(true, false, t0 + 500 * MS), 4 * MS, "the third miss in a row: poll instead");
        assert!(!p.trusts_announcements());
        let waits: Vec<_> = (0..4).map(|i| p.polled(false, false, t0 + 800 * MS + i * MS).as_millis()).collect();
        assert_eq!(waits, [8, 16, 32, 32], "plain polling from here on");
    }

    #[test]
    fn announcements_still_speed_up_an_untrusting_pacer() {
        let (mut p, t0) = pacer();
        for i in 0..5 {
            p.polled(false, false, t0 + 300 * MS + i * MS);
        }
        assert_eq!(p.polled(false, true, t0 + 400 * MS), 4 * MS);
    }
}
