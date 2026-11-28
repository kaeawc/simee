//! What the stone does while simee works. Each state is a pure function of time, so any frame can
//! be drawn on its own (a test, a headless snapshot, a window that slept). The shape never
//! changes and the stone is never given a face: speed, colour, position and roll carry the meaning.

use std::f32::consts::{PI, TAU};

use super::render::{Pose, Style, Tone};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Motion {
    /// Ready, nothing running: still, with a glint crossing every five seconds.
    Idle,
    /// A command is running: a steady turn.
    Working,
    /// Long work (provisioning an image, building the shaper): the turn swells and settles.
    Heavy,
    /// One automation step: exactly one facet's turn, then a stop.
    Step,
    /// Reading the screen or view hierarchy: the stone holds, the light sweeps across it.
    Inspect,
    /// A download or install this far done (0 to 1): fills with emerald from the tip up.
    Progress(f32),
    /// Starting or stopping a device: the pyramids meet, turn, and part.
    Boot,
    /// Finished: two hops, a flash, one full turn.
    Done,
    /// Waiting on something outside simee (adb, boot_completed): floats and turns slowly.
    Waiting,
    /// A dialog needs the user: a short roll back and forth.
    Attention,
    /// Citrine, a slow turn and a soft pulse.
    Warning,
    /// Ruby, and a hard shake that dies out.
    Error,
    /// The device is off: smoke grey and still.
    Offline,
    /// The device turned: a quarter roll per change.
    Rotate,
    /// Moving between platforms: emerald (Android) and sapphire (iOS) in turn.
    Platform,
}

fn ease(t: f32) -> f32 {
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

impl Motion {
    /// Every state, for galleries and tests (progress shown half done).
    pub const ALL: [Motion; 15] = [
        Motion::Idle,
        Motion::Working,
        Motion::Heavy,
        Motion::Step,
        Motion::Inspect,
        Motion::Progress(0.5),
        Motion::Boot,
        Motion::Done,
        Motion::Waiting,
        Motion::Attention,
        Motion::Warning,
        Motion::Error,
        Motion::Offline,
        Motion::Rotate,
        Motion::Platform,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Motion::Idle => "idle",
            Motion::Working => "working",
            Motion::Heavy => "heavy",
            Motion::Step => "step",
            Motion::Inspect => "inspect",
            Motion::Progress(_) => "progress",
            Motion::Boot => "boot",
            Motion::Done => "done",
            Motion::Waiting => "waiting",
            Motion::Attention => "attention",
            Motion::Warning => "warning",
            Motion::Error => "error",
            Motion::Offline => "offline",
            Motion::Rotate => "rotate",
            Motion::Platform => "platform",
        }
    }

    /// Seconds before the state repeats (or, for a steady turn, before it has turned all the
    /// way round): a contact sheet samples one period.
    pub fn period(self) -> f32 {
        match self {
            Motion::Idle | Motion::Progress(_) => 5.0,
            Motion::Working | Motion::Inspect => TAU / 1.5,
            Motion::Heavy => TAU / 1.1,
            Motion::Step => 0.9,
            Motion::Boot => 4.0,
            Motion::Done => 2.6,
            Motion::Waiting => TAU / 2.4,
            Motion::Attention => 2.2,
            Motion::Warning => TAU / 3.0,
            Motion::Error => 2.4,
            Motion::Offline => 1.0,
            Motion::Rotate => 7.2,
            Motion::Platform => 6.0,
        }
    }

    /// Whether drawing it needs more than one frame.
    pub fn moves(self) -> bool {
        self != Motion::Offline
    }

    /// The pose at `t` seconds into the state, for a stone with `sides` sides drawn in `style`.
    pub fn pose(self, t: f32, sides: u32, style: &Style) -> Pose {
        let mut p = Pose::rest(style);
        let rest = Pose::REST_YAW;
        let step = TAU / sides.max(3) as f32;
        let t = t.max(0.0);
        match self {
            Motion::Idle => {
                let c = t % 5.0;
                if c < 1.1 {
                    p.light[0] = -0.55 + 1.15 * (PI * ease(c / 1.1)).sin();
                }
            }
            Motion::Working => p.yaw = rest + 1.5 * t,
            // Speed 1.9 + 1.6·cos(1.1t) rad/s: from 0.3 to 3.5 and back every 5.7 s.
            Motion::Heavy => p.yaw = rest + 1.9 * t + 1.6 / 1.1 * (1.1 * t).sin(),
            Motion::Step => {
                let (k, f) = ((t / 0.9).floor(), (t % 0.9) / 0.9);
                p.yaw = rest + (k + ease((f / 0.5).min(1.0))) * step;
            }
            Motion::Inspect => {
                let a = (t * 1.5).sin() * 1.1;
                p.light = [a.sin(), 0.55, a.cos() * 0.9];
            }
            Motion::Progress(f) => {
                p.fill = Some(f.clamp(0.0, 1.0));
                p.yaw = rest + 0.45 * t;
            }
            Motion::Boot => {
                // Each 4 s: meet (1.1 s), spin down (2.1 s, 2.24 rad in all), part (0.8 s).
                let (cycle, c) = ((t / 4.0).floor(), t % 4.0);
                let spun = |k: f32| 2.24 * (1.0 - (1.0 - k).powi(3));
                p.yaw = rest + cycle * 2.24;
                if c < 1.1 {
                    let e = 1.0 - ease_out(c / 1.1);
                    (p.split, p.split_turn) = (0.16 * e, 2.2 * e);
                } else if c < 3.2 {
                    p.yaw += spun((c - 1.1) / 2.1);
                    if c < 1.4 {
                        p.flash = 0.6 * (1.0 - (c - 1.1) / 0.3);
                    }
                } else {
                    let e = ease((c - 3.2) / 0.8);
                    p.yaw += 2.24;
                    (p.split, p.split_turn) = (0.16 * e, 2.2 * e);
                }
            }
            Motion::Done => {
                let c = t % 2.6;
                if c < 0.7 {
                    p.offset[1] = -0.2 * (PI * c / 0.35).sin().abs() * (1.0 - c / 0.7);
                }
                p.flash = (1.0 - c / 0.5).max(0.0) * 0.6;
                p.yaw = rest + ease_out((c / 0.9).min(1.0)) * TAU;
            }
            Motion::Waiting => {
                p.offset[1] = (t * 2.4).sin() * 0.07;
                p.yaw = rest + 0.45 * t;
            }
            Motion::Attention => {
                let c = t % 2.2;
                if c < 0.9 {
                    p.roll = 0.2 * (c * 26.0).sin() * (1.0 - c / 0.9);
                }
            }
            Motion::Warning => {
                p.tone = Tone::Citrine;
                p.yaw = rest + 0.6 * t;
                p.flash = 0.16 * (0.5 + 0.5 * (t * 3.0).sin()).powi(4);
            }
            Motion::Error => {
                let c = t % 2.4;
                p.tone = Tone::Ruby;
                if c < 0.8 {
                    p.offset[0] = 0.12 * (c * 44.0).sin() * (1.0 - c / 0.8).powf(1.5);
                }
                p.flash = (0.5 * (1.0 - c / 0.35)).max(0.0);
            }
            Motion::Offline => {
                p.tone = Tone::Smoke;
                p.pitch *= 0.6;
            }
            Motion::Rotate => {
                let (k, f) = ((t / 1.8).floor(), (t % 1.8) / 1.8);
                p.roll = -(k + ease((f / 0.4).min(1.0))) * PI / 2.0;
            }
            Motion::Platform => {
                p.yaw = rest + 0.7 * t;
                let c = t % 6.0;
                let k = match c {
                    c if c < 2.5 => 0.0,
                    c if c < 3.0 => ease((c - 2.5) / 0.5),
                    c if c < 5.5 => 1.0,
                    c => 1.0 - ease((c - 5.5) / 0.5),
                };
                p.blend = Some((Tone::Sapphire, k));
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLE: Style = Style::MARK;

    #[test]
    fn every_state_starts_near_rest() {
        for m in Motion::ALL {
            let p = m.pose(0.0, 10, &STYLE);
            assert!(p.offset[0].abs() < 1e-6 && p.offset[1].abs() < 1e-6, "{}", m.name());
            assert!(p.roll.abs() < 1e-6, "{}", m.name());
        }
    }

    #[test]
    fn looping_states_join_up() {
        // The pose a moment before a loop ends matches the one a moment after it starts.
        for (m, period) in [(Motion::Done, 2.6), (Motion::Error, 2.4), (Motion::Attention, 2.2), (Motion::Platform, 6.0)] {
            let (a, b) = (m.pose(period - 1e-3, 10, &STYLE), m.pose(period + 1e-3, 10, &STYLE));
            assert!((a.offset[0] - b.offset[0]).abs() < 0.02 && (a.offset[1] - b.offset[1]).abs() < 0.02, "{}", m.name());
            assert!((a.roll - b.roll).abs() < 0.02, "{}", m.name());
        }
        let (a, b) = (Motion::Boot.pose(4.0 - 1e-3, 10, &STYLE), Motion::Boot.pose(4.0 + 1e-3, 10, &STYLE));
        assert!((a.split - b.split).abs() < 0.01 && (a.split_turn - b.split_turn).abs() < 0.05);
        assert!((a.yaw - b.yaw).abs() < 0.05, "the stone carries its turn into the next cycle");
    }

    #[test]
    fn a_step_turns_exactly_one_facet() {
        let step = TAU / 10.0;
        let a = Motion::Step.pose(0.0, 10, &STYLE).yaw;
        let b = Motion::Step.pose(0.9 * 0.75, 10, &STYLE).yaw;
        assert!((b - a - step).abs() < 1e-4);
    }

    #[test]
    fn heavy_work_never_turns_backwards() {
        let mut last = Motion::Heavy.pose(0.0, 10, &STYLE).yaw;
        for i in 1..600 {
            let y = Motion::Heavy.pose(i as f32 / 60.0, 10, &STYLE).yaw;
            assert!(y > last);
            last = y;
        }
    }

    #[test]
    fn states_carry_their_colours() {
        assert_eq!(Motion::Error.pose(1.0, 10, &STYLE).tone, Tone::Ruby);
        assert_eq!(Motion::Warning.pose(1.0, 10, &STYLE).tone, Tone::Citrine);
        assert_eq!(Motion::Offline.pose(1.0, 10, &STYLE).tone, Tone::Smoke);
        assert_eq!(Motion::Platform.pose(4.0, 10, &STYLE).blend, Some((Tone::Sapphire, 1.0)));
        assert_eq!(Motion::Working.pose(1.0, 10, &STYLE).tone, Tone::Emerald);
    }

    #[test]
    fn offline_holds_still_and_the_rest_move() {
        assert!(!Motion::Offline.moves());
        assert_eq!(Motion::Offline.pose(0.0, 10, &STYLE), Motion::Offline.pose(3.0, 10, &STYLE));
        assert!(Motion::ALL.iter().filter(|m| m.moves()).all(|m| m.pose(0.3, 10, &STYLE) != m.pose(1.7, 10, &STYLE)));
    }
}
