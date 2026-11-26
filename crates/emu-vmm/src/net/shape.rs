//! Network shaping for the NAT, as the console's `network speed|delay` asks for it (stock's
//! netshaper and netdelay): a token-bucket rate for each direction (0 = unlimited) and a
//! latency drawn between a minimum and a maximum for each frame inbound. Pure logic over
//! caller-supplied instants, so it tests without sleeping.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sim_core::network::Conditions;

/// The smallest burst a bucket holds, in bits: one full-size frame, so a frame always fits.
const MIN_BURST_BITS: f64 = 12_144.0;
/// A bucket holds this much of its rate as burst (stock's shaper allows about as much).
const BURST: f64 = 0.02;

/// A token bucket of bits. A frame is always let through once the bucket is not in debt and
/// then charged in full, so the bucket may go negative by one frame: the average rate is exact
/// at any frame size.
pub struct Bucket {
    rate_bps: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl Bucket {
    pub fn new() -> Bucket {
        Bucket { rate_bps: 0.0, tokens: 0.0, last: None }
    }

    fn capacity(&self) -> f64 {
        (self.rate_bps * BURST).max(MIN_BURST_BITS)
    }

    /// Changes the rate (0 = unlimited). Going from unlimited to a rate starts with a full
    /// bucket; a lower rate keeps what it can hold and any debt.
    pub fn set_rate(&mut self, bps: f64) {
        let bps = if bps.is_finite() && bps > 0.0 { bps } else { 0.0 };
        let was_unlimited = self.rate_bps == 0.0;
        self.rate_bps = bps;
        if bps == 0.0 {
            self.tokens = 0.0;
        } else if was_unlimited {
            self.tokens = self.capacity();
        } else {
            self.tokens = self.tokens.min(self.capacity());
        }
    }

    fn refill(&mut self, now: Instant) {
        if let Some(last) = self.last.replace(now).filter(|_| self.rate_bps > 0.0) {
            let gained = now.saturating_duration_since(last).as_secs_f64() * self.rate_bps;
            self.tokens = (self.tokens + gained).min(self.capacity());
        }
    }

    /// How long until a frame may go: zero unless the bucket is in debt.
    pub fn wait(&mut self, now: Instant) -> Duration {
        self.refill(now);
        if self.rate_bps == 0.0 || self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate_bps) }
    }

    /// Charges a frame of `bytes`; returns how long until it may go (the debt it leaves).
    pub fn take(&mut self, now: Instant, bytes: usize) -> Duration {
        self.refill(now);
        if self.rate_bps == 0.0 {
            return Duration::ZERO;
        }
        self.tokens -= bytes as f64 * 8.0;
        if self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate_bps) }
    }
}

/// Frames for the guest, each held until its time: the download rate first, then a latency
/// between the minimum and maximum. Frames leave in the order they came in (a TCP flow
/// would read reordering as loss), so a short delay behind a long one waits for it.
pub struct Downlink {
    bucket: Bucket,
    min_ms: u32,
    max_ms: u32,
    rng: u64,
    last_due: Option<Instant>,
    queue: VecDeque<(Instant, Vec<u8>)>,
}

impl Downlink {
    pub fn new() -> Downlink {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64);
        Downlink { bucket: Bucket::new(), min_ms: 0, max_ms: 0, rng: seed | 1, last_due: None, queue: VecDeque::new() }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Applies new conditions. Frames already held are timed again under them, so lifting a
    /// throttle frees the backlog now rather than at the old rate.
    pub fn set(&mut self, now: Instant, c: &Conditions) {
        let held = std::mem::take(&mut self.queue);
        self.bucket.set_rate(c.download_bps);
        // Stock stores the latencies as ints and a negative one is no delay.
        self.min_ms = c.min_latency_ms.max(0) as u32;
        self.max_ms = (c.max_latency_ms.max(0) as u32).max(self.min_ms);
        self.last_due = None;
        for (_, f) in held {
            self.push(now, f);
        }
    }

    fn jitter(&mut self) -> Duration {
        if self.max_ms == self.min_ms {
            return Duration::from_millis(self.min_ms.into());
        }
        // xorshift64*: the spread only needs to look uniform.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        let span = u64::from(self.max_ms - self.min_ms) * 1000 + 1;
        Duration::from_micros(u64::from(self.min_ms) * 1000 + r % span)
    }

    /// Holds a frame for the guest.
    pub fn push(&mut self, now: Instant, frame: Vec<u8>) {
        let sent = now + self.bucket.take(now, frame.len());
        let mut due = sent + self.jitter();
        if let Some(last) = self.last_due {
            due = due.max(last);
        }
        self.last_due = Some(due);
        self.queue.push_back((due, frame));
    }

    /// The next frame whose time has come.
    pub fn pop_due(&mut self, now: Instant) -> Option<Vec<u8>> {
        match self.queue.front() {
            Some((due, _)) if *due <= now => self.queue.pop_front().map(|(_, f)| f),
            _ => None,
        }
    }

    /// When the oldest held frame is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|(d, _)| *d)
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.last_due = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 1500;

    fn cond(up: f64, down: f64, min: i32, max: i32) -> Conditions {
        Conditions { upload_bps: up, download_bps: down, min_latency_ms: min, max_latency_ms: max }
    }

    /// Pushes `n` frames at `t0` and returns when the last is due.
    fn last_due(d: &mut Downlink, t0: Instant, n: usize) -> Instant {
        for _ in 0..n {
            d.push(t0, vec![0; FRAME]);
        }
        d.queue.back().expect("pushed").0
    }

    #[test]
    fn download_rate_holds_per_direction_within_tolerance() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(t0, &cond(0.0, 473_600.0, 0, 0));
        // 200 frames = 2.4 Mbit at 473.6 kbit/s: 5.07 s less the opening burst.
        let secs = last_due(&mut d, t0, 200).duration_since(t0).as_secs_f64();
        let ideal = 200.0 * FRAME as f64 * 8.0 / 473_600.0;
        assert!((ideal - 0.05..=ideal).contains(&secs), "{secs} vs {ideal}");
    }

    #[test]
    fn upload_bucket_admits_at_the_rate() {
        let t0 = Instant::now();
        let mut up = Bucket::new();
        up.set_rate(1_000_000.0);
        let (mut now, mut sent) = (t0, 0usize);
        // Send whenever the bucket is not in debt, over 10 simulated seconds.
        while now.duration_since(t0) < Duration::from_secs(10) {
            let w = up.wait(now);
            if w.is_zero() {
                up.take(now, FRAME);
                sent += FRAME;
            } else {
                now += w;
            }
        }
        let bps = sent as f64 * 8.0 / 10.0;
        assert!((900_000.0..=1_030_000.0).contains(&bps), "{bps}");
    }

    #[test]
    fn zero_is_unlimited_and_undelayed() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(t0, &cond(0.0, 0.0, 0, 0));
        for _ in 0..5000 {
            d.push(t0, vec![0; FRAME]);
        }
        let mut n = 0;
        while d.pop_due(t0).is_some() {
            n += 1;
        }
        assert_eq!(n, 5000);
        let mut b = Bucket::new();
        assert_eq!(b.wait(t0), Duration::ZERO);
        assert_eq!(b.take(t0, usize::MAX / 16), Duration::ZERO);
    }

    #[test]
    fn latency_stays_between_min_and_max_and_keeps_order() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(t0, &cond(0.0, 0.0, 35, 200));
        let mut dues = Vec::new();
        for i in 0..500u32 {
            let at = t0 + Duration::from_millis(u64::from(i) * 2);
            d.push(at, vec![i as u8]);
            let due = d.queue.back().expect("pushed").0;
            assert!(due >= at + Duration::from_millis(35), "frame {i} early");
            dues.push(due);
        }
        // Order is kept, and no frame waits past the max after the one before it.
        assert!(dues.windows(2).all(|w| w[0] <= w[1]));
        let first = d.queue.front().expect("held").0;
        assert!(first <= t0 + Duration::from_millis(200));
        // The first frame, undisturbed by one ahead, sits inside [min, max].
        let mut lone = Downlink::new();
        lone.set(t0, &cond(0.0, 0.0, 35, 200));
        lone.push(t0, vec![0]);
        let wait = lone.next_due().expect("held").duration_since(t0);
        assert!((Duration::from_millis(35)..=Duration::from_millis(200)).contains(&wait));
    }

    #[test]
    fn fixed_latency_is_exact() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(t0, &cond(0.0, 0.0, 40, 40));
        d.push(t0, vec![1]);
        assert_eq!(d.next_due(), Some(t0 + Duration::from_millis(40)));
        assert!(d.pop_due(t0 + Duration::from_millis(39)).is_none());
        assert!(d.pop_due(t0 + Duration::from_millis(40)).is_some());
    }

    #[test]
    fn changing_conditions_mid_stream_retimes_held_frames() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(t0, &cond(0.0, 14_400.0, 0, 0));
        let slow = last_due(&mut d, t0, 20).duration_since(t0);
        assert!(slow > Duration::from_secs(10), "{slow:?}");
        // `network speed full`: the backlog is due now.
        let t1 = t0 + Duration::from_secs(1);
        d.set(t1, &cond(0.0, 0.0, 0, 0));
        assert_eq!(d.len(), 20);
        let mut n = 0;
        while d.pop_due(t1).is_some() {
            n += 1;
        }
        assert_eq!(n, 20);
        // And a throttle applied later takes hold for the frames after it.
        d.set(t1, &cond(0.0, 100_000.0, 0, 0));
        let secs = last_due(&mut d, t1, 20).duration_since(t1).as_secs_f64();
        assert!(secs > 2.0, "{secs}");
    }

    #[test]
    fn idle_time_refills_only_a_burst() {
        let t0 = Instant::now();
        let mut b = Bucket::new();
        b.set_rate(100_000.0);
        // A long quiet spell doesn't bank more than the burst: 100 frames still take their time.
        let t1 = t0 + Duration::from_secs(3600);
        let mut wait = Duration::ZERO;
        for _ in 0..100 {
            wait = b.take(t1, FRAME);
        }
        let ideal = 100.0 * FRAME as f64 * 8.0 / 100_000.0;
        assert!(wait.as_secs_f64() > ideal - 0.2, "{wait:?}");
    }

    #[test]
    fn negative_or_invalid_settings_mean_unshaped() {
        let t0 = Instant::now();
        let mut d = Downlink::new();
        d.set(t0, &cond(-5.0, f64::NAN, -10, -1));
        d.push(t0, vec![0; FRAME]);
        assert_eq!(d.next_due(), Some(t0));
    }
}
