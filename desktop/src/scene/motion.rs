//! Eased, interruptible motion between two values on the window's clock: the one transition the
//! 3D view's camera ([`super::camera::Camera`]) and the device's flip and spin
//! ([`super::camera::Turn`]) move by, and what a face-on 2D view (#253) and scripted timelines
//! (#254) reuse.
//!
//! A [`Motion`] is one track: where a value started, where it is going, when it set off, how
//! long it takes and how it eases. [`Motion::at`] reads it at any clock time, so whoever draws
//! (the compositor) and whoever wakes the window (`poll`) agree on it without stepping state per
//! frame, and a headless run lands on the same frame every time.
//!
//! - [`Motion::go_with`] sets off toward a target at a clock time, over a duration, with a chosen
//!   [`Ease`], from wherever the value is then: a new target interrupts a running one without a
//!   jump. A timeline's step is one call, and it moves the same way whenever it is issued.
//! - [`Motion::go`] is `go_with` for interactive changes: it picks the ease itself, leaving at
//!   speed ([`Ease::OutCubic`]) when it interrupts a running motion and easing out of rest
//!   ([`Ease::InOutCubic`]) otherwise.
//! - [`Motion::set`] is direct manipulation (a drag, a pinch): the value is there at once and
//!   nothing runs.
//! - [`Motion::moving`] says whether it is still on its way (what `poll` and a timeline wait on),
//!   and [`Motion::target`] where it will rest.
//!
//! Each property that moves on its own is its own track (the view keeps the camera, the flip and
//! the spin apart), so steps on different tracks overlap freely: a spin issued mid-flip runs on
//! its own clock and leaves the flip's arc, timing and ease alone. Only a step on the same track
//! interrupts.
//!
//! A value only needs [`Lerp`]: a straight blend, and optionally [`Lerp::near`] to pick the
//! equivalent target closest to the start (an angle the short way round).

/// How long a change of view or of the device's transform takes, seconds.
pub const DURATION: f64 = 0.25;

/// A value [`Motion`] can move.
pub trait Lerp: Copy {
    /// `a` at `t` 0, `b` at 1, straight between.
    fn lerp(a: Self, b: Self, t: f32) -> Self;

    /// The value equivalent to `self` that is closest to `from`, for a motion setting off from
    /// `from` (an angle a whole turn nearer). `self` by default.
    fn near(self, _from: Self) -> Self {
        self
    }
}

impl Lerp for f32 {
    fn lerp(a: f32, b: f32, t: f32) -> f32 {
        mix(a, b, t)
    }
}

/// `a` at `t` 0, `b` at 1.
pub fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// How a motion's progress follows time: `ease(0) == 0`, `ease(1) == 1`, monotonic between.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ease {
    Linear,
    /// Slow out of rest and into the target: a motion from a still value.
    #[default]
    InOutCubic,
    /// Fast out, slow in: a motion that interrupts another, so it moves at once instead of
    /// pausing.
    OutCubic,
}

impl Ease {
    /// Progress at `t`, clamped to 0..1.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::InOutCubic if t < 0.5 => 4.0 * t * t * t,
            Ease::InOutCubic => 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0,
            Ease::OutCubic => 1.0 - (1.0 - t).powi(3),
        }
    }
}

/// A value on its way from `from` to `to` (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion<T> {
    from: T,
    to: T,
    /// Clock seconds it set off, and how long it takes.
    start: f64,
    duration: f64,
    ease: Ease,
}

impl<T: Lerp> Motion<T> {
    /// Resting at `v`.
    pub fn still(v: T) -> Motion<T> {
        Motion { from: v, to: v, start: 0.0, duration: 0.0, ease: Ease::Linear }
    }

    /// The value at clock `now`: `from` before it set off, `to` once it is over.
    pub fn at(&self, now: f64) -> T {
        if self.duration <= 0.0 {
            return self.to;
        }
        let t = ((now - self.start) / self.duration) as f32;
        if t >= 1.0 { self.to } else { T::lerp(self.from, self.to, self.ease.apply(t)) }
    }

    /// Where it is going (where it rests when still).
    pub fn target(&self) -> T {
        self.to
    }

    /// Whether it is still on its way at `now`.
    pub fn moving(&self, now: f64) -> bool {
        self.duration > 0.0 && now < self.start + self.duration
    }

    /// Sets off toward `to` at `now`, over `duration` seconds, from wherever it is at `now`. A
    /// motion still running is interrupted there, with no jump, and the new one leaves at speed
    /// ([`Ease::OutCubic`]); one from rest eases out of it ([`Ease::InOutCubic`]).
    pub fn go(&mut self, to: T, now: f64, duration: f64) {
        let ease = if self.moving(now) { Ease::OutCubic } else { Ease::InOutCubic };
        self.go_with(to, now, duration, ease);
    }

    /// Sets off toward `to` at `now`, over `duration` seconds, eased by `ease`, from wherever it
    /// is at `now` (interrupting a running motion there, with no jump). The same call moves the
    /// same way whenever it is issued: a scripted step's timing and easing are its own.
    pub fn go_with(&mut self, to: T, now: f64, duration: f64, ease: Ease) {
        let from = self.at(now);
        *self = Motion { from, to: to.near(from), start: now, duration: duration.max(0.0), ease };
    }

    /// Is `v` now, with nothing running: direct manipulation.
    pub fn set(&mut self, v: T) {
        *self = Motion::still(v);
    }
}

impl<T: Lerp + Default> Default for Motion<T> {
    fn default() -> Motion<T> {
        Motion::still(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eases_start_at_zero_end_at_one_and_never_go_back() {
        for ease in [Ease::Linear, Ease::InOutCubic, Ease::OutCubic] {
            assert_eq!(ease.apply(0.0), 0.0, "{ease:?}");
            assert_eq!(ease.apply(1.0), 1.0, "{ease:?}");
            assert_eq!(ease.apply(-1.0), 0.0, "{ease:?} clamps before the start");
            assert_eq!(ease.apply(2.0), 1.0, "{ease:?} clamps after the end");
            let mut last = 0.0;
            for i in 1..=100 {
                let v = ease.apply(i as f32 / 100.0);
                assert!(v >= last, "{ease:?} at {i}");
                last = v;
            }
        }
        // In-out is symmetric and halfway at half time; out is ahead of linear.
        assert!((Ease::InOutCubic.apply(0.5) - 0.5).abs() < 1e-6);
        assert!((Ease::InOutCubic.apply(0.25) + Ease::InOutCubic.apply(0.75) - 1.0).abs() < 1e-6);
        assert!(Ease::OutCubic.apply(0.25) > 0.25);
    }

    #[test]
    fn a_motion_runs_from_its_start_to_its_target_and_then_rests() {
        let mut m = Motion::still(0.0f32);
        assert!(!m.moving(0.0));
        m.go(10.0, 1.0, 0.25);
        assert_eq!(m.at(1.0), 0.0, "it sets off from where it was");
        assert_eq!(m.at(0.5), 0.0, "before it set off");
        assert!(m.moving(1.1));
        assert!((m.at(1.125) - 5.0).abs() < 1e-4, "in-out is halfway at half time");
        assert_eq!(m.at(1.25), 10.0);
        assert!(!m.moving(1.25), "over at its end");
        assert_eq!(m.at(9.0), 10.0);
        assert_eq!(m.target(), 10.0);
    }

    #[test]
    fn a_new_target_interrupts_without_a_jump_and_moves_at_once() {
        let mut m = Motion::still(0.0f32);
        m.go(10.0, 0.0, 0.25);
        let mid = m.at(0.1);
        m.go(-10.0, 0.1, 0.25);
        assert_eq!(m.at(0.1), mid, "no jump where it was interrupted");
        // Out-cubic leaves at speed: a frame later it has visibly turned toward the new target.
        assert!(m.at(0.1 + 1.0 / 60.0) < mid - 0.5);
        assert_eq!(m.at(0.35), -10.0);
        // Set is direct: there at once, nothing running.
        m.set(3.0);
        assert_eq!(m.at(0.2), 3.0);
        assert!(!m.moving(0.2));
    }

    #[test]
    fn go_with_eases_as_asked_wherever_it_is_issued() {
        // A step issued as the last one ends and one issued a moment earlier ease the same way.
        let mut at_end = Motion::still(0.0f32);
        at_end.go(10.0, 0.0, 0.25);
        at_end.go_with(20.0, 0.25, 1.0, Ease::Linear);
        let mut early = Motion::still(0.0f32);
        early.go(10.0, 0.0, 0.25);
        early.go_with(20.0, 0.249, 1.0, Ease::Linear);
        assert!((at_end.at(0.75) - 15.0).abs() < 1e-4, "linear: halfway at half time");
        assert!((early.at(0.749) - at_end.at(0.75)).abs() < 0.05, "{} {}", early.at(0.749), at_end.at(0.75));
        for ease in [Ease::Linear, Ease::InOutCubic, Ease::OutCubic] {
            let mut m = Motion::still(0.0f32);
            m.go_with(1.0, 2.0, 0.5, ease);
            assert!((m.at(2.1) - ease.apply(0.2)).abs() < 1e-6, "{ease:?}");
            assert_eq!(m.at(2.5), 1.0);
        }
    }

    #[test]
    fn separate_tracks_overlap_without_retiming_each_other() {
        let (mut a, mut b) = (Motion::still(0.0f32), Motion::still(0.0f32));
        a.go_with(1.0, 0.0, 1.0, Ease::Linear);
        let alone = a;
        b.go_with(5.0, 0.4, 0.2, Ease::OutCubic);
        assert_eq!(a, alone, "a step on another track leaves this one alone");
        assert!((a.at(0.5) - 0.5).abs() < 1e-6 && b.moving(0.5) && !b.moving(0.61) && a.moving(0.61));
    }

    #[test]
    fn zero_duration_lands_at_once() {
        let mut m = Motion::still(1.0f32);
        m.go(2.0, 5.0, 0.0);
        assert_eq!(m.at(5.0), 2.0);
        assert!(!m.moving(5.0));
    }
}
