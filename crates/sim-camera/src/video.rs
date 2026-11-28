//! Video files as cameras (`video:<path>`): decoded frame by frame and looped, each capture tick
//! showing the frame the file shows at that moment of playback. A file plays at its own frame
//! rate whatever rate the capture runs at: a faster capture repeats frames (shared, not copied), a
//! slower one skips them. Decoding is the host's ([`Decode`]: AVFoundation's `AVAssetReader` on
//! macOS, `avf.rs`); this is the timeline, tested with a fake decoder.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::source::Source;

/// One decoded frame: BGRA, tightly packed, at its presentation time in the file.
#[derive(Clone, Debug)]
pub(crate) struct Picture {
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    pub bgra: Arc<[u8]>,
}

/// A host decoder for one file.
pub(crate) trait Decode: Send {
    /// The next frame in presentation order; `None` at the end of the file.
    fn next(&mut self) -> Result<Option<Picture>>;
    /// Starts over at the first frame.
    fn restart(&mut self) -> Result<()>;
    /// The file's nominal time between frames: how long the last frame shows when the file has
    /// only one.
    fn interval(&self) -> Duration;
}

/// A looping player over a decoder: the [`Source`] a `video:` id opens. It decodes one frame
/// ahead of the one it shows, so a tick costs a decode only when the file has moved on.
pub(crate) struct Player<D> {
    name: String,
    decoder: D,
    /// The frame on show.
    shown: Picture,
    /// The frame after it; `None` once the file has ended.
    pending: Option<Picture>,
    /// The first frame's time in the file (a file need not start at zero).
    first: Duration,
    /// The capture time at which the current pass of the file started.
    base: Duration,
    /// The time between the last two frames decoded: how long the last frame of a pass shows.
    gap: Option<Duration>,
    /// Frames decoded in this pass.
    frames: usize,
}

impl<D: Decode> Player<D> {
    /// A player showing the file's first frame; a file with none is refused here, at open.
    pub(crate) fn new(name: String, mut decoder: D) -> Result<Player<D>> {
        let shown = first_frame(&name, &mut decoder)?;
        let pending = decoder.next()?;
        let first = shown.pts;
        let frames = 1 + usize::from(pending.is_some());
        Ok(Player { name, decoder, shown, pending, first, base: Duration::ZERO, gap: None, frames })
    }

    /// Moves the playback to capture time `pts`: decodes past the frames due by then and, at the
    /// end of the file, starts it over (skipping whole passes at once after a long gap).
    fn seek(&mut self, pts: Duration) -> Result<()> {
        loop {
            let at = self.first + pts.saturating_sub(self.base);
            while let Some(next) = self.pending.take_if(|p| p.pts <= at) {
                self.gap = Some(next.pts.saturating_sub(self.shown.pts));
                self.shown = next;
                self.pending = self.decoder.next()?;
                self.frames += usize::from(self.pending.is_some());
            }
            // One frame in the file is a still: nothing to loop.
            if self.pending.is_some() || self.frames <= 1 {
                return Ok(());
            }
            let last = self.gap.filter(|g| !g.is_zero()).unwrap_or_else(|| self.decoder.interval());
            let length = (self.shown.pts.saturating_sub(self.first) + last).max(Duration::from_millis(1));
            let elapsed = pts.saturating_sub(self.base);
            if elapsed < length {
                return Ok(());
            }
            let passes = elapsed.as_nanos() / length.as_nanos();
            self.base += Duration::from_nanos(u64::try_from(length.as_nanos() * passes).unwrap_or(u64::MAX));
            self.decoder.restart()?;
            self.shown = first_frame(&self.name, &mut self.decoder)?;
            self.first = self.shown.pts;
            self.pending = self.decoder.next()?;
            (self.gap, self.frames) = (None, 1 + usize::from(self.pending.is_some()));
        }
    }
}

fn first_frame(name: &str, decoder: &mut impl Decode) -> Result<Picture> {
    decoder.next()?.ok_or_else(|| Error::Invalid(format!("{name} has no video frames")))
}

impl<D: Decode> Source for Player<D> {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        self.seek(tick.pts)?;
        let shown = &self.shown;
        let frame = CameraFrame { width: shown.width, height: shown.height, bgra: shown.bgra.clone(), tick };
        Ok(frame.fit(spec.width, spec.height))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Frames of one value each (`[v, v, v, 255]`), at the given times in milliseconds.
    pub(crate) struct Fake {
        frames: Vec<(u64, u8)>,
        at: usize,
        interval: Duration,
        pub(crate) restarts: Arc<Mutex<usize>>,
        decoded: Arc<Mutex<usize>>,
    }

    impl Fake {
        fn new(frames: &[(u64, u8)], interval_ms: u64) -> Fake {
            Fake {
                frames: frames.to_vec(),
                at: 0,
                interval: Duration::from_millis(interval_ms),
                restarts: Arc::default(),
                decoded: Arc::default(),
            }
        }
    }

    impl Decode for Fake {
        fn next(&mut self) -> Result<Option<Picture>> {
            let Some(&(ms, v)) = self.frames.get(self.at) else { return Ok(None) };
            self.at += 1;
            *self.decoded.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            Ok(Some(Picture { pts: Duration::from_millis(ms), width: 2, height: 2, bgra: [v, v, v, 255].repeat(4).into() }))
        }

        fn restart(&mut self) -> Result<()> {
            self.at = 0;
            *self.restarts.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            Ok(())
        }

        fn interval(&self) -> Duration {
            self.interval
        }
    }

    fn at(ms: u64) -> Tick {
        Tick { seq: ms, pts: Duration::from_millis(ms), ..Tick::start() }
    }

    fn spec() -> FrameSpec {
        FrameSpec { width: 2, height: 2, fps: 30 }
    }

    fn value(player: &mut Player<Fake>, ms: u64) -> u8 {
        player.frame(spec(), at(ms)).map(|f| f.pixel(0, 0)[0]).unwrap_or_else(|e| panic!("{ms} ms: {e}"))
    }

    /// Four frames at 10 fps: a 400 ms loop.
    fn clip() -> Fake {
        Fake::new(&[(0, 10), (100, 20), (200, 30), (300, 40)], 100)
    }

    #[test]
    fn each_tick_shows_the_frame_the_file_shows_then() {
        let mut p = Player::new("video:clip".into(), clip()).unwrap_or_else(|e| panic!("{e}"));
        let shown: Vec<u8> = [0, 33, 99, 100, 150, 250, 399].iter().map(|&ms| value(&mut p, ms)).collect();
        assert_eq!(shown, [10, 10, 10, 20, 20, 30, 40]);
    }

    #[test]
    fn the_file_loops_at_its_own_length() {
        let mut p = Player::new("video:clip".into(), clip()).unwrap_or_else(|e| panic!("{e}"));
        let restarts = p.decoder.restarts.clone();
        let shown: Vec<u8> = [0, 399, 400, 499, 500, 850, 1300].iter().map(|&ms| value(&mut p, ms)).collect();
        assert_eq!(shown, [10, 40, 10, 10, 20, 10, 20]);
        assert_eq!(*restarts.lock().unwrap_or_else(|e| e.into_inner()), 3, "one restart per pass reached");
    }

    #[test]
    fn a_long_gap_skips_whole_passes_with_one_restart() {
        let mut p = Player::new("video:clip".into(), clip()).unwrap_or_else(|e| panic!("{e}"));
        let restarts = p.decoder.restarts.clone();
        assert_eq!(value(&mut p, 0), 10);
        // 100 passes later, 250 ms into the pass.
        assert_eq!(value(&mut p, 40_250), 30);
        assert_eq!(*restarts.lock().unwrap_or_else(|e| e.into_inner()), 1);
    }

    #[test]
    fn a_repeated_frame_shares_its_pixels() {
        let mut p = Player::new("video:clip".into(), clip()).unwrap_or_else(|e| panic!("{e}"));
        let a = p.frame(spec(), at(10)).unwrap_or_else(|e| panic!("{e}"));
        let b = p.frame(spec(), at(40)).unwrap_or_else(|e| panic!("{e}"));
        assert!(Arc::ptr_eq(&a.bgra, &b.bgra));
        assert_eq!(b.tick.seq, 40, "the frame carries the capture's tick");
        let big = p.frame(FrameSpec { width: 8, height: 4, fps: 30 }, at(50)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((big.width, big.height, big.bgra.len()), (8, 4, 8 * 4 * 4), "scaled to the request");
    }

    #[test]
    fn a_file_that_starts_late_and_runs_unevenly_keeps_its_timing() {
        // The first frame at 1 s; gaps of 50 and 150 ms; the last shows for its own gap (150 ms).
        let mut p = Player::new("video:vfr".into(), Fake::new(&[(1000, 1), (1050, 2), (1200, 3)], 40)).unwrap_or_else(|e| panic!("{e}"));
        let shown: Vec<u8> = [0, 49, 50, 199, 200, 349, 350].iter().map(|&ms| value(&mut p, ms)).collect();
        assert_eq!(shown, [1, 1, 2, 2, 3, 3, 1]);
    }

    #[test]
    fn a_single_frame_file_is_a_still_and_never_redecoded() {
        let mut p = Player::new("video:one".into(), Fake::new(&[(0, 7)], 33)).unwrap_or_else(|e| panic!("{e}"));
        let (restarts, decoded) = (p.decoder.restarts.clone(), p.decoder.decoded.clone());
        for ms in [0, 33, 1000, 60_000] {
            assert_eq!(value(&mut p, ms), 7);
        }
        assert_eq!(*restarts.lock().unwrap_or_else(|e| e.into_inner()), 0);
        assert_eq!(*decoded.lock().unwrap_or_else(|e| e.into_inner()), 1);
    }

    #[test]
    fn a_file_with_no_frames_is_refused_at_open() {
        let err = Player::new("video:empty.mov".into(), Fake::new(&[], 33)).err();
        assert!(matches!(&err, Some(Error::Invalid(m)) if m.contains("video:empty.mov") && m.contains("no video frames")), "{err:?}");
    }

    struct Failing;

    impl Decode for Failing {
        fn next(&mut self) -> Result<Option<Picture>> {
            Err(Error::Capture("decode error -12909".into()))
        }
        fn restart(&mut self) -> Result<()> {
            Ok(())
        }
        fn interval(&self) -> Duration {
            Duration::from_millis(33)
        }
    }

    #[test]
    fn a_decode_error_is_the_capture_error() {
        let err = Player::new("video:bad".into(), Failing).err();
        assert!(matches!(&err, Some(Error::Capture(m)) if m.contains("-12909")), "{err:?}");
    }
}
