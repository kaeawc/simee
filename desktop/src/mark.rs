//! The simee mark in the window, drawn live: [`sim_core::mark`] turns, projects and lights the cut
//! emerald for a motion state and a time, and each visible facet becomes one Rune fill. It is the
//! activity indicator beside the toolbar's "Booting…", each device's state in the list, and the
//! empty stage's picture. Change the stone in `sim_core::mark` and the window follows.

use std::sync::LazyLock;
use std::time::Duration;

use rune::color::srgb_decode;
use rune::{Color, El, LineCap, Shape, canvas};
use sim_core::device::DeviceState;
use sim_core::mark::{Cut, Motion, Solid, Style, Tone, render};
use sim_core::platform::Platform;

static STONE: LazyLock<Solid> = LazyLock::new(|| Solid::new(&Cut::SIMEE));

/// How often a moving mark is redrawn: 30 frames a second is smooth at these sizes, and the
/// window asks for none while nothing moves.
pub const FRAME: Duration = Duration::from_millis(33);

/// A time at which every state is at rest (past the idle glint, before the next).
const REST: f32 = 2.5;

/// The mark's facets as fills in a 2×2 box. Each facet is also outlined in its own colour, half a
/// point wide, so two antialiased neighbours leave no hairline of the background between them.
pub fn shape(motion: Motion, t: f32, tone: Option<Tone>) -> Shape {
    let style = Style::MARK;
    let mut pose = motion.pose(t, STONE.sides, &style);
    if let Some(tone) = tone
        && pose.tone == Tone::Emerald
    {
        pose.tone = tone;
    }
    let mut shape = Shape::new().view_box(2.0, 2.0);
    for f in render(&STONE, &pose, &style) {
        let points: Vec<[f32; 2]> = f.points.iter().map(|p| [p[0] + 1.0, p[1] + 1.0]).collect();
        let c = Color {
            r: srgb_decode(f32::from(f.rgba[0]) / 255.0),
            g: srgb_decode(f32::from(f.rgba[1]) / 255.0),
            b: srgb_decode(f32::from(f.rgba[2]) / 255.0),
            a: f32::from(f.rgba[3]) / 255.0,
        };
        shape = shape.fill(points.clone(), c).stroke_with(points, true, 0.5, LineCap::Round, c);
    }
    shape
}

/// The mark `size` points square, `t` seconds into `motion`; `None` draws it at rest (the
/// system's reduced motion, or a state that doesn't move).
pub fn mark(motion: Motion, t: Option<f32>, size: f32) -> El {
    toned(motion, t, None, size)
}

/// [`mark`] in a platform's tone where the state has none of its own.
pub fn toned(motion: Motion, t: Option<f32>, tone: Option<Tone>, size: f32) -> El {
    let t = t.filter(|_| motion.moves()).unwrap_or(REST);
    canvas(shape(motion, t, tone)).size(size, size)
}

/// What a device in the list shows: resting when booted, turning while it starts or stops, smoke
/// when off.
pub fn for_device(state: DeviceState) -> Motion {
    match state {
        DeviceState::Booted => Motion::Idle,
        DeviceState::Booting | DeviceState::ShuttingDown | DeviceState::Creating => Motion::Working,
        _ => Motion::Offline,
    }
}

/// Whether a device's mark moves (so the window keeps drawing).
pub fn moves(state: DeviceState) -> bool {
    for_device(state) == Motion::Working
}

/// Emerald for Android, sapphire for Apple's platforms.
pub fn tone(platform: Platform) -> Tone {
    if platform == Platform::Android { Tone::Emerald } else { Tone::Sapphire }
}

/// What the toolbar shows beside a background job's label.
pub fn for_busy(label: &str) -> Motion {
    if label.starts_with("Boot") { Motion::Boot } else { Motion::Working }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_is_one_fill_and_one_outline_per_visible_facet() {
        let s = shape(Motion::Idle, REST, None);
        assert_eq!(s.view_box, Some([2.0, 2.0]));
        assert!(s.ops.len() >= 40 && s.ops.len().is_multiple_of(2), "{}", s.ops.len());
    }

    #[test]
    fn platforms_tint_the_resting_stone_but_not_an_error() {
        let a = shape(Motion::Idle, REST, Some(Tone::Sapphire));
        let b = shape(Motion::Idle, REST, None);
        assert_ne!(a, b);
        assert_eq!(shape(Motion::Error, 1.0, Some(Tone::Sapphire)), shape(Motion::Error, 1.0, None));
    }

    #[test]
    fn devices_starting_or_stopping_turn_and_the_rest_hold_still() {
        assert!(moves(DeviceState::Booting) && moves(DeviceState::ShuttingDown));
        assert!(!moves(DeviceState::Booted) && !moves(DeviceState::Shutdown));
        assert_eq!(for_busy("Booting…"), Motion::Boot);
        assert_eq!(for_busy("Folding…"), Motion::Working);
    }
}
