//! When to encode. The source says when the device presented a frame (the iOS framebuffer's
//! frame callback, emu's frame announcements); the [`Pacer`] encodes the newest one no sooner
//! than one frame interval after the previous encode (an fps cap that never loses the last frame:
//! one held back is encoded when its turn comes), and on a static screen re-encodes the last
//! frame every heartbeat so a viewer that joins, or a server that times out idle tracks, still
//! gets pictures. [`PtsClock`] turns capture instants into strictly increasing presentation times.
//!
//! Both are pure functions of the instants they're given, so tests drive them with made-up times.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pacing {
    /// Encodes at most this often (1 / the fps cap).
    pub min_interval: Duration,
    /// Re-encode the last frame after this long without a new one; `None`: never.
    pub heartbeat: Option<Duration>,
}

impl Pacing {
    pub fn new(fps: u32, heartbeat: Option<Duration>) -> Pacing {
        Pacing { min_interval: Duration::from_secs(1) / fps.max(1), heartbeat }
    }
}

/// What to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Encode the newest frame now. `captured` is when it was captured (for a heartbeat, now).
    Encode { captured: Instant, heartbeat: bool },
    /// Nothing is due before this instant (`None`: until a frame arrives).
    Wait(Option<Instant>),
}

#[derive(Clone, Debug)]
pub struct Pacer {
    pacing: Pacing,
    last_encode: Option<Instant>,
    pending: Option<Instant>,
    /// Frames replaced by a newer one before their turn came.
    pub superseded: u64,
}

impl Pacer {
    pub fn new(pacing: Pacing) -> Pacer {
        Pacer { pacing, last_encode: None, pending: None, superseded: 0 }
    }

    /// The source captured a frame at `at`. A newer frame replaces one still waiting.
    pub fn frame(&mut self, at: Instant) {
        if self.pending.replace(at).is_some() {
            self.superseded += 1;
        }
    }

    /// Re-encode the current frame at its next turn even though nothing new arrived (a keyframe
    /// was asked for on a static screen). Does nothing when a frame is already waiting.
    pub fn refresh(&mut self, now: Instant) {
        self.pending.get_or_insert(now);
    }

    pub fn poll(&self, now: Instant) -> Action {
        let next_slot = self.last_encode.map(|l| l + self.pacing.min_interval);
        if let Some(captured) = self.pending {
            return match next_slot {
                Some(due) if now < due => Action::Wait(Some(due)),
                _ => Action::Encode { captured, heartbeat: false },
            };
        }
        match (self.last_encode, self.pacing.heartbeat) {
            (Some(last), Some(beat)) => {
                let due = last + beat.max(self.pacing.min_interval);
                if now >= due { Action::Encode { captured: now, heartbeat: true } } else { Action::Wait(Some(due)) }
            }
            _ => Action::Wait(None),
        }
    }

    /// An encode decided at `at` happened (or was dropped by the encoder): the next slot counts
    /// from it, and the frame it took is no longer pending.
    pub fn encoded(&mut self, at: Instant) {
        self.last_encode = Some(at);
        self.pending = None;
    }
}

/// Presentation times from capture instants: the time since `origin`, kept strictly increasing by
/// at least one 90 kHz tick (the RTP clock), so two pictures never share a timestamp even when a
/// heartbeat lands right after a frame or the source's clock jitters backwards.
#[derive(Clone, Debug)]
pub struct PtsClock {
    origin: Instant,
    last: Option<Duration>,
}

/// One tick of the 90 kHz RTP clock, rounded up.
pub const MIN_STEP: Duration = Duration::from_nanos(11_112);

impl PtsClock {
    pub fn new(origin: Instant) -> PtsClock {
        PtsClock { origin, last: None }
    }

    pub fn stamp(&mut self, captured: Instant) -> Duration {
        let raw = captured.saturating_duration_since(self.origin);
        let pts = match self.last {
            Some(last) if raw < last + MIN_STEP => last + MIN_STEP,
            _ => raw,
        };
        self.last = Some(pts);
        pts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn caps_the_rate_and_encodes_the_newest_frame_when_its_turn_comes() {
        let t0 = Instant::now();
        let mut p = Pacer::new(Pacing::new(50, None)); // one every 20 ms
        assert_eq!(p.poll(t0), Action::Wait(None), "nothing yet");
        p.frame(t0);
        assert_eq!(p.poll(t0), Action::Encode { captured: t0, heartbeat: false }, "the first frame goes at once");
        p.encoded(t0);
        // Frames at 5 and 10 ms: held back until 20 ms, then the newest one goes.
        p.frame(t0 + ms(5));
        assert_eq!(p.poll(t0 + ms(5)), Action::Wait(Some(t0 + ms(20))));
        p.frame(t0 + ms(10));
        assert_eq!(p.superseded, 1);
        assert_eq!(p.poll(t0 + ms(20)), Action::Encode { captured: t0 + ms(10), heartbeat: false });
        p.encoded(t0 + ms(20));
        assert_eq!(p.poll(t0 + ms(25)), Action::Wait(None), "no heartbeat configured");
        // A frame long after the last encode goes at once.
        p.frame(t0 + ms(100));
        assert_eq!(p.poll(t0 + ms(100)), Action::Encode { captured: t0 + ms(100), heartbeat: false });
    }

    #[test]
    fn a_static_screen_gets_a_heartbeat() {
        let t0 = Instant::now();
        let mut p = Pacer::new(Pacing::new(30, Some(ms(500))));
        assert_eq!(p.poll(t0 + ms(900)), Action::Wait(None), "no heartbeat before the first frame");
        p.frame(t0);
        p.encoded(t0);
        assert_eq!(p.poll(t0 + ms(499)), Action::Wait(Some(t0 + ms(500))));
        assert_eq!(p.poll(t0 + ms(500)), Action::Encode { captured: t0 + ms(500), heartbeat: true });
        p.encoded(t0 + ms(500));
        // A real frame resets the heartbeat's clock.
        p.frame(t0 + ms(700));
        p.encoded(t0 + ms(700));
        assert_eq!(p.poll(t0 + ms(1100)), Action::Wait(Some(t0 + ms(1200))));
        // A refresh (keyframe asked for) re-encodes at the next slot, not before.
        p.refresh(t0 + ms(710));
        assert_eq!(p.poll(t0 + ms(710)), Action::Wait(Some(t0 + ms(700) + Duration::from_secs(1) / 30)));
        assert_eq!(p.poll(t0 + ms(740)), Action::Encode { captured: t0 + ms(710), heartbeat: false });
        // A heartbeat shorter than the frame interval waits for the interval.
        let mut fast = Pacer::new(Pacing::new(10, Some(ms(1))));
        fast.frame(t0);
        fast.encoded(t0);
        assert_eq!(fast.poll(t0 + ms(50)), Action::Wait(Some(t0 + ms(100))));
    }

    #[test]
    fn presentation_times_strictly_increase() {
        let t0 = Instant::now();
        let mut c = PtsClock::new(t0);
        assert_eq!(c.stamp(t0 + ms(10)), ms(10));
        assert_eq!(c.stamp(t0 + ms(10)), ms(10) + MIN_STEP, "the same instant twice");
        assert_eq!(c.stamp(t0 + ms(5)), ms(10) + MIN_STEP * 2, "a capture from before the last one");
        assert_eq!(c.stamp(t0 + ms(40)), ms(40));
        // Before the origin counts as the origin.
        let mut late = PtsClock::new(t0 + ms(100));
        assert_eq!(late.stamp(t0), Duration::ZERO);
        let mut last = None;
        for i in [3u64, 3, 1, 7, 7, 7, 2, 9] {
            let pts = c.stamp(t0 + ms(40 + i));
            assert!(last.is_none_or(|l| pts > l), "{pts:?} after {last:?}");
            last = Some(pts);
        }
    }
}
