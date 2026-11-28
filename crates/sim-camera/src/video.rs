//! Video files as cameras (`video:<path>`): decoded frame by frame and looped, each capture tick
//! showing the frame the file shows at that moment of playback. A file plays at its own frame
//! rate whatever rate the capture runs at: a faster capture repeats frames, a slower one skips
//! them. Only the frame a tick shows is copied out of the decoder, and it is fitted once per
//! output size, so a repeated frame costs nothing. Decoding is the host's ([`Decode`]:
//! AVFoundation's `AVAssetReader` on macOS, `avf.rs`); this is the timeline, tested with a fake
//! decoder.

use std::sync::Arc;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::frame::{CameraFrame, Fitted, FrameSpec, Tick, fill};
use crate::source::Source;
use crate::synth::check_size_of;

/// One decoded frame, not yet copied out: its presentation time in the file, its size as shown
/// (after the track's rotation), and the decoder's handle to its pixels.
pub(crate) struct Sample<S> {
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    pub data: S,
}

/// A frame copied out of the decoder: BGRA, tightly packed, upright.
#[derive(Clone, Debug)]
struct Picture {
    width: u32,
    height: u32,
    bgra: Arc<[u8]>,
}

/// A host decoder for one file.
pub(crate) trait Decode: Send {
    /// The decoder's handle to one decoded frame (a retained `CMSampleBuffer` on macOS).
    type Data: Send;
    /// The next frame in presentation order; `None` at the end of the file.
    fn next(&mut self) -> Result<Option<Sample<Self::Data>>>;
    /// A frame's pixels: BGRA, tightly packed and upright, `width * height * 4` bytes.
    fn pixels(&mut self, sample: &Sample<Self::Data>) -> Result<Vec<u8>>;
    /// Starts over at file time `to`: the next frame is the one showing then (the first frame
    /// when `to` is before it).
    fn seek(&mut self, to: Duration) -> Result<()>;
    /// The file's nominal time between frames: how long the last frame shows when the file has
    /// only one.
    fn interval(&self) -> Duration;
    /// The file time the track ends at, when the container says.
    fn end(&self) -> Option<Duration>;
}

/// The most frames one tick decodes: past this, playback jumps (when it knows where the file
/// ends) or catches up over the next ticks, so a long stall never blocks the capture thread.
const DECODES_PER_TICK: usize = 120;

/// Decode failures in a row that end the capture; each one before restarts the reader where
/// playback is, after a delay that doubles from [`RETRY`].
const FAILURES_ALLOWED: usize = 5;
const RETRY: Duration = Duration::from_millis(100);

/// Output sizes a player keeps the frame on show fitted to at once.
const FITTED_SIZES: usize = 4;

/// A looping player over a decoder: the [`Source`] a `video:` id opens. It decodes one frame
/// ahead of the one it shows, so a tick costs a decode only when the file has moved on.
pub(crate) struct Player<D: Decode> {
    name: String,
    decoder: D,
    /// The frame on show, copied out.
    shown: Picture,
    /// The frame `shown` is fitted to, by output size.
    fitted: Fitted,
    /// The newest frame due, when it is newer than `shown`: copied out only when a tick shows it.
    due: Option<Sample<D::Data>>,
    /// The time in the file of the newest frame due (`due`'s, or `shown`'s).
    now: Duration,
    /// The frame after it; `None` once the file has ended.
    pending: Option<Sample<D::Data>>,
    /// The first frame's time in the file (a file need not start at zero).
    first: Duration,
    /// The capture time at which the current pass of the file started.
    base: Duration,
    /// The time between the last two frames decoded: how long the last frame of a pass shows.
    gap: Option<Duration>,
    /// How long one pass lasts: measured once a pass has been decoded to its end; until then the
    /// container's end stands in, when a stall in the first pass has gone past it (or a jump has
    /// found no frame before it).
    length: Option<Duration>,
    /// `length` is the measured one: the last frame's time plus the gap, replacing the container's.
    measured: bool,
    /// The file time the decoder last restarted at, until playback reaches it: no jump before
    /// then, so a decoder that starts a jump at the keyframe before its time catches up instead
    /// of jumping again every tick.
    target: Option<Duration>,
    /// The file has one frame: a still, never decoded again.
    still: bool,
    /// Decode failures in a row, and the capture time to restart the reader at after the last.
    failures: usize,
    retry_at: Option<Duration>,
}

impl<D: Decode> Player<D> {
    /// A player showing the file's first frame; a file with none, or with frames larger than an
    /// `image:` may be, is refused here, at open.
    pub(crate) fn new(name: String, mut decoder: D) -> Result<Player<D>> {
        let first = decoder.next()?.ok_or_else(|| no_frames(&name))?;
        let shown = copy_out(&name, &mut decoder, &first)?;
        let pending = decoder.next()?;
        Ok(Player {
            still: pending.is_none(),
            now: first.pts,
            first: first.pts,
            name,
            decoder,
            shown,
            fitted: Vec::new(),
            due: None,
            pending,
            base: Duration::ZERO,
            gap: None,
            length: None,
            measured: false,
            target: None,
            failures: 0,
            retry_at: None,
        })
    }

    /// Moves the playback to capture time `pts`: decodes past the frames due by then and, at the
    /// end of the file, starts it over (skipping whole passes at once after a long gap). Decodes
    /// at most [`DECODES_PER_TICK`] frames; further behind than that, it jumps when it knows
    /// where the file ends (wrapping when that is behind it, and not again before it has
    /// reached the last jump's time), and otherwise leaves the rest to the next ticks.
    fn advance(&mut self, pts: Duration) -> Result<()> {
        if self.still {
            return Ok(());
        }
        if self.retry_at.is_some() {
            // After a decode failure: a fresh reader, where playback is now.
            self.reposition(self.first + pts.saturating_sub(self.base))?;
        }
        let mut budget = DECODES_PER_TICK;
        loop {
            if let Some(length) = self.length {
                let elapsed = pts.saturating_sub(self.base);
                if elapsed >= length {
                    let passes = elapsed.as_nanos() / length.as_nanos();
                    self.base += Duration::from_nanos(u64::try_from(length.as_nanos() * passes).unwrap_or(u64::MAX));
                    self.reposition(self.first + pts.saturating_sub(self.base))?;
                }
            }
            let at = self.first + pts.saturating_sub(self.base);
            if self.target.is_some_and(|t| self.now >= t) {
                self.target = None;
            }
            if let Some(next) = &self.pending
                && at > next.pts
                && (at - next.pts).as_nanos() / self.decoder.interval().as_nanos().max(1) >= budget as u128
            {
                if self.before_end(at) {
                    if self.target.is_none() {
                        let before = self.length;
                        self.reposition(at)?;
                        if self.length != before {
                            // No frame at `at`: the gap is the end, and playback has wrapped.
                            continue;
                        }
                    }
                } else if self.length.is_none()
                    && let Some(end) = self.decoder.end().filter(|&end| end > self.first)
                {
                    // A stall past the end the container gives, before the pass has been decoded
                    // to it: that end stands in for the measured length, so playback wraps
                    // instead of catching up.
                    self.length = Some(end - self.first);
                    continue;
                }
            }
            while let Some(next) = self.pending.take_if(|p| p.pts <= at) {
                if budget == 0 {
                    self.pending = Some(next);
                    return Ok(());
                }
                budget -= 1;
                self.gap = Some(next.pts.saturating_sub(self.now));
                self.now = next.pts;
                self.due = Some(next);
                self.pending = self.decoder.next()?;
            }
            if self.pending.is_some() {
                return Ok(());
            }
            // The end of the pass: it lasts until the last frame's time plus the gap before it.
            let last = self.gap.filter(|g| !g.is_zero()).unwrap_or_else(|| self.decoder.interval());
            let measured = (self.now.saturating_sub(self.first) + last).max(Duration::from_millis(1));
            if !self.measured {
                // The first full pass replaces the container's length, so the loop does not
                // depend on whether the host stalled.
                (self.length, self.measured) = (Some(measured), true);
            }
            let length = self.length.unwrap_or(measured);
            if pts.saturating_sub(self.base) < length {
                return Ok(());
            }
        }
    }

    /// Whether file time `at` is before the end of the file, as far as the player knows.
    fn before_end(&self, at: Duration) -> bool {
        self.length.map(|l| self.first + l).or_else(|| self.decoder.end()).is_some_and(|end| at < end)
    }

    /// Restarts the decoder at file time `at` (from the first frame when `at` is past the end),
    /// its first frame due; playback doesn't jump again until it has reached `at`.
    fn reposition(&mut self, at: Duration) -> Result<()> {
        self.decoder.seek(at)?;
        self.target = Some(at);
        let mut sample = self.decoder.next()?;
        if sample.is_none() && at > self.first {
            // No frame at `at`, in the gap between the last frame and the container's end: treat
            // the gap as the end of the pass, and start over.
            if !self.measured {
                self.length = Some(at - self.first);
            }
            self.decoder.seek(Duration::ZERO)?;
            self.target = None;
            sample = self.decoder.next()?;
        }
        let sample = sample.ok_or_else(|| no_frames(&self.name))?;
        self.now = sample.pts;
        self.due = Some(sample);
        self.gap = None;
        self.pending = self.decoder.next()?;
        Ok(())
    }

    /// Copies the frame due out of the decoder, to show it.
    fn show(&mut self) -> Result<()> {
        if let Some(sample) = self.due.take() {
            self.shown = copy_out(&self.name, &mut self.decoder, &sample)?;
            self.fitted.clear();
        }
        Ok(())
    }

    /// The frame on show at `size`: its own pixels, or its fit to that size, made once.
    fn fitted(&mut self, size: (u32, u32)) -> Arc<[u8]> {
        let shown = &self.shown;
        if size == (shown.width, shown.height) {
            return shown.bgra.clone();
        }
        if let Some((_, bgra)) = self.fitted.iter().find(|(s, _)| *s == size) {
            return bgra.clone();
        }
        let bgra: Arc<[u8]> = fill(shown.width, shown.height, &shown.bgra, size.0, size.1).into();
        if self.fitted.len() >= FITTED_SIZES {
            self.fitted.remove(0);
        }
        self.fitted.push((size, bgra.clone()));
        bgra
    }
}

fn no_frames(name: &str) -> Error {
    Error::Invalid(format!("{name} has no video frames"))
}

/// A sample's pixels, after checking its size against what `image:` accepts.
fn copy_out<D: Decode>(name: &str, decoder: &mut D, sample: &Sample<D::Data>) -> Result<Picture> {
    let (width, height) = (sample.width, sample.height);
    check_size_of("video", width, height).map_err(|e| Error::Invalid(format!("{name}: {e}")))?;
    let bgra = decoder.pixels(sample)?;
    if bgra.len() != width as usize * height as usize * 4 {
        return Err(Error::Capture(format!("{name}: a {width}x{height} frame came out as {} bytes", bgra.len())));
    }
    Ok(Picture { width, height, bgra: bgra.into() })
}

impl<D: Decode> Source for Player<D> {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        if self.retry_at.is_none_or(|at| tick.pts >= at) {
            let played = self.advance(tick.pts);
            // The frame due before any failure is still shown.
            match played.and(self.show()) {
                Ok(()) => (self.failures, self.retry_at) = (0, None),
                Err(e) => {
                    self.due = None;
                    self.failures += 1;
                    if self.failures >= FAILURES_ALLOWED {
                        return Err(e);
                    }
                    let backoff = RETRY * (1u32 << (self.failures - 1).min(16));
                    self.retry_at = Some(tick.pts + backoff);
                }
            }
        }
        let bgra = self.fitted((spec.width, spec.height));
        Ok(CameraFrame { width: spec.width, height: spec.height, bgra, tick })
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
        size: (u32, u32),
        /// Frame indexes whose decode fails once each (a one-off decoder hiccup).
        fail_once: Vec<usize>,
        /// Every decode from this frame index on fails.
        broken_from: Option<usize>,
        /// Where the file says it ends, when it says.
        end: Option<Duration>,
        /// Frames between keyframes, when a seek starts at the keyframe before its time (a
        /// decoder that delivers a jump from its sync sample).
        keyframes: Option<usize>,
        /// A seek further past the last frame than one interval finds no frame (a container whose
        /// track runs past its last sample).
        empty_past_last: bool,
        pub(crate) restarts: Arc<Mutex<usize>>,
        decoded: Arc<Mutex<usize>>,
        copies: Arc<Mutex<usize>>,
    }

    impl Fake {
        fn new(frames: &[(u64, u8)], interval_ms: u64) -> Fake {
            Fake {
                frames: frames.to_vec(),
                at: 0,
                interval: Duration::from_millis(interval_ms),
                size: (2, 2),
                fail_once: Vec::new(),
                broken_from: None,
                end: None,
                keyframes: None,
                empty_past_last: false,
                restarts: Arc::default(),
                decoded: Arc::default(),
                copies: Arc::default(),
            }
        }

        /// `n` frames `step_ms` apart, frame `i` of value `i % 251`.
        fn steady(n: usize, step_ms: u64) -> Fake {
            let frames: Vec<(u64, u8)> = (0..n).map(|i| (i as u64 * step_ms, (i % 251) as u8)).collect();
            Fake::new(&frames, step_ms)
        }

        fn sized(mut self, width: u32, height: u32) -> Fake {
            self.size = (width, height);
            self
        }

        fn ending_at(mut self, ms: u64) -> Fake {
            self.end = Some(Duration::from_millis(ms));
            self
        }

        fn keyframe_every(mut self, frames: usize) -> Fake {
            self.keyframes = Some(frames);
            self
        }

        fn empty_past_last_frame(mut self) -> Fake {
            self.empty_past_last = true;
            self
        }

        fn failing_once_at(mut self, index: usize) -> Fake {
            self.fail_once.push(index);
            self
        }

        fn broken_from(mut self, index: usize) -> Fake {
            self.broken_from = Some(index);
            self
        }

        /// The decode of frame `self.at`, failing as configured.
        fn check(&mut self) -> Result<()> {
            let failed = self.broken_from.is_some_and(|i| self.at >= i) || {
                let once = self.fail_once.iter().position(|&i| i == self.at);
                once.map(|i| self.fail_once.remove(i)).is_some()
            };
            if failed {
                return Err(Error::Capture(format!("decode error -12909 at frame {}", self.at)));
            }
            Ok(())
        }
    }

    fn count(n: &Arc<Mutex<usize>>) -> usize {
        *n.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Decode for Fake {
        type Data = u8;

        fn next(&mut self) -> Result<Option<Sample<u8>>> {
            let Some(&(ms, v)) = self.frames.get(self.at) else { return Ok(None) };
            self.check()?;
            self.at += 1;
            *self.decoded.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            let (width, height) = self.size;
            Ok(Some(Sample { pts: Duration::from_millis(ms), width, height, data: v }))
        }

        fn pixels(&mut self, sample: &Sample<u8>) -> Result<Vec<u8>> {
            *self.copies.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            let v = sample.data;
            Ok([v, v, v, 255].repeat((sample.width * sample.height) as usize))
        }

        /// To the last frame at or before `to` (the first, when `to` is before it), or to the
        /// keyframe before that one.
        fn seek(&mut self, to: Duration) -> Result<()> {
            let at = self.frames.iter().rposition(|&(ms, _)| Duration::from_millis(ms) <= to).unwrap_or(0);
            self.at = self.keyframes.map_or(at, |k| at / k.max(1) * k.max(1));
            if self.empty_past_last && self.frames.last().is_some_and(|&(ms, _)| to > Duration::from_millis(ms) + self.interval) {
                self.at = self.frames.len();
            }
            *self.restarts.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            Ok(())
        }

        fn interval(&self) -> Duration {
            self.interval
        }

        fn end(&self) -> Option<Duration> {
            self.end
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

    #[test]
    fn a_repeated_frame_is_fitted_once_per_size() {
        let mut p = Player::new("video:clip".into(), clip()).unwrap_or_else(|e| panic!("{e}"));
        let big = FrameSpec { width: 8, height: 4, fps: 30 };
        let a = p.frame(big, at(10)).unwrap_or_else(|e| panic!("{e}"));
        let b = p.frame(big, at(40)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((b.width, b.height), (8, 4));
        assert!(Arc::ptr_eq(&a.bgra, &b.bgra), "the same frame at the same size is fitted once");
        let c = p.frame(big, at(150)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.pixel(0, 0)[0], 20, "a new frame is fitted afresh");
    }

    #[test]
    fn a_skipped_frame_is_never_copied() {
        let fake = Fake::steady(100, 10);
        let copies = fake.copies.clone();
        let mut p = Player::new("video:steady".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(count(&copies), 1, "only the frame on show is copied out, not the one decoded ahead");
        assert_eq!(value(&mut p, 995), 99);
        assert_eq!(count(&copies), 2, "98 frames skipped straight past, none copied");
    }

    #[test]
    fn a_long_stall_decodes_a_bounded_number_of_frames() {
        // 100 s at 100 fps; the container says where it ends, so playback can jump.
        let fake = Fake::steady(10_000, 10).ending_at(100_000);
        let (decoded, restarts) = (fake.decoded.clone(), fake.restarts.clone());
        let mut p = Player::new("video:long".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&mut p, 0), 0);
        let before = count(&decoded);
        assert_eq!(value(&mut p, 90_005), (9000 % 251) as u8, "the frame due 90 s in");
        let decodes = count(&decoded) - before;
        assert!(decodes <= DECODES_PER_TICK + 2, "{decodes} frames decoded in one tick");
        assert_eq!(count(&restarts), 1, "one jump");
        assert_eq!(value(&mut p, 90_015), (9001 % 251) as u8, "and it plays on from there");
    }

    #[test]
    fn after_a_pass_a_long_stall_wraps_without_decoding_the_rest() {
        // 10 s at 100 fps, its length unknown until one pass has played.
        let fake = Fake::steady(1000, 10);
        let decoded = fake.decoded.clone();
        let mut p = Player::new("video:pass".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        for ms in (0..=10_050).step_by(50) {
            assert_eq!(value(&mut p, ms), ((ms % 10_000 / 10) % 251) as u8, "{ms} ms");
        }
        let before = count(&decoded);
        // 25 passes later, 9 s into the pass.
        let ms = 10_050 + 25 * 10_000 + 8_950;
        assert_eq!(value(&mut p, ms), (900 % 251) as u8);
        let decodes = count(&decoded) - before;
        assert!(decodes <= DECODES_PER_TICK + 2, "{decodes} frames decoded in one tick");
    }

    #[test]
    fn a_stall_in_an_unmeasured_pass_catches_up_in_bounded_steps() {
        // No length from the container and no pass played yet: playback can't jump safely, so
        // each tick decodes at most its share and the next ones catch up.
        let fake = Fake::steady(1000, 10);
        let decoded = fake.decoded.clone();
        let mut p = Player::new("video:unmeasured".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        let mut last = count(&decoded);
        let mut shown = 0;
        for tick in 0..20u64 {
            let ms = 9_000 + tick * 33;
            shown = value(&mut p, ms);
            let decodes = count(&decoded) - last;
            assert!(decodes <= DECODES_PER_TICK, "{decodes} frames decoded at {ms} ms");
            last = count(&decoded);
        }
        assert_eq!(shown, (((9_000 + 19 * 33) / 10) % 251) as u8, "caught up");
    }

    #[test]
    fn a_jump_into_a_long_gop_catches_up_without_jumping_again() {
        // 20 s at 100 fps with a keyframe every 10 s, from a decoder that starts a jump at the
        // keyframe before it: after the jump, playback is still 500 frames behind.
        let fake = Fake::steady(2000, 10).ending_at(20_000).keyframe_every(1000);
        let restarts = fake.restarts.clone();
        let mut p = Player::new("video:gop".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&mut p, 0), 0);
        let shown: Vec<u8> = (0..10u64).map(|tick| value(&mut p, 15_000 + tick * 10)).collect();
        assert_eq!(count(&restarts), 1, "one jump, not one per tick: {shown:?}");
        assert_eq!(shown[9], (1509 % 251) as u8, "caught up: {shown:?}");
    }

    #[test]
    fn a_first_pass_stalled_past_a_known_end_wraps() {
        // 10 s at 100 fps; the container says where it ends, and a stall passes it before the
        // first pass has been decoded to its end.
        let fake = Fake::steady(1000, 10).ending_at(10_000);
        let (decoded, restarts) = (fake.decoded.clone(), fake.restarts.clone());
        let mut p = Player::new("video:stalled".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&mut p, 0), 0);
        let before = count(&decoded);
        assert_eq!(value(&mut p, 12_500), 250, "2.5 s into the second pass");
        let decodes = count(&decoded) - before;
        assert!(decodes <= DECODES_PER_TICK + 2, "{decodes} frames decoded in one tick");
        assert_eq!(count(&restarts), 1, "one restart, at the wrap");
        assert_eq!(value(&mut p, 12_510), 0, "and it plays on from there (frame 251)");
    }

    #[test]
    fn a_measured_pass_replaces_the_length_the_container_gave() {
        // 10 s at 100 fps; the track runs to 12 s, 2 s past the last frame's end. A stall in the
        // first pass wraps on the container's 12 s; once a pass has played to its last frame, the
        // loop is that frame's time plus the gap (10 s), stall or no stall.
        let fake = Fake::steady(1000, 10).ending_at(12_000);
        let mut p = Player::new("video:long-track".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&mut p, 0), 0);
        assert_eq!(value(&mut p, 12_500), 50, "0.5 s into the second pass, wrapped on the container's length");
        for ms in (12_550..22_000).step_by(50) {
            value(&mut p, ms);
        }
        assert_eq!(value(&mut p, 22_000), 0, "the pass that started at 12 s lasts its measured 10 s");
        assert_eq!(value(&mut p, 22_050), 5);
    }

    #[test]
    fn a_jump_into_the_gap_after_the_last_frame_wraps_without_replaying_the_start() {
        // 10 s at 100 fps in a track that runs to 12 s: a stall lands 11 s in, where no frame is.
        let fake = Fake::steady(1000, 10).ending_at(12_000).empty_past_last_frame();
        let restarts = fake.restarts.clone();
        let mut p = Player::new("video:gap".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&mut p, 0), 0);
        let shown: Vec<u8> = (0..10u64).map(|tick| value(&mut p, 11_000 + tick * 10)).collect();
        assert_eq!(shown, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9], "starts over and plays on, no replay of the first frames");
        assert!(count(&restarts) <= 3, "{} restarts", count(&restarts));
    }

    #[test]
    fn a_frame_larger_than_image_allows_is_refused() {
        let err = Player::new("video:wide.mov".into(), Fake::new(&[(0, 1)], 33).sized(8193, 2)).err();
        assert!(matches!(&err, Some(Error::Invalid(m)) if m.contains("video:wide.mov") && m.contains("8192")), "{err:?}");
        assert!(Player::new("video:ok.mov".into(), Fake::new(&[(0, 1)], 33).sized(8192, 2)).is_ok());
    }

    #[test]
    fn one_decode_error_restarts_the_reader_and_playback_goes_on() {
        let fake = clip().failing_once_at(2);
        let restarts = fake.restarts.clone();
        let mut p = Player::new("video:hiccup".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(value(&mut p, 0), 10);
        assert_eq!(value(&mut p, 100), 20, "the frame due stays on show while the decoder recovers");
        let shown: Vec<u8> = [250, 399, 400, 500].iter().map(|&ms| value(&mut p, ms)).collect();
        assert_eq!(shown, [30, 40, 10, 20], "the reader restarted where playback was, and loops on");
        assert!(count(&restarts) >= 1);
    }

    #[test]
    fn repeated_decode_errors_end_the_capture_after_backoff() {
        let fake = clip().broken_from(2);
        let restarts = fake.restarts.clone();
        let mut p = Player::new("video:broken".into(), fake).unwrap_or_else(|e| panic!("{e}"));
        let mut ms = 100;
        let err = loop {
            match p.frame(spec(), at(ms)) {
                Ok(f) => assert_eq!(f.pixel(0, 0)[0], 20, "{ms} ms: the last good frame"),
                Err(e) => break e,
            }
            ms += 10;
            assert!(ms < 10_000, "never gave up");
        };
        assert!(matches!(&err, Error::Capture(m) if m.contains("-12909")), "{err:?}");
        // The first failure at 100 ms; retries 100, 200, 400 and 800 ms after each.
        assert!((1_600..1_700).contains(&ms), "gave up at {ms} ms");
        assert_eq!(count(&restarts), FAILURES_ALLOWED - 1, "one reader restart per retry");
    }

    struct Failing;

    impl Decode for Failing {
        type Data = ();
        fn next(&mut self) -> Result<Option<Sample<()>>> {
            Err(Error::Capture("decode error -12909".into()))
        }
        fn pixels(&mut self, _sample: &Sample<()>) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn seek(&mut self, _to: Duration) -> Result<()> {
            Ok(())
        }
        fn interval(&self) -> Duration {
            Duration::from_millis(33)
        }
        fn end(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn a_decode_error_is_the_capture_error() {
        let err = Player::new("video:bad".into(), Failing).err();
        assert!(matches!(&err, Some(Error::Capture(m)) if m.contains("-12909")), "{err:?}");
    }
}
