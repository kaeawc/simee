//! XR bodies (visionOS, Android XR headsets and AI glasses).
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. The chrome is the device itself with the live frame on its eye-side face (+z), not
//! a second virtual room: the frame already shows the room.
//!
//! A headset is a superellipse visor shell with a heavy bevel, a `Role::Trim` light seal that
//! shows as a lip around the eye-side face, a `Role::Lens` front glass on the outside (-z, what
//! the back view shows) and `Role::Strap` head straps running back toward -z. The frame is one
//! wide display whose density is chosen so it spans the eyepiece area, not the density simee
//! reports for it (a generic body too: [`SIZES_BY_DPI`] is false).
//!
//! Glasses (the SDK's `ai_glasses_device`, [`ANDROID_GLASSES_SCREEN`]) are a front bar with two
//! `Role::Lens` lenses, `Role::Strap` temples toward -z, and the HUD as the screen in the right
//! lens (+x as the eye-side face is seen).

use super::table::{GENERIC, Spec};
use super::*;
use crate::form_factor::{ANDROID_GLASSES_SCREEN, is_glasses_tag};

/// Headsets and glasses have bodies ([`super::has_body`]).
pub(super) const ENABLED: bool = true;

/// A headset's frame spans the eyepiece area (a width and height it fits), and the glasses' HUD
/// fits the lens, whatever dpi simee reports for the screen.
#[cfg(test)]
pub(super) const SIZES_BY_DPI: bool = false;

/// The widest and tallest a frame is drawn on a headset's face, mm: the eyepiece area.
const EYEPIECE: [f32; 2] = [150.0, 112.0];
/// The width of the glasses' HUD on the lens, mm.
const HUD_SPAN: f32 = 32.0;

const VISION_PRO: &str = "Apple publishes the Vision Pro's display (23 million pixels, 3660x3200 per eye) and weight (600-650 g) but no outer size: width, height, depth, visor shape, seal and strap are estimates, and the frame is drawn at a density that spans the eyepiece area";
const XR_HEADSET: &str = "no published dimensions for the SDK's xr_headset_device: width, height, depth, visor shape, seal and strap are estimates, and the frame is drawn at a density that spans the eyepiece area";
const GLASSES: &str = "no published dimensions for the SDK's ai_glasses_device: front bar, lenses and temples are estimates, and the HUD is drawn at a density that fits the right lens";

const fn corner(radius: f32, exponent: f32) -> Corner {
    Corner { radius, exponent }
}

fn block(role: Role, center: [f32; 3], size: [f32; 3], corner: Corner, bevel: f32) -> Block {
    Block::new(role, center, size, corner, bevel)
}

/// A headset row: the visor's size, and the width the frame spans on its eye-side face.
struct Headset {
    id: &'static str,
    /// Other identifiers it answers to: the default visionOS device type is `Apple-Vision-Pro-4K`.
    aliases: &'static [&'static str],
    name: &'static str,
    platform: Platform,
    source: &'static str,
    /// Width, height, depth, mm (estimates).
    size: [f32; 3],
    px: (u32, u32),
    /// Width of the frame on the face, mm.
    span: f32,
}

const HEADSETS: [Headset; 2] = [
    Headset {
        id: "Apple-Vision-Pro",
        aliases: &["Apple-Vision-Pro-4K"],
        name: "Apple Vision Pro",
        platform: Platform::Ios,
        source: VISION_PRO,
        size: [200.0, 135.0, 85.0],
        px: (2732, 2048),
        span: 150.0,
    },
    Headset {
        id: "xr_headset_device",
        aliases: &[],
        name: "Android XR headset",
        platform: Platform::Android,
        source: XR_HEADSET,
        size: [190.0, 130.0, 95.0],
        px: (2560, 2558),
        span: 112.0,
    },
];

/// The parts every headset has, from its size: the light seal, the front glass and the straps.
fn headset_extras([w, h, t]: [f32; 3]) -> Vec<Block> {
    let radius = 0.2 * h;
    let seal_depth = 0.18 * t;
    let strap = [0.07 * w, 0.3 * h, 1.1 * t + 0.6 * w];
    let top = [0.3 * w, 0.07 * w, strap[2]];
    vec![
        // A lip around the eye-side face: its front sits just behind the face, so only the rim
        // beyond the shell shows.
        block(Role::Trim, [0.0, 0.0, t / 2.0 - 2.0 - seal_depth / 2.0], [w + 6.0, h + 6.0, seal_depth], corner(radius + 3.0, 3.0), 0.0),
        block(Role::Lens, [0.0, 0.0, -t / 2.0 - 0.5], [0.8 * w, 0.62 * h, 3.0], corner(0.2 * h, 3.0), 0.8),
        block(Role::Strap, [-w / 2.0, 0.0, -strap[2] / 2.0], strap, corner(0.02 * w, 2.0), 1.5),
        block(Role::Strap, [w / 2.0, 0.0, -strap[2] / 2.0], strap, corner(0.02 * w, 2.0), 1.5),
        block(Role::Strap, [0.0, h / 2.0, -top[2] / 2.0], top, corner(0.02 * w, 2.0), 1.5),
    ]
}

/// The visor shell around a frame of `px` at `ppi` centred on the face, `size` its outline.
fn headset(id: &str, name: String, platform: Platform, source: Source, size: [f32; 3], screen: Screen) -> Body {
    let [_, h, _] = size;
    let [sw, sh] = screen.size();
    // The shell's corner keeps the frame's corners on the flat face, inside the outline inset by
    // the bevel: a corner at the margin `m` from the edge is inside while R(k - 1) <= k m - bevel,
    // k being the superellipse's 2^(1/n) reach along its diagonal.
    let margin = ((size[0] - sw) / 2.0).min((h - sh) / 2.0);
    let bevel = (0.8 * margin).min(8.0);
    let k = 2.0_f32.powf(1.0 / 3.0);
    let radius = (0.45 * h).min(0.9 * (k * margin - bevel) / (k - 1.0));
    Body {
        id: id.to_owned(),
        aliases: Vec::new(),
        name,
        platform,
        form_factor: FormFactor::Vision,
        source,
        size,
        corner: corner(radius, 3.0),
        bevel,
        screens: vec![screen],
        bump: None,
        buttons: Vec::new(),
        crown: None,
        fold: None,
        extras: headset_extras(size),
        // A silver aluminium frame; the outside is the front glass block over it.
        finish: Finish::new(Surface::Aluminium, [0.62, 0.62, 0.63], Surface::Aluminium, [0.62, 0.62, 0.63]),
    }
}

/// The rows of the table: the Vision Pro, the SDK's headset and its glasses.
pub(super) fn rows() -> Vec<Body> {
    let mut out: Vec<Body> = HEADSETS
        .iter()
        .map(|p| {
            let ppi = p.px.0 as f32 * MM_PER_INCH / p.span;
            let screen = Screen {
                display: Display::Main,
                face: Face::Front,
                px: p.px,
                ppi,
                center: [0.0, 0.0],
                shape: reference_shape(p.platform),
                corner_radius: 0.0,
                cutout: None,
                published: None,
            };
            let mut body = headset(p.id, p.name.to_owned(), p.platform, Source::Generic(p.source), p.size, screen);
            body.aliases = p.aliases.to_vec();
            body
        })
        .collect();
    let (w, h, _) = ANDROID_GLASSES_SCREEN;
    out.push(ai_glasses("ai_glasses_device", "Android XR AI glasses".to_owned(), Platform::Android, Source::Generic(GLASSES), (w, h)));
    out
}

fn reference_shape(platform: Platform) -> Shape {
    super::table::reference_shape(platform, FormFactor::Vision)
}

/// Glasses: a front bar, a lens each side, temples toward -z, the HUD of `px` fitted to the right
/// lens.
fn ai_glasses(id: &str, name: String, platform: Platform, source: Source, px: (u32, u32)) -> Body {
    let (lens_w, lens_h) = (50.0_f32, 40.0_f32);
    let eye = 36.0_f32;
    let lens_y = 2.0;
    let mut screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px,
        ppi: px.0.max(px.1) as f32 * MM_PER_INCH / HUD_SPAN,
        center: [0.0, 0.0],
        shape: reference_shape(platform),
        corner_radius: 0.0,
        cutout: None,
        published: None,
    };
    let [sw, sh] = screen.size();
    // The lens holds the HUD, however tall or wide it is.
    let (lens_w, lens_h) = (lens_w.max(sw + 6.0), lens_h.max(sh + 6.0));
    let eye = eye.max(lens_w / 2.0 + 2.0);
    screen.center = [eye, lens_y];
    let (w, h, t) = (2.0 * (eye + lens_w / 2.0) + 16.0, lens_h + 12.0, 8.0);
    let lens_corner = corner(0.3 * lens_h, 2.5);
    let temple = [4.0, 7.0, 120.0];
    let mut extras: Vec<Block> =
        [-eye, eye].iter().map(|&x| block(Role::Lens, [x, lens_y, -t / 2.0 - 0.5], [lens_w, lens_h, 2.4], lens_corner, 0.6)).collect();
    extras.extend([-1.0, 1.0].map(|s| block(Role::Strap, [s * (w / 2.0 - 2.0), 0.0, -temple[2] / 2.0], temple, corner(1.5, 2.0), 0.8)));
    Body {
        id: id.to_owned(),
        aliases: Vec::new(),
        name,
        platform,
        form_factor: FormFactor::Vision,
        source,
        size: [w, h, t],
        corner: corner(0.4 * h, 2.5),
        bevel: 2.0,
        screens: vec![screen],
        bump: None,
        buttons: Vec::new(),
        crown: None,
        fold: None,
        extras,
        // Black acetate frames.
        finish: Finish::new(Surface::Plastic, [0.014, 0.014, 0.016], Surface::Plastic, [0.014, 0.014, 0.016]),
    }
}

/// A headset or glasses with no row: Android XR glasses (the `ai-glasses` image) get a front bar
/// around the HUD, else a visor around the frame, which spans the eyepiece area.
pub(super) fn generic(g: &Spec) -> Option<Body> {
    if g.platform == Platform::Android && is_glasses_tag(g.image_tag) {
        return Some(ai_glasses(g.id, g.name.clone(), g.platform, Source::Generic(GENERIC), g.px));
    }
    let (px, py) = (g.px.0 as f32, g.px.1 as f32);
    let span = EYEPIECE[0].min(EYEPIECE[1] * px / py);
    let mut screen = g.screen(0.0);
    screen.ppi = px * MM_PER_INCH / span;
    let [sw, sh] = screen.size();
    let margin = (0.08 * sw.max(sh)).max(10.0);
    let (w, h) = (sw + 2.0 * margin, sh + 2.0 * margin);
    let t = (0.45 * h).clamp(40.0, 120.0);
    Some(headset(g.id, g.name.clone(), g.platform, Source::Generic(GENERIC), [w, h, t], screen))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form_factor::{Density, Kind};

    fn row(id: &str) -> Arc<Body> {
        get(id).unwrap_or_else(|| panic!("{id} is a row"))
    }

    fn aspect(b: &Body) -> f32 {
        let [w, h] = b.screens[0].size();
        w / h
    }

    fn extras(b: &Body, role: Role) -> Vec<&Block> {
        b.extras.iter().filter(|x| x.role == role).collect()
    }

    #[test]
    fn the_rows_are_in_the_table() {
        assert!(has_body(FormFactor::Vision));
        for (id, platform) in
            [("Apple-Vision-Pro", Platform::Ios), ("xr_headset_device", Platform::Android), ("ai_glasses_device", Platform::Android)]
        {
            let b = row(id);
            assert_eq!((b.platform, b.form_factor), (platform, FormFactor::Vision), "{id}");
            assert!(!b.source.note().is_empty());
        }
        // The full CoreSimulator identifier finds the Vision Pro too.
        assert_eq!(
            get("com.apple.CoreSimulator.SimDeviceType.Apple-Vision-Pro").map(|b| b.id.clone()).as_deref(),
            Some("Apple-Vision-Pro")
        );
        // The simulator's default visionOS type (`Apple-Vision-Pro-4K`) is the same body.
        assert_eq!(
            get("com.apple.CoreSimulator.SimDeviceType.Apple-Vision-Pro-4K").map(|b| b.id.clone()).as_deref(),
            Some("Apple-Vision-Pro")
        );
        // The Vision Pro's note says what Apple publishes and that the rest is estimated.
        let note = row("Apple-Vision-Pro").source.note().to_owned();
        assert!(note.contains("23 million pixels") && note.contains("estimates"), "{note}");
    }

    #[test]
    fn the_frame_has_the_aspect_simee_reports() {
        let want = [("Apple-Vision-Pro", 2732.0 / 2048.0), ("xr_headset_device", 2560.0 / 2558.0), ("ai_glasses_device", 1.0)];
        for (id, a) in want {
            let b = row(id);
            assert!((aspect(&b) - a).abs() < 1e-4, "{id}: {} vs {a}", aspect(&b));
            // Square pixels: face-on, one frame pixel is one pixel whatever the density.
            let s = &b.screens[0];
            let [w, h] = s.size();
            assert!((w / s.px.0 as f32 - h / s.px.1 as f32).abs() < 1e-5, "{id}");
        }
        for (platform, px, a) in [(Platform::Ios, (2732, 2048), 2732.0 / 2048.0), (Platform::Android, (2560, 2558), 2560.0 / 2558.0)] {
            let p = Kind::new(platform, FormFactor::Vision).expect("kind").profile();
            assert_eq!((p.width, p.height), px);
            let b = for_profile(&p).expect("generic headset");
            assert!((aspect(&b) - a).abs() < 1e-4);
        }
        assert_eq!(ANDROID_GLASSES_SCREEN.0, ANDROID_GLASSES_SCREEN.1);
    }

    #[test]
    fn the_frame_is_one_display_on_the_eye_side_face() {
        for id in ["Apple-Vision-Pro", "xr_headset_device", "ai_glasses_device"] {
            let b = row(id);
            assert_eq!(b.screens.len(), 1, "{id}");
            let s = &b.screens[0];
            assert_eq!((s.display, s.face), (Display::Main, Face::Front), "{id}");
        }
        // A headset's frame is centred and fills most of the visor's width.
        for id in ["Apple-Vision-Pro", "xr_headset_device"] {
            let b = row(id);
            let s = &b.screens[0];
            assert_eq!(s.center, [0.0, 0.0], "{id}");
            assert!(s.size()[0] > 0.5 * b.size[0] && s.size()[0] < b.size[0], "{id}: spans the eyepieces");
        }
    }

    #[test]
    fn a_headset_has_a_seal_a_front_glass_and_straps_behind() {
        for id in ["Apple-Vision-Pro", "xr_headset_device"] {
            let b = row(id);
            let [_, _, t] = b.size;
            assert!(b.bevel >= 4.0, "{id}: a heavy bevel");
            assert_eq!(b.corner.exponent, 3.0, "{id}: a superellipse");
            let seal = extras(&b, Role::Trim);
            assert_eq!(seal.len(), 1, "{id}");
            // The seal's front stays behind the face, so it never covers the frame.
            assert!(seal[0].center[2] + seal[0].size[2] / 2.0 < t / 2.0, "{id}");
            assert!(seal[0].size[0] > b.size[0] && seal[0].size[1] > b.size[1], "{id}: a lip around the shell");
            let straps = extras(&b, Role::Strap);
            assert!(straps.len() >= 2, "{id}");
            for s in straps {
                assert!(s.center[2] < 0.0 && s.center[2] - s.size[2] / 2.0 < -t / 2.0 - 50.0, "{id}: runs toward -z");
            }
        }
    }

    #[test]
    fn the_back_view_shows_the_visor() {
        for id in ["Apple-Vision-Pro", "xr_headset_device"] {
            let b = row(id);
            // The only screen faces +z, so flipping to the back shows the outside: a front glass
            // on the -z face.
            assert!(b.screens.iter().all(|s| s.face == Face::Front), "{id}");
            let glass = extras(&b, Role::Lens);
            assert_eq!(glass.len(), 1, "{id}");
            let g = glass[0];
            assert!(g.center[2] < -b.size[2] / 2.0 + g.size[2] / 2.0 && g.center[2] < 0.0, "{id}");
            assert!(g.size[0] < b.size[0] && g.size[1] < b.size[1], "{id}: inside the outline");
            let mesh = mesh(&b);
            let (lens, seal) = (Role::Lens, Role::Trim);
            assert!(mesh.parts.iter().any(|p| p.role == lens) && mesh.parts.iter().any(|p| p.role == seal), "{id}");
            // Nothing but the front glass and shell faces -z at the back: no screen there.
            assert!(mesh.parts.iter().all(|p| p.role != Role::Screen(Display::Outer)), "{id}");
        }
    }

    #[test]
    fn glasses_have_two_lenses_temples_and_the_hud_in_the_right_one() {
        let b = row("ai_glasses_device");
        let lenses = extras(&b, Role::Lens);
        assert_eq!(lenses.len(), 2);
        assert!(lenses[0].center[0] < 0.0 && lenses[1].center[0] > 0.0);
        let s = &b.screens[0];
        assert_eq!(s.px, (450, 450));
        let right = lenses[1];
        assert_eq!(s.center, [right.center[0], right.center[1]], "the HUD is in the right lens");
        let [l, r, bottom, top] = s.rect();
        assert!(l >= right.center[0] - right.size[0] / 2.0 && r <= right.center[0] + right.size[0] / 2.0, "within the lens");
        assert!(bottom >= right.center[1] - right.size[1] / 2.0 && top <= right.center[1] + right.size[1] / 2.0, "within the lens");
        let temples = extras(&b, Role::Strap);
        assert_eq!(temples.len(), 2);
        assert!(temples.iter().all(|t| t.center[2] < 0.0 && t.size[2] > 80.0), "toward -z");
        assert!(b.size[0] > 100.0 && b.size[2] < 20.0, "a front bar");
    }

    #[test]
    fn a_generic_body_fits_the_eyepiece_and_the_image_tag_picks_glasses() {
        let (w, h, dpi) = ANDROID_GLASSES_SCREEN;
        let glasses = Kind::new(Platform::Android, FormFactor::Vision).expect("xr").image_profile("ai-glasses");
        assert_eq!((glasses.width, glasses.height, glasses.density), (w, h, Density::Dpi(dpi)));
        let b = for_android_image("custom_glasses", &glasses, None, "ai-glasses").expect("glasses body");
        assert!(matches!(b.source, Source::Generic(_)) && b.shows((w, h)));
        assert_eq!(extras(&b, Role::Lens).len(), 2, "glasses, not a visor");
        let hud = &b.screens[0];
        assert!((hud.size()[0] - HUD_SPAN).abs() < 1e-3, "the HUD fits the lens, not {dpi} dpi");
        assert!(Arc::ptr_eq(&b, &for_android_image("custom_glasses", &glasses, None, "ai-glasses").expect("again")), "cached");
        // The same 450x450 screen on another image is a headset, a different body.
        let visor = for_android_image("custom", &glasses, None, "google-xr").expect("visor");
        assert_ne!(visor.id, b.id);
        assert!(extras(&visor, Role::Lens).len() == 1 && visor.screens[0].size()[0] <= EYEPIECE[0]);
        // The SDK's own device names get their rows.
        assert_eq!(
            for_android_image("ai_glasses_device", &glasses, None, "ai-glasses").map(|b| b.id.clone()).as_deref(),
            Some("ai_glasses_device")
        );
        let headset = Kind::new(Platform::Android, FormFactor::Vision).expect("xr").profile();
        assert_eq!(for_android("xr_headset_device", &headset).map(|b| b.id.clone()).as_deref(), Some("xr_headset_device"));
        // A generic headset's frame spans the eyepiece area at the aspect simee reports.
        for (platform, aspect) in [(Platform::Android, 2560.0 / 2558.0), (Platform::Ios, 2732.0 / 2048.0)] {
            let p = Kind::new(platform, FormFactor::Vision).expect("kind").profile();
            let g = for_profile(&p).expect("generic headset");
            let [sw, sh] = g.screens[0].size();
            assert!(sw <= EYEPIECE[0] + 1e-3 && sh <= EYEPIECE[1] + 1e-3 && (sw / sh - aspect).abs() < 1e-4, "{platform:?}: {sw}x{sh}");
            assert!(sw.max(sh * aspect) > 0.99 * EYEPIECE[0].min(EYEPIECE[1] * aspect), "{platform:?} fills it");
        }
    }

    #[test]
    fn every_screen_sits_inside_the_shell_outline() {
        let mut bodies: Vec<Arc<Body>> = all().iter().filter(|b| b.form_factor == FormFactor::Vision).cloned().collect();
        for platform in Platform::ALL {
            if let Ok(k) = Kind::new(platform, FormFactor::Vision) {
                bodies.extend(for_profile(&k.profile()));
            }
        }
        assert!(bodies.len() >= 5);
        for b in bodies {
            let [w, h, _] = b.size;
            let (a, c) = (w / 2.0 - b.corner.radius, h / 2.0 - b.corner.radius);
            let n = b.corner.exponent;
            let [l, r, bottom, top] = b.screens[0].rect();
            for (x, y) in [(l, bottom), (l, top), (r, bottom), (r, top)] {
                let (dx, dy) = ((x.abs() - a).max(0.0), (y.abs() - c).max(0.0));
                let norm = (dx.powf(n) + dy.powf(n)).powf(1.0 / n);
                assert!(norm <= b.corner.radius - b.bevel, "{}: the frame's corner is off the flat face", b.id);
            }
        }
    }
}
