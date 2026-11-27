//! Fan-out: one capture per source, shared by every subscriber linked to it. The first
//! subscription opens the source and starts its capture thread; the capture runs at the largest
//! size and the highest rate its subscribers ask for, hands each subscriber frames at its own size
//! and rate (a still or a same-size frame is shared, never copied), and stops, dropping the source
//! and releasing any camera, as soon as the last subscription is dropped.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::id::SourceSpec;
use crate::source::{Opener, Source};

/// Frames a slow subscriber may fall behind by before newer frames are dropped for it (the
/// capture never waits for a subscriber).
const QUEUE: usize = 2;

/// The captures running now, keyed by canonical source id.
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    opener: Box<dyn Opener>,
    captures: Mutex<HashMap<String, Arc<Capture>>>,
}

struct Capture {
    key: String,
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    subscribers: Vec<Subscriber>,
    next_id: u64,
    /// Set once the capture thread has decided to exit; a new subscription then starts a new one.
    stopped: bool,
    /// Why the capture ended, when it failed: what every subscriber's next receive returns.
    failure: Option<Error>,
}

struct Subscriber {
    id: u64,
    spec: FrameSpec,
    tx: SyncSender<CameraFrame>,
    due: Instant,
}

/// A running capture, for listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Active {
    pub source: String,
    pub subscribers: usize,
}

impl Hub {
    pub fn new(opener: impl Opener + 'static) -> Hub {
        Hub { inner: Arc::new(Inner { opener: Box::new(opener), captures: Mutex::new(HashMap::new()) }) }
    }

    /// Subscribes to `source` at `spec`. Opens the source (and fails here, with the reason, when
    /// it can't: a missing camera, the permission) unless a capture of it is already running.
    pub fn subscribe(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Subscription> {
        let canonical = self.inner.opener.canonical(source)?;
        let key = canonical.to_string();
        let mut captures = lock(&self.inner.captures);
        if let Some(capture) = captures.get(&key).cloned() {
            let mut state = lock(&capture.state);
            if !state.stopped {
                let (id, rx) = add(&mut state, spec);
                drop(state);
                capture.wake.notify_all();
                return Ok(Subscription { id, spec, capture, rx });
            }
        }
        // Opened under the hub's lock, so a second subscriber to a source that is still opening
        // waits and joins its capture instead of opening the camera twice.
        let source = self.inner.opener.open(&canonical, spec)?;
        let capture = Arc::new(Capture {
            key: key.clone(),
            state: Mutex::new(State { subscribers: Vec::new(), next_id: 0, stopped: false, failure: None }),
            wake: Condvar::new(),
        });
        let (id, rx) = add(&mut lock(&capture.state), spec);
        captures.insert(key.clone(), capture.clone());
        drop(captures);
        let (weak, run) = (Arc::downgrade(&self.inner), capture.clone());
        // Named by kind only: the id can be long, or hold a NUL (a `qr:` text), which a thread
        // name can't.
        let name = format!("camera {}", canonical.kind());
        let spawned = thread::Builder::new().name(name).spawn(move || run_capture(weak, run, source));
        if let Err(e) = spawned {
            lock(&self.inner.captures).remove(&key);
            return Err(Error::Capture(format!("can't start the capture thread: {e}")));
        }
        Ok(Subscription { id, spec, capture, rx })
    }

    /// The captures running now, by source id.
    pub fn active(&self) -> Vec<Active> {
        let captures = lock(&self.inner.captures);
        let mut out: Vec<Active> = captures
            .values()
            .map(|c| Active { source: c.key.clone(), subscribers: lock(&c.state).subscribers.len() })
            .filter(|a| a.subscribers > 0)
            .collect();
        out.sort_by(|a, b| a.source.cmp(&b.source));
        out
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn add(state: &mut State, spec: FrameSpec) -> (u64, Receiver<CameraFrame>) {
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let id = state.next_id;
    state.next_id += 1;
    state.subscribers.push(Subscriber { id, spec, tx, due: Instant::now() });
    (id, rx)
}

/// One consumer's link to a capture. Dropping it unsubscribes; the capture stops with the last.
pub struct Subscription {
    id: u64,
    spec: FrameSpec,
    capture: Arc<Capture>,
    rx: Receiver<CameraFrame>,
}

impl Subscription {
    /// The canonical id of the source (`webcam:<id>` for a webcam however it was named).
    pub fn source(&self) -> &str {
        &self.capture.key
    }

    pub fn spec(&self) -> FrameSpec {
        self.spec
    }

    /// The next frame, waiting up to `timeout`; `Ok(None)` on a timeout. Once the capture has
    /// failed (the camera was unplugged): the frames already queued, then the failure.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<CameraFrame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// A frame if one is waiting.
    pub fn try_recv(&self) -> Result<Option<CameraFrame>> {
        match self.rx.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(self.ended()),
        }
    }

    fn ended(&self) -> Error {
        let failure = lock(&self.capture.state).failure.clone();
        failure.unwrap_or_else(|| Error::Capture(format!("the capture of {} stopped", self.capture.key)))
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.capture.state).subscribers.retain(|s| s.id != self.id);
        self.capture.wake.notify_all();
    }
}

/// The capture spec for a set of subscribers: the largest frame asked for, at the highest rate.
fn capture_spec(subscribers: &[Subscriber]) -> Option<FrameSpec> {
    let largest = subscribers.iter().map(|s| s.spec).max_by_key(FrameSpec::area)?;
    let fps = subscribers.iter().map(|s| s.spec.fps).max().unwrap_or(largest.fps);
    Some(FrameSpec { fps, ..largest })
}

fn run_capture(hub: Weak<Inner>, capture: Arc<Capture>, mut source: Box<dyn Source>) {
    let started = Instant::now();
    let wall = SystemTime::now();
    let mut seq = 0u64;
    let mut next = started;
    loop {
        // Sleep until the next tick, waking early when a subscriber comes or goes.
        let spec = {
            let mut state = lock(&capture.state);
            loop {
                if state.subscribers.is_empty() {
                    break None;
                }
                let now = Instant::now();
                if now >= next {
                    break capture_spec(&state.subscribers);
                }
                state = capture.wake.wait_timeout(state, next - now).map(|(s, _)| s).unwrap_or_else(|p| p.into_inner().0);
            }
        };
        let Some(spec) = spec else {
            if stop_if_idle(&hub, &capture) {
                return;
            }
            continue;
        };
        let now = Instant::now();
        let pts = now - started;
        let tick = Tick { seq, pts, wall: wall + pts };
        let frame = source.frame(spec, tick);
        seq += 1;
        next = (next + spec.interval()).max(now);
        let mut state = lock(&capture.state);
        match frame {
            Ok(frame) => deliver(&mut state.subscribers, &frame, now),
            Err(e) => {
                // Dropping the senders ends every subscriber's stream; each then reads `failure`.
                state.failure = Some(e);
                state.stopped = true;
                state.subscribers.clear();
                drop(state);
                remove(&hub, &capture);
                return;
            }
        }
    }
}

/// Sends a frame to every subscriber that is due, at its size; drops subscribers whose
/// subscription is gone.
fn deliver(subscribers: &mut Vec<Subscriber>, frame: &CameraFrame, now: Instant) {
    let mut sized: Vec<CameraFrame> = vec![frame.clone()];
    subscribers.retain_mut(|s| {
        if now < s.due {
            return true;
        }
        s.due = (s.due + s.spec.interval()).max(now);
        let out = match sized.iter().find(|f| (f.width, f.height) == (s.spec.width, s.spec.height)) {
            Some(f) => f.clone(),
            None => {
                let f = frame.fit(s.spec.width, s.spec.height);
                sized.push(f.clone());
                f
            }
        };
        !matches!(s.tx.try_send(out), Err(TrySendError::Disconnected(_)))
    });
}

/// Ends the capture if it still has no subscribers (checked under the hub's lock, so a racing
/// subscribe either joins before this or starts a fresh capture after).
fn stop_if_idle(hub: &Weak<Inner>, capture: &Arc<Capture>) -> bool {
    let Some(inner) = hub.upgrade() else { return true };
    let mut captures = lock(&inner.captures);
    let mut state = lock(&capture.state);
    if !state.subscribers.is_empty() {
        return false;
    }
    state.stopped = true;
    if captures.get(&capture.key).is_some_and(|c| Arc::ptr_eq(c, capture)) {
        captures.remove(&capture.key);
    }
    true
}

fn remove(hub: &Weak<Inner>, capture: &Arc<Capture>) {
    if let Some(inner) = hub.upgrade() {
        let mut captures = lock(&inner.captures);
        if captures.get(&capture.key).is_some_and(|c| Arc::ptr_eq(c, capture)) {
            captures.remove(&capture.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::synth;

    /// Counts opens and drops; frames are the synthetic source's, or an error after `fail_after`
    /// frames.
    #[derive(Clone, Default)]
    struct Counts {
        opened: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
        frames: Arc<AtomicUsize>,
    }

    struct Fake {
        counts: Counts,
        fail_after: Option<usize>,
    }

    struct Counted {
        inner: Box<dyn Source>,
        counts: Counts,
        fail_after: Option<usize>,
    }

    impl Source for Counted {
        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let n = self.counts.frames.fetch_add(1, Ordering::SeqCst);
            if self.fail_after.is_some_and(|max| n >= max) {
                return Err(Error::Capture("unplugged".into()));
            }
            self.inner.frame(spec, tick)
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.counts.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Opener for Fake {
        fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
            // "webcam:any" plays a webcam with two spellings.
            match spec {
                SourceSpec::Webcam(_) => Ok(SourceSpec::Webcam("the-id".into())),
                other => Ok(other.clone()),
            }
        }

        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> Result<Box<dyn Source>> {
            let inner = match source {
                SourceSpec::Webcam(_) => synth::open(&SourceSpec::Pattern(crate::id::Pattern::Testcard))?,
                other => synth::open(other)?,
            };
            self.counts.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Counted { inner, counts: self.counts.clone(), fail_after: self.fail_after }))
        }
    }

    fn hub(fail_after: Option<usize>) -> (Hub, Counts) {
        let counts = Counts::default();
        (Hub::new(Fake { counts: counts.clone(), fail_after }), counts)
    }

    fn spec(w: u32, h: u32, fps: u32) -> FrameSpec {
        FrameSpec::new(w, h, fps).unwrap_or_default()
    }

    fn parse(id: &str) -> SourceSpec {
        SourceSpec::parse(id).unwrap_or_else(|e| panic!("{id}: {e}"))
    }

    fn next(sub: &Subscription) -> CameraFrame {
        match sub.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(f)) => f,
            other => panic!("no frame from {}: {other:?}", sub.source()),
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn subscribers_of_one_source_share_one_capture_and_its_pixels() {
        let (hub, counts) = hub(None);
        let a = hub.subscribe(&parse("solid:red"), spec(64, 48, 60)).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("solid:red"), spec(64, 48, 60)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
        assert_eq!(hub.active(), vec![Active { source: "solid:red".into(), subscribers: 2 }]);
        let (fa, fb) = (next(&a), next(&b));
        assert!(Arc::ptr_eq(&fa.bgra, &fb.bgra), "one frame, shared");
        assert_eq!(fa.pixel(0, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn two_spellings_of_one_webcam_share_a_capture() {
        let (hub, counts) = hub(None);
        let a = hub.subscribe(&parse("webcam:obs"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("OBS Virtual Camera"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 1);
        assert_eq!((a.source(), b.source()), ("webcam:the-id", "webcam:the-id"));
    }

    #[test]
    fn the_capture_stops_with_the_last_subscriber_and_restarts_on_demand() {
        let (hub, counts) = hub(None);
        let a = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 120)).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 120)).unwrap_or_else(|e| panic!("{e}"));
        next(&a);
        drop(a);
        next(&b);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 0, "still one subscriber");
        drop(b);
        wait_until("the source to be dropped", || counts.dropped.load(Ordering::SeqCst) == 1);
        assert!(hub.active().is_empty());

        let c = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 120)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 2, "a fresh capture");
        let first = next(&c);
        assert_eq!(crate::synth::testcard_seq(&first), Some(first.tick.seq as u32));
    }

    #[test]
    fn different_sources_get_their_own_captures() {
        let (hub, counts) = hub(None);
        let _a = hub.subscribe(&parse("solid:red"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        let _b = hub.subscribe(&parse("qr:hello"), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        // An id a thread name couldn't hold.
        let _c = hub.subscribe(&SourceSpec::Qr("a\0b".into()), FrameSpec::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(counts.opened.load(Ordering::SeqCst), 3);
        assert_eq!(hub.active().len(), 3);
    }

    #[test]
    fn each_subscriber_gets_its_own_size() {
        let (hub, _) = hub(None);
        let big = hub.subscribe(&parse("pattern:testcard"), spec(640, 480, 60)).unwrap_or_else(|e| panic!("{e}"));
        let small = hub.subscribe(&parse("pattern:testcard"), spec(320, 240, 60)).unwrap_or_else(|e| panic!("{e}"));
        // Frames already queued for `big` before `small` joined were at 640x480 either way.
        let (fb, fs) = (next(&big), next(&small));
        assert_eq!((fb.width, fb.height), (640, 480));
        assert_eq!((fs.width, fs.height), (320, 240));
        assert_eq!(crate::synth::testcard_seq(&fs), Some(fs.tick.seq as u32), "scaled from the capture frame");
    }

    #[test]
    fn a_slower_subscriber_gets_fewer_frames() {
        let (hub, _) = hub(None);
        let fast = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let slow = hub.subscribe(&parse("pattern:testcard"), spec(64, 48, 10)).unwrap_or_else(|e| panic!("{e}"));
        let (mut n_fast, mut n_slow) = (0, 0);
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            n_fast += usize::from(matches!(fast.try_recv(), Ok(Some(_))));
            n_slow += usize::from(matches!(slow.try_recv(), Ok(Some(_))));
            thread::sleep(Duration::from_millis(2));
        }
        // Loose bounds: a loaded CI host runs the capture thread late, but never lets the 10 fps
        // subscriber see more than its share.
        assert!((1..=10).contains(&n_slow), "slow got {n_slow}");
        assert!(n_fast > n_slow, "fast got {n_fast}, slow {n_slow}");
    }

    #[test]
    fn a_failing_capture_tells_every_subscriber_and_stops() {
        let (hub, counts) = hub(Some(2));
        let a = hub.subscribe(&parse("solid:blue"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let b = hub.subscribe(&parse("solid:blue"), spec(64, 48, 100)).unwrap_or_else(|e| panic!("{e}"));
        let failure = |s: &Subscription| loop {
            match s.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        assert_eq!(failure(&a), Err(Error::Capture("unplugged".into())));
        assert_eq!(failure(&b), Err(Error::Capture("unplugged".into())));
        wait_until("the source to be dropped", || counts.dropped.load(Ordering::SeqCst) == 1);
        assert!(hub.active().is_empty());
        assert!(matches!(a.recv_timeout(Duration::from_millis(10)), Err(Error::Capture(_))));
    }

    #[test]
    fn an_open_failure_is_returned_to_the_subscriber() {
        let (hub, counts) = hub(None);
        let err = hub.subscribe(&parse("video:/x.mp4"), FrameSpec::default()).err();
        assert!(matches!(err, Some(Error::Unsupported(_))), "{err:?}");
        assert_eq!(counts.opened.load(Ordering::SeqCst), 0);
        assert!(hub.active().is_empty());
    }

    #[test]
    fn the_capture_runs_at_the_largest_size_and_highest_rate() {
        let subs: Vec<Subscriber> = [spec(320, 240, 60), spec(1280, 720, 15), spec(640, 480, 30)]
            .into_iter()
            .enumerate()
            .map(|(i, spec)| Subscriber { id: i as u64, spec, tx: mpsc::sync_channel(1).0, due: Instant::now() })
            .collect();
        assert_eq!(capture_spec(&subs), Some(spec(1280, 720, 60)));
        assert_eq!(capture_spec(&[]), None);
    }
}
