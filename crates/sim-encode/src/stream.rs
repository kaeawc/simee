//! The encode loop: a [`Source`] says when frames are captured and hands the newest one over as a
//! picture, the [`Pacer`] decides when to encode, the [`Encoder`] turns pictures into Annex B
//! access units, and the sink (a publisher) takes them. [`Controls`] carry what the sink's peer
//! asks for (a keyframe, a new bitrate, stop) in from other threads.
//!
//! The loop runs on the caller's thread and sleeps in [`Source::wait`]; it never polls. The clock
//! is injected so tests run it on made-up time.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::Result;
use crate::pacing::{Action, Pacer, Pacing, PtsClock};

/// One encoded picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    /// Annex B NAL units; an IDR has the SPS and PPS in front.
    pub data: Vec<u8>,
    /// From the stream's start to the frame's capture.
    pub pts: Duration,
    pub keyframe: bool,
    /// A re-encode of the last frame on a static screen.
    pub heartbeat: bool,
    /// From handing the picture to the encoder to having its output.
    pub encode_time: Duration,
}

/// Where frames come from.
pub trait Source {
    type Picture;
    /// Sleeps until a frame is captured or `deadline` passes; the frame's capture instant, or
    /// `None` at the deadline. An error ends the loop (the device went away).
    fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>>;
    /// The newest frame, ready for the encoder (the last one again when nothing new arrived);
    /// `None` before the first.
    fn picture(&mut self) -> Result<Option<&Self::Picture>>;
    /// When the picture a [`Source::picture`] call just took was captured, once, if that call
    /// took a new one (it can be newer than the frame `wait` reported): the stamp for it.
    fn take_captured(&mut self) -> Option<Instant> {
        None
    }
    /// Frames that arrived and were replaced by the one a [`Source::picture`] call just took,
    /// without `wait` ever reporting them; once.
    fn take_unreported(&mut self) -> u64 {
        0
    }
}

/// What turns pictures into access units.
pub trait Encoder<P> {
    /// Encodes one picture (an IDR when `keyframe`) and returns what came out of the encoder:
    /// usually that picture, nothing when the encoder dropped it.
    fn encode(&mut self, picture: &P, pts: Duration, keyframe: bool) -> Result<Vec<AccessUnit>>;
    /// Retargets the average bitrate.
    fn set_bitrate(&mut self, bps: u64) -> Result<()>;
}

/// Requests into a running loop, from any thread (a publisher's event callback).
#[derive(Debug, Default)]
pub struct Controls {
    keyframe: AtomicBool,
    bitrate: AtomicU64,
    stop: AtomicBool,
}

impl Controls {
    /// The next encode is an IDR (a viewer's PLI or FIR).
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Release);
    }

    /// Retarget the encoder (a bandwidth estimate), from the next frame on.
    pub fn set_bitrate(&self, bps: u64) {
        self.bitrate.store(bps.max(1), Ordering::Release);
    }

    /// End the loop after the current frame.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn keyframe_pending(&self) -> bool {
        self.keyframe.load(Ordering::Acquire)
    }

    fn take_keyframe(&self) -> bool {
        self.keyframe.swap(false, Ordering::AcqRel)
    }

    fn take_bitrate(&self) -> Option<u64> {
        Some(self.bitrate.swap(0, Ordering::AcqRel)).filter(|&b| b > 0)
    }
}

/// Whether the sink wants more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frames the source announced.
    pub frames: u64,
    /// Access units out of the encoder (heartbeats included).
    pub encoded: u64,
    pub heartbeats: u64,
    pub keyframes: u64,
    /// Pictures the encoder dropped.
    pub dropped: u64,
    /// Frames replaced by a newer one before the fps cap let them through.
    pub superseded: u64,
    pub bytes: u64,
}

/// The clock the loop reads.
pub trait Clock {
    fn now(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The longest the loop sleeps in one [`Source::wait`], so [`Controls::stop`] is seen promptly
/// on a static screen with no heartbeat.
pub const MAX_WAIT: Duration = Duration::from_millis(100);

/// Runs until the sink says [`Flow::Stop`], [`Controls::stop`] is called, or the source or the
/// encoder fails. Presentation times count from the call. The first picture is an IDR; after it,
/// one is forced whenever [`Controls::request_keyframe`] was called (on a static screen, the last
/// frame is re-encoded for it at once).
pub fn run<S: Source, E: Encoder<S::Picture>>(
    source: &mut S,
    encoder: &mut E,
    pacing: Pacing,
    controls: &Controls,
    sink: impl FnMut(AccessUnit) -> Result<Flow>,
) -> Result<Stats> {
    run_with_clock(source, encoder, pacing, controls, &SystemClock, sink)
}

/// [`run`] on an injected clock.
pub fn run_with_clock<S: Source, E: Encoder<S::Picture>>(
    source: &mut S,
    encoder: &mut E,
    pacing: Pacing,
    controls: &Controls,
    clock: &impl Clock,
    mut sink: impl FnMut(AccessUnit) -> Result<Flow>,
) -> Result<Stats> {
    let mut pts = PtsClock::new(clock.now());
    let mut pacer = Pacer::new(pacing);
    let mut stats = Stats::default();
    let mut first = true;
    let mut done = false;
    while !done && !controls.stopped() {
        if let Some(bps) = controls.take_bitrate() {
            encoder.set_bitrate(bps)?;
        }
        let now = clock.now();
        if controls.keyframe_pending() && !first {
            pacer.refresh(now);
        }
        match pacer.poll(now) {
            Action::Wait(deadline) => {
                let cap = now + MAX_WAIT;
                if let Some(at) = source.wait(deadline.map_or(cap, |d| d.min(cap)))? {
                    stats.frames += 1;
                    pacer.frame(at);
                }
            }
            Action::Encode { captured, heartbeat } => {
                pacer.encoded(now);
                if source.picture()?.is_none() {
                    continue;
                }
                let stamp = source.take_captured().filter(|_| !heartbeat).unwrap_or(captured);
                let extra = source.take_unreported();
                stats.frames += extra;
                stats.superseded += extra;
                let Some(picture) = source.picture()? else { continue };
                let keyframe = controls.take_keyframe() || first;
                let units = encoder.encode(picture, pts.stamp(stamp), keyframe)?;
                first = false;
                if units.is_empty() {
                    stats.dropped += 1;
                }
                // A requested IDR that the encoder dropped (or answered with a P frame) is asked
                // for again: the viewer is still waiting for it.
                if keyframe && !units.iter().any(|u| u.keyframe) {
                    controls.request_keyframe();
                }
                for mut unit in units {
                    unit.heartbeat = heartbeat;
                    stats.encoded += 1;
                    stats.heartbeats += u64::from(heartbeat);
                    stats.keyframes += u64::from(unit.keyframe);
                    stats.bytes += unit.data.len() as u64;
                    done |= sink(unit)? == Flow::Stop;
                }
            }
        }
    }
    stats.superseded += pacer.superseded;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct FakeClock(Rc<Cell<Instant>>);

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    /// Frames at the given offsets; time jumps to each frame (or to the deadline) as the loop
    /// waits, and the loop is stopped once time reaches `end`.
    struct FakeSource {
        now: Rc<Cell<Instant>>,
        t0: Instant,
        frames: Vec<Duration>,
        next: usize,
        end: Duration,
        newest: Option<usize>,
        controls: Rc<Controls>,
    }

    impl Source for FakeSource {
        type Picture = usize;

        fn wait(&mut self, deadline: Instant) -> Result<Option<Instant>> {
            let end = self.t0 + self.end;
            match self.frames.get(self.next).map(|&d| self.t0 + d) {
                Some(at) if at <= deadline => {
                    self.now.set(self.now.get().max(at));
                    self.newest = Some(self.next);
                    self.next += 1;
                    Ok(Some(at))
                }
                _ => {
                    self.now.set(deadline.min(end).max(self.now.get()));
                    if self.now.get() >= end {
                        self.controls.stop();
                    }
                    Ok(None)
                }
            }
        }

        fn picture(&mut self) -> Result<Option<&usize>> {
            Ok(self.newest.as_ref())
        }
    }

    #[derive(Default)]
    struct FakeEncoder {
        calls: Vec<(usize, Duration, bool)>,
        bitrates: Vec<u64>,
    }

    impl Encoder<usize> for FakeEncoder {
        fn encode(&mut self, picture: &usize, pts: Duration, keyframe: bool) -> Result<Vec<AccessUnit>> {
            self.calls.push((*picture, pts, keyframe));
            Ok(vec![AccessUnit { data: vec![*picture as u8], pts, keyframe, heartbeat: false, encode_time: Duration::ZERO }])
        }

        fn set_bitrate(&mut self, bps: u64) -> Result<()> {
            self.bitrates.push(bps);
            Ok(())
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Runs the fake source until its end; `on_unit` sees each unit and may use the controls.
    fn drive(
        frames: Vec<Duration>,
        end: Duration,
        pacing: Pacing,
        mut on_unit: impl FnMut(&AccessUnit, &Controls),
    ) -> (Stats, Vec<AccessUnit>, FakeEncoder) {
        let t0 = Instant::now();
        let now = Rc::new(Cell::new(t0));
        let controls = Rc::new(Controls::default());
        let mut source = FakeSource { now: now.clone(), t0, frames, next: 0, end, newest: None, controls: controls.clone() };
        let mut encoder = FakeEncoder::default();
        let mut units = Vec::new();
        let stats = run_with_clock(&mut source, &mut encoder, pacing, &controls, &FakeClock(now), |u| {
            on_unit(&u, &controls);
            units.push(u);
            Ok(Flow::Continue)
        })
        .expect("run");
        (stats, units, encoder)
    }

    #[test]
    fn frames_faster_than_the_cap_are_thinned_and_the_last_one_is_kept() {
        // 120 Hz for 100 ms into a 30 fps cap, then nothing.
        let frames: Vec<Duration> = (0..12).map(|i| Duration::from_micros(i * 8_333)).collect();
        let (stats, units, enc) = drive(frames, ms(300), Pacing::new(30, None), |_, _| {});
        let pts: Vec<Duration> = units.iter().map(|u| u.pts).collect();
        assert!(pts.windows(2).all(|w| w[1] > w[0]), "{pts:?}");
        assert!(pts.windows(2).all(|w| w[1] - w[0] >= Duration::from_micros(24_000)), "spaced by the cap: {pts:?}");
        assert_eq!(enc.calls.last().map(|c| c.0), Some(11), "the newest frame is encoded at the end");
        assert_eq!((stats.frames, stats.encoded, stats.heartbeats), (12, units.len() as u64, 0));
        assert_eq!(stats.encoded + stats.superseded, 12);
        assert!(units[0].keyframe && units[1..].iter().all(|u| !u.keyframe), "only the first is an IDR");
    }

    #[test]
    fn a_static_screen_gets_heartbeats_with_increasing_timestamps() {
        let (stats, units, enc) = drive(vec![ms(0)], ms(1050), Pacing::new(60, Some(ms(250))), |_, _| {});
        // The frame at 0, then heartbeats at 250, 500, 750 and 1000 ms.
        assert_eq!(units.iter().map(|u| u.heartbeat).collect::<Vec<_>>(), vec![false, true, true, true, true]);
        assert_eq!(units.iter().map(|u| u.pts).collect::<Vec<_>>(), vec![ms(0), ms(250), ms(500), ms(750), ms(1000)]);
        assert!(enc.calls.iter().all(|c| c.0 == 0), "the last frame each time");
        assert_eq!(stats.heartbeats, 4);
    }

    #[test]
    fn keyframe_requests_and_bitrates_reach_the_encoder() {
        let frames: Vec<Duration> = (0..10).map(|i| ms(i * 50)).collect();
        let (stats, units, enc) = drive(frames, ms(600), Pacing::new(30, None), |u, c| {
            if u.pts == ms(150) {
                c.request_keyframe();
                c.set_bitrate(1_500_000);
            }
        });
        // The IDR doesn't wait for the next frame (at 200 ms): the last one is re-encoded at the
        // next slot (183 ms), then the frames go on.
        let keys: Vec<(bool, Duration)> = units.iter().map(|u| (u.keyframe, u.pts)).collect();
        let step = crate::pacing::MIN_STEP;
        let want: Vec<(bool, Duration)> = [(true, ms(0)), (false, ms(50)), (false, ms(100)), (false, ms(150)), (true, ms(150) + step)]
            .into_iter()
            .chain((4..10).map(|i| (false, ms(i * 50))))
            .collect();
        assert_eq!(keys, want);
        assert_eq!(stats.keyframes, 2);
        assert_eq!(enc.bitrates, vec![1_500_000]);
    }

    #[test]
    fn a_keyframe_request_on_a_static_screen_re_encodes_the_last_frame_at_once() {
        let mut asked = false;
        let (_, units, _) = drive(vec![ms(0), ms(10)], ms(400), Pacing::new(30, None), |u, c| {
            if !asked && u.pts >= ms(10) {
                asked = true;
                c.request_keyframe();
            }
        });
        let got: Vec<(bool, bool)> = units.iter().map(|u| (u.keyframe, u.heartbeat)).collect();
        // The first frame (an IDR), the second held to the 33 ms slot, then the forced IDR of the
        // same picture one slot later, with no new frame and no heartbeat configured.
        assert_eq!(got, vec![(true, false), (false, false), (true, false)]);
        assert!(units[2].pts > units[1].pts);
    }

    /// Drops the first `drops` pictures it is asked to make an IDR of.
    struct DroppingEncoder {
        drops: usize,
        calls: Vec<bool>,
    }

    impl Encoder<usize> for DroppingEncoder {
        fn encode(&mut self, picture: &usize, pts: Duration, keyframe: bool) -> Result<Vec<AccessUnit>> {
            self.calls.push(keyframe);
            if keyframe && self.drops > 0 {
                self.drops -= 1;
                return Ok(Vec::new());
            }
            Ok(vec![AccessUnit { data: vec![*picture as u8], pts, keyframe, heartbeat: false, encode_time: Duration::ZERO }])
        }

        fn set_bitrate(&mut self, _: u64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_idr_the_encoder_drops_is_asked_for_again_until_one_comes_out() {
        let t0 = Instant::now();
        let now = Rc::new(Cell::new(t0));
        let controls = Rc::new(Controls::default());
        let frames = (0..10).map(|i| ms(i * 40)).collect();
        let mut source = FakeSource { now: now.clone(), t0, frames, next: 0, end: ms(500), newest: None, controls: controls.clone() };
        let mut encoder = DroppingEncoder { drops: 2, calls: Vec::new() };
        let mut units = Vec::new();
        let stats = run_with_clock(&mut source, &mut encoder, Pacing::new(30, None), &controls, &FakeClock(now), |u| {
            units.push(u);
            Ok(Flow::Continue)
        })
        .expect("run");
        assert_eq!(stats.dropped, 2);
        assert_eq!(&encoder.calls[..4], &[true, true, true, false], "the request survives each drop");
        assert!(units[0].keyframe, "the first unit out is the IDR");
        assert_eq!(stats.keyframes, 1, "and it isn't repeated once it came out");
    }

    #[test]
    fn the_sink_can_stop_the_loop() {
        let t0 = Instant::now();
        let now = Rc::new(Cell::new(t0));
        let controls = Rc::new(Controls::default());
        let frames = (0..100).map(|i| ms(i * 40)).collect();
        let mut source = FakeSource { now: now.clone(), t0, frames, next: 0, end: ms(10_000), newest: None, controls: controls.clone() };
        let mut n = 0;
        let stats = run_with_clock(&mut source, &mut FakeEncoder::default(), Pacing::new(30, None), &controls, &FakeClock(now), |_| {
            n += 1;
            Ok(if n == 3 { Flow::Stop } else { Flow::Continue })
        })
        .expect("run");
        assert_eq!(stats.encoded, 3);
        assert!(!controls.stopped(), "the sink's stop is the loop's, not the shared controls'");
    }
}
