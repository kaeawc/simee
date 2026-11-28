//! Waiting for a still screen (`wait_idle`, #114): returns once no frame has changed the picture
//! for a quiet period, or fails at a timeout.
//!
//! The frame clock is the surface's seed, read when the device presents a frame (the damage
//! callback) or on a short poll. On iOS 27.2 the seed moves only with a new picture (60-65 times a
//! second under a swipe, never on a still screen), so a still seed is a still screen and nothing
//! is read but the seed. On iOS 26.5 the seed and the callback tick 10 times a second whether or
//! not anything changed, so a moved seed alone proves nothing: the contents are hashed (at most
//! every [`HASH_EVERY`]) and only a new hash counts as a change, dated at the seed change before
//! it. A still screen there is still within one tick (100 ms) of going still.

use std::time::{Duration, Instant};

/// The least time between two content hashes while the seed keeps moving: an animation on a
/// runtime with an honest seed costs a hash this often, not one a frame.
pub const HASH_EVERY: Duration = Duration::from_millis(50);

/// A display as the wait reads it.
pub trait Probe {
    /// The surface's seed now (cheap: no pixels).
    fn seed(&mut self) -> Result<u32, String>;
    /// The contents' hash and the seed they were read at (one lock).
    fn hash(&mut self) -> Result<(u32, u64), String>;
    /// Blocks until the device presents a frame or `at_most` passes.
    fn wait(&mut self, at_most: Duration);
    /// The time now.
    fn now(&self) -> Instant;
    /// Why the wait should end now, if it should: the call was cancelled, or the device went
    /// away (its frozen surface would read as a still screen).
    fn stop(&mut self) -> Option<String>;
}

/// A wait that ended on a still screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Still {
    /// From the start of the wait until it saw the screen still for the quiet period.
    pub waited: Duration,
    /// Seed changes seen.
    pub frames: u64,
    /// Content hashes taken (the first is the baseline).
    pub hashes: u64,
}

/// Waits until the screen has been still for `quiet`, at most `timeout`; fails, saying so, when
/// it kept changing.
pub fn wait_idle(probe: &mut impl Probe, quiet: Duration, timeout: Duration, hash_every: Duration) -> Result<Still, String> {
    let start = probe.now();
    let (mut seed, mut hash) = probe.hash()?;
    let (mut frames, mut hashes) = (0u64, 1u64);
    // When the picture last changed (as far as is known), when the seed last moved, and whether it
    // moved since the last hash.
    let (mut still_since, mut moved_at, mut dirty, mut hashed_at) = (start, start, false, start);
    loop {
        let now = probe.now();
        if dirty && now.duration_since(hashed_at) >= hash_every {
            let (at, h) = probe.hash()?;
            (hashes, hashed_at, dirty) = (hashes + 1, now, false);
            if at != seed {
                // It moved again after the last read: the hashed picture is this late.
                (seed, moved_at, frames) = (at, now, frames + 1);
            }
            if h != hash {
                (hash, still_since) = (h, moved_at);
            }
        }
        if !dirty && now.duration_since(still_since) >= quiet {
            return Ok(Still { waited: now.duration_since(start), frames, hashes });
        }
        if now.duration_since(start) >= timeout {
            return Err(format!(
                "the screen kept changing: not still for {} ms within {} ms ({frames} frames, still for the last {} ms)",
                quiet.as_millis(),
                timeout.as_millis(),
                now.duration_since(still_since).as_millis()
            ));
        }
        // Look again when the next hash is due (the seed moved), else when the quiet period
        // would end, unless a frame comes first.
        let until = if dirty { hashed_at + hash_every } else { still_since + quiet }.min(start + timeout);
        probe.wait(until.saturating_duration_since(now));
        if let Some(why) = probe.stop() {
            return Err(why);
        }
        let at = probe.seed()?;
        if at != seed {
            (seed, moved_at, dirty, frames) = (at, probe.now(), true, frames + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_millis(300);
    const TIMEOUT: Duration = Duration::from_secs(2);
    const FRAME: Duration = Duration::from_micros(16_667);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A device on a fake clock: each event moves the seed at its time, and changes the picture
    /// when its flag is set.
    struct Fake {
        start: Instant,
        at: Duration,
        events: Vec<(Duration, bool)>,
        seed_reads: u64,
        /// The time (from the start) from which `stop` says why.
        stop_at: Option<Duration>,
    }

    impl Fake {
        fn new(events: Vec<(Duration, bool)>) -> Self {
            Self { start: Instant::now(), at: Duration::ZERO, events, seed_reads: 0, stop_at: None }
        }

        /// Frames every `every` from `from` until `until`, changing the picture or not.
        fn frames(from: Duration, until: Duration, every: Duration, changes: bool) -> Vec<(Duration, bool)> {
            std::iter::successors(Some(from), |t| Some(*t + every)).take_while(|t| *t < until).map(|t| (t, changes)).collect()
        }

        fn past(&self) -> impl Iterator<Item = &(Duration, bool)> {
            self.events.iter().filter(|(t, _)| *t <= self.at)
        }
    }

    impl Probe for Fake {
        fn seed(&mut self) -> Result<u32, String> {
            self.seed_reads += 1;
            Ok(self.past().count() as u32)
        }
        fn hash(&mut self) -> Result<(u32, u64), String> {
            Ok((self.past().count() as u32, self.past().filter(|(_, c)| *c).count() as u64))
        }
        fn wait(&mut self, at_most: Duration) {
            let next = self.events.iter().map(|(t, _)| *t).filter(|t| *t > self.at).min();
            self.at = next.filter(|t| *t <= self.at + at_most).unwrap_or(self.at + at_most);
        }
        fn now(&self) -> Instant {
            self.start + self.at
        }
        fn stop(&mut self) -> Option<String> {
            self.stop_at.filter(|t| self.at >= *t).map(|_| "cancelled".to_owned())
        }
    }

    fn wait(fake: &mut Fake) -> Result<Still, String> {
        wait_idle(fake, QUIET, TIMEOUT, HASH_EVERY)
    }

    #[test]
    fn a_still_screen_is_idle_after_the_quiet_period_reading_only_the_seed() {
        let mut fake = Fake::new(Vec::new());
        let still = wait(&mut fake).expect("idle");
        assert_eq!(still, Still { waited: QUIET, frames: 0, hashes: 1 });
        assert!(fake.seed_reads <= 2, "no polling while nothing is presented: {}", fake.seed_reads);
    }

    #[test]
    fn an_animation_is_idle_within_a_frame_of_its_last_frame_plus_quiet() {
        let mut fake = Fake::new(Fake::frames(ms(0), ms(500), FRAME, true));
        let last = fake.events.last().expect("frames").0;
        let still = wait(&mut fake).expect("idle");
        assert!(still.waited >= last + QUIET && still.waited <= last + QUIET + FRAME, "{still:?} after a last frame at {last:?}");
        assert!(still.hashes <= 1 + 500 / 50 + 1, "hashed at most every 50 ms, not every frame: {}", still.hashes);
    }

    #[test]
    fn a_screen_that_keeps_changing_times_out() {
        let mut fake = Fake::new(Fake::frames(ms(0), ms(10_000), FRAME, true));
        let e = wait(&mut fake).expect_err("busy");
        assert!(e.contains("kept changing"), "{e}");
        assert_eq!(fake.now() - fake.start, TIMEOUT, "gives up at the timeout, not before or long after");
    }

    #[test]
    fn a_seed_ticking_on_a_still_screen_is_idle_by_its_hash() {
        // iOS 26.5: the seed and the callback tick 10 times a second, moving or not.
        let mut fake = Fake::new(Fake::frames(ms(100), ms(10_000), ms(100), false));
        let still = wait(&mut fake).expect("idle");
        assert!(still.waited >= QUIET && still.waited <= QUIET + ms(100), "{still:?}");
    }

    #[test]
    fn a_ticking_seed_is_idle_within_a_tick_of_the_picture_going_still() {
        let mut events = Fake::frames(ms(100), ms(10_000), ms(100), false);
        events.iter_mut().filter(|(t, _)| *t <= ms(400)).for_each(|(_, c)| *c = true);
        let mut fake = Fake::new(events);
        let still = wait(&mut fake).expect("idle");
        assert!(still.waited >= ms(400) + QUIET && still.waited <= ms(400) + QUIET + ms(100), "{still:?}");
    }

    #[test]
    fn a_frame_with_no_change_after_a_change_does_not_hide_it() {
        // A change at 250 ms, then seed-only ticks: still since 250 ms, not since the last tick.
        let mut fake = Fake::new(vec![(ms(250), true), (ms(260), false), (ms(270), false)]);
        let still = wait(&mut fake).expect("idle");
        assert_eq!(still.waited, ms(250) + QUIET, "still since the change at 250 ms, not the last tick at 270 ms: {still:?}");
    }

    #[test]
    fn a_cancelled_or_stopped_wait_ends_with_why_not_at_the_timeout() {
        let mut fake = Fake::new(Fake::frames(ms(0), ms(10_000), FRAME, true));
        fake.stop_at = Some(ms(100));
        let e = wait(&mut fake).expect_err("stopped");
        assert_eq!(e, "cancelled");
        assert!(fake.now() - fake.start < ms(200), "stops at the first look after the signal, not at the timeout");
    }
}
