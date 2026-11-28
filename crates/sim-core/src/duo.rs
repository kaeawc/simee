//! The iPhone Duo (iOS 27.1 runtimes on): two built-in screens either side of a hinge. The outer
//! screen (`LCD`, the device type's main screen, 1398x2034) shows the device folded; the inner one
//! (`LCD-1`, 2007x2853, a landscape panel stored portrait) shows it unfolded, and whichever is not
//! in use stays black.
//!
//! The simulator has no hinge sensor and no simulator UI folds it. SpringBoard's display tool
//! service (`SBSDisplayToolService`, the service its internal `suiatool display swap` drives) sweeps
//! a synthetic hinge to a position and moves the content across when the sweep crosses the swap
//! boundary (about 85° opening from closed). simee runs a small in-device program that asks it to.
//! Each screen has its own digitizer, addressed by its screen ID with the screen bit set.

/// The outer screen's pixels (`LCD`), the device type's main screen: what a folded Duo shows.
pub const OUTER_PX: (u32, u32) = (1398, 2034);
/// The inner screen's pixels as its framebuffer holds them (`LCD-1`): a landscape panel stored a
/// quarter turn from upright, so it lies unfolded at (2853, 2007).
pub const INNER_PX: (u32, u32) = (2007, 2853);

/// One of a two-screen device's built-in screens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// The outer (cover) screen: the device type's main screen.
    Outer,
    /// The inner screen, in use unfolded.
    Inner,
}

impl Screen {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outer => "outer",
            Self::Inner => "inner",
        }
    }
}

/// Whether a device type has two built-in screens and a hinge (the iPhone Duo), by its identifier
/// (`com.apple.CoreSimulator.SimDeviceType.iPhone-Duo`).
pub fn folds(device_type_id: &str) -> bool {
    device_type_id.rsplit('.').next() == Some("iPhone-Duo")
}

/// The screen a method's `display` parameter names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    /// The screen in use: the only one, or the lit one of an iPhone Duo's two.
    Active,
    /// A built-in screen by name (the iPhone Duo's `outer` or `inner`).
    Builtin(Screen),
    /// An iPhone's CarPlay screen.
    CarPlay,
}

impl Display {
    /// The `display` parameter's value: `main` (or none, or `active`), `outer`, `inner` or
    /// `carplay`.
    pub fn parse(name: Option<&str>) -> Result<Self, String> {
        match name.unwrap_or("main") {
            "main" | "active" => Ok(Self::Active),
            "outer" => Ok(Self::Builtin(Screen::Outer)),
            "inner" => Ok(Self::Builtin(Screen::Inner)),
            "carplay" | "car" => Ok(Self::CarPlay),
            other => Err(format!("unknown display `{other}` (main, outer, inner or carplay)")),
        }
    }
}

/// The screen in use given whether each of a two-screen device's screens shows anything: the
/// inner one when only it is lit, else the outer one (folded, booting, or both black).
pub fn active(outer_lit: bool, inner_lit: bool) -> Screen {
    if inner_lit && !outer_lit { Screen::Inner } else { Screen::Outer }
}

/// The screen a sweep to `angle` ends on when the angle alone says: the outer one when closed, the
/// inner one when open flat, none in between (SpringBoard's swap boundary decides).
pub fn expected_screen(angle: f64) -> Option<Screen> {
    match posture(angle) {
        "closed" => Some(Screen::Outer),
        "opened" => Some(Screen::Inner),
        _ => None,
    }
}

/// The screen in use once a swap has landed, or `None` while it hasn't: exactly one screen is lit
/// and it is the `expected` one, or, when none is expected, not the one `before` the swap (one
/// screen is lit before the swap lands too, so lit alone is not enough).
pub fn settled(before: Screen, expected: Option<Screen>, outer_lit: bool, inner_lit: bool) -> Option<Screen> {
    if outer_lit == inner_lit {
        return None;
    }
    let now = active(outer_lit, inner_lit);
    match expected {
        Some(e) => (now == e).then_some(now),
        None => (now != before).then_some(now),
    }
}

/// The screen a live view keeps showing: the lit one when just one is, else (mid-swap, or a black
/// app) the one it showed.
pub fn follow(shown: Screen, outer_lit: bool, inner_lit: bool) -> Screen {
    if outer_lit == inner_lit { shown } else { active(outer_lit, inner_lit) }
}

/// The bit SimulatorKit sets on a screen ID to address that screen's digitizer
/// (`IndigoHIDTargetForScreen`); the guest's HID service refuses a screen target without it.
pub const SCREEN_TARGET: u32 = 0x4000_0000;

/// The HID target of the digitizer on the screen `screen_id` (the `screenID` of its properties).
pub const fn digitizer_target(screen_id: u32) -> u32 {
    SCREEN_TARGET | screen_id
}

/// Whether a BGRA (or BGRX) frame shows anything: some pixel, of those on a `step`-pixel grid, is
/// not black. A screen not in use is exactly black; a dark app still draws its status bar.
pub fn lit(bgra: &[u8], width: usize, height: usize, stride: usize, step: usize) -> bool {
    let step = step.max(1);
    (0..height).step_by(step).any(|y| {
        let row = &bgra[(y * stride).min(bgra.len())..];
        (0..width).step_by(step).any(|x| row.get(x * 4..x * 4 + 3).is_some_and(|px| px.iter().any(|&c| c > 8)))
    })
}

/// The hinge angle of a closed device, degrees.
pub const CLOSED: f64 = 0.0;
/// The book posture (`suiatool`'s `book`, about 100°): open enough to unfold from closed.
pub const BOOK: f64 = 100.0;
/// Open flat, degrees.
pub const OPEN: f64 = 180.0;

/// The postures `fold` takes on an iPhone Duo, by their stock (Android) names.
pub const POSTURES: &str = "closed, half-opened (the book posture, 100°), opened";

/// The hinge angle a `fold` request asks for: `folded` (closed or open flat), an `angle`
/// (0-180°), or a `posture`; the first one given wins, as on Android.
pub fn hinge_angle(folded: Option<bool>, angle: Option<f64>, posture: Option<&str>) -> Result<f64, String> {
    match (folded, angle, posture) {
        (Some(folded), ..) => Ok(if folded { CLOSED } else { OPEN }),
        (None, Some(a), _) if (CLOSED..=OPEN).contains(&a) => Ok(a),
        (None, Some(a), _) => Err(format!("an iPhone Duo's hinge opens 0-180°, not {a}")),
        (None, None, Some(name)) => match name {
            "closed" | "1" => Ok(CLOSED),
            "half-opened" | "half_opened" | "book" | "2" => Ok(BOOK),
            "opened" | "open" | "3" => Ok(OPEN),
            "flipped" | "tent" | "4" | "5" => Err(format!("`{name}` is a dual-screen Android posture; an iPhone Duo's are {POSTURES}")),
            _ => Err(format!("unknown posture `{name}` ({POSTURES})")),
        },
        (None, None, None) => Err(format!("give `folded` (true|false), `angle` (0-180) or `posture` ({POSTURES})")),
    }
}

/// The posture name for a hinge angle, as `fold` reports it.
pub fn posture(angle: f64) -> &'static str {
    match angle {
        a if a < 10.0 => "closed",
        a if a > 170.0 => "opened",
        _ => "half-opened",
    }
}

/// The display tool's hinge position for an angle: 0 closed, 1 open flat.
pub fn position(angle: f64) -> f64 {
    (angle / OPEN).clamp(0.0, 1.0)
}

/// The visible part of the sweep, seconds: about a hand's fold, so the guest animates it as it
/// would. 0 asks for the instant swap, which only works from fully closed.
pub const SWEEP_SECONDS: f64 = 0.5;

/// The in-device display tool's arguments for a sweep to `angle`.
pub fn swap_args(angle: f64) -> Vec<String> {
    vec!["swap".into(), format!("{:.4}", position(angle)), format!("{SWEEP_SECONDS}")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_parameter_names_a_screen() {
        assert_eq!(Display::parse(None), Ok(Display::Active));
        assert_eq!(Display::parse(Some("main")), Ok(Display::Active), "main is the screen in use, as before");
        assert_eq!(Display::parse(Some("outer")), Ok(Display::Builtin(Screen::Outer)));
        assert_eq!(Display::parse(Some("inner")), Ok(Display::Builtin(Screen::Inner)));
        assert_eq!(Display::parse(Some("carplay")), Ok(Display::CarPlay));
        assert!(Display::parse(Some("hud")).expect_err("unknown").contains("main, outer, inner or carplay"));
        assert_eq!((Screen::Outer.as_str(), Screen::Inner.as_str()), ("outer", "inner"));
    }

    #[test]
    fn only_the_iphone_duo_folds() {
        assert!(folds("com.apple.CoreSimulator.SimDeviceType.iPhone-Duo"));
        assert!(!folds("com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"));
        assert!(!folds("com.apple.CoreSimulator.SimDeviceType.iPhone-Duo-Max"));
        assert!(!folds(""));
    }

    #[test]
    fn the_inner_screen_is_in_use_only_when_it_alone_is_lit() {
        assert_eq!(active(true, false), Screen::Outer, "folded");
        assert_eq!(active(false, true), Screen::Inner, "unfolded");
        assert_eq!(active(false, false), Screen::Outer, "booting, or a black app: the device type's main screen");
        assert_eq!(active(true, true), Screen::Outer, "mid-sweep");
        // A live view switches only when one screen alone is lit.
        assert_eq!(follow(Screen::Inner, false, false), Screen::Inner);
        assert_eq!(follow(Screen::Inner, true, true), Screen::Inner);
        assert_eq!(follow(Screen::Inner, true, false), Screen::Outer);
        assert_eq!(follow(Screen::Outer, false, true), Screen::Inner);
    }

    #[test]
    fn a_swap_has_not_landed_while_the_old_screen_is_still_the_only_lit_one() {
        use Screen::{Inner, Outer};
        // Unfolding: the outer screen alone is lit before the swap lands.
        assert_eq!(settled(Outer, Some(Inner), true, false), None);
        assert_eq!(settled(Outer, Some(Inner), true, true), None, "mid-sweep");
        assert_eq!(settled(Outer, Some(Inner), false, false), None);
        assert_eq!(settled(Outer, Some(Inner), false, true), Some(Inner));
        // Folding.
        assert_eq!(settled(Inner, Some(Outer), false, true), None);
        assert_eq!(settled(Inner, Some(Outer), true, false), Some(Outer));
        // Already on the expected screen: done at once.
        assert_eq!(settled(Inner, Some(Inner), false, true), Some(Inner));
        // No expectation (an angle between): wait for a change.
        assert_eq!(settled(Outer, None, true, false), None);
        assert_eq!(settled(Outer, None, false, true), Some(Inner));
        assert_eq!(expected_screen(0.0), Some(Outer));
        assert_eq!(expected_screen(180.0), Some(Inner));
        assert_eq!(expected_screen(100.0), None);
    }

    #[test]
    fn each_screen_has_its_own_digitizer() {
        // The iPhone Duo's screen IDs: LCD 1, LCD-1 3 (TVOut is 2).
        assert_eq!(digitizer_target(3), 0x4000_0003);
        assert_eq!(digitizer_target(1), 0x4000_0001);
    }

    #[test]
    fn a_black_frame_is_unlit_and_one_lit_pixel_on_the_grid_lights_it() {
        let (w, h) = (64, 48);
        let stride = w * 4 + 16;
        let mut frame = vec![0u8; stride * h];
        assert!(!lit(&frame, w, h, stride, 8));
        // Alpha alone (BGRX's padding) isn't light.
        frame.iter_mut().skip(3).step_by(4).for_each(|a| *a = 0xff);
        assert!(!lit(&frame, w, h, stride, 8));
        frame[(16 * stride) + 24 * 4 + 1] = 200;
        assert!(lit(&frame, w, h, stride, 8));
        assert!(!lit(&frame, w, h, stride, 5), "off the grid");
        // Near-black noise doesn't count; a short buffer doesn't panic.
        let dim = vec![4u8; stride * h];
        assert!(!lit(&dim, w, h, stride, 1));
        assert!(!lit(&frame[..10], w, h, stride, 1));
    }

    #[test]
    fn fold_requests_become_hinge_angles() {
        assert_eq!(hinge_angle(Some(true), Some(90.0), None), Ok(CLOSED), "folded wins");
        assert_eq!(hinge_angle(Some(false), None, None), Ok(OPEN));
        assert_eq!(hinge_angle(None, Some(120.0), Some("closed")), Ok(120.0), "then angle");
        assert_eq!(hinge_angle(None, None, Some("half-opened")), Ok(BOOK));
        assert_eq!(hinge_angle(None, None, Some("opened")), Ok(OPEN));
        assert_eq!(hinge_angle(None, None, Some("closed")), Ok(CLOSED));
        assert!(hinge_angle(None, Some(200.0), None).expect_err("past flat").contains("0-180"));
        assert!(hinge_angle(None, None, Some("tent")).expect_err("android only").contains("dual-screen Android posture"));
        assert!(hinge_angle(None, None, Some("ajar")).expect_err("unknown").contains("unknown posture `ajar`"));
        assert!(hinge_angle(None, None, None).expect_err("nothing").starts_with("give `folded`"));
    }

    #[test]
    fn angles_map_to_postures_and_tool_positions() {
        assert_eq!([posture(0.0), posture(BOOK), posture(OPEN)], ["closed", "half-opened", "opened"]);
        assert_eq!((position(CLOSED), position(90.0), position(OPEN)), (0.0, 0.5, 1.0));
        assert_eq!(swap_args(BOOK), ["swap", "0.5556", "0.5"]);
        assert_eq!(swap_args(OPEN), ["swap", "1.0000", "0.5"]);
    }
}
