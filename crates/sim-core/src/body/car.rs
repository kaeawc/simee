//! Car head-unit bodies (Android Automotive, CarPlay): a housing around the screen at its
//! density, a front strip of keys and a rotary knob, and the dash surround it sits in.
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. The keys (home, back, volume) are `Role::Button` [`super::Block`]s on the front
//! face, the knob a round `Role::Knob`, and the dash a `Role::Trim` slab behind the unit with vent
//! slots (more `Trim`), matching [`crate::form_factor::InputMethod::HeadUnit`]. The desktop's
//! `carplay` scene shows the device type `CarPlay-800x480`.
//!
//! Nobody publishes a head unit's casing, so every body here is an estimate around a screen: the
//! housing is the screen in even bezels over a control strip, the dash a generic surround.

use super::table::{GENERIC, Spec, reference_shape};
use super::*;

/// Car head units have bodies ([`super::has_body`]).
pub(super) const ENABLED: bool = true;

/// The generic body's screen is drawn at the dpi simee reports (a head unit is as big as its
/// profile (a head unit is sized at the dpi the AVD reports; its lane may change this). When false, the shared test checks only the aspect.
#[cfg(test)]
pub(super) const SIZES_BY_DPI: bool = true;

const AAOS: &str = "Android SDK automotive.xml hardware profiles (developer.android.com/training/cars/testing/emulator): \
     the screen's pixel size; density, housing, keys, knob and dash estimated";
const CARPLAY: &str = "CarPlay's 800x480 minimum display (Apple CarPlay guidelines) at a nominal 7.5 inch head unit; \
     housing, keys, knob and dash estimated";

/// A listed head unit: its screen as the profile gives it and the density it is drawn at.
struct Unit {
    id: &'static str,
    name: &'static str,
    platform: Platform,
    px: (u32, u32),
    ppi: f32,
    /// The density is the profile's own (else an estimate): the row publishes its screen.
    sdk_density: bool,
    source: &'static str,
}

/// The SDK's automotive profiles (landscape, portrait, ultrawide) and CarPlay's reference
/// display. The profiles publish pixels; the densities other than the 1024p profile's 160 are
/// estimates that put each screen at a plausible size in a car, so those rows are `Generic`.
const UNITS: [Unit; 6] = [
    Unit {
        id: "automotive_1024p_landscape",
        name: "Automotive (1024p landscape)",
        platform: Platform::Android,
        px: (1024, 768),
        ppi: 160.0,
        sdk_density: true,
        source: AAOS,
    },
    Unit {
        id: "automotive_1080p_landscape",
        name: "Automotive (1080p landscape)",
        platform: Platform::Android,
        px: (1080, 600),
        ppi: 160.0,
        sdk_density: false,
        source: AAOS,
    },
    Unit {
        id: "automotive_portrait",
        name: "Automotive Portrait",
        platform: Platform::Android,
        px: (800, 1280),
        ppi: 160.0,
        sdk_density: false,
        source: AAOS,
    },
    Unit {
        id: "automotive_large_portrait",
        name: "Automotive Large Portrait",
        platform: Platform::Android,
        px: (1280, 1606),
        ppi: 240.0,
        sdk_density: false,
        source: AAOS,
    },
    Unit {
        id: "automotive_ultrawide",
        name: "Automotive Ultrawide",
        platform: Platform::Android,
        px: (3904, 1320),
        ppi: 240.0,
        sdk_density: false,
        source: AAOS,
    },
    // 800x480 is 933 px on the diagonal: 7.5 inches is 124.4 ppi.
    Unit {
        id: "CarPlay-800x480",
        name: "CarPlay (800x480)",
        platform: Platform::Ios,
        px: (800, 480),
        ppi: 124.4,
        sdk_density: false,
        source: CARPLAY,
    },
];

/// The head-unit rows of the table.
pub(super) fn rows() -> Vec<Body> {
    UNITS
        .iter()
        .map(|u| {
            let shape = reference_shape(u.platform, FormFactor::Auto);
            let (source, published) = if u.sdk_density {
                let diagonal = (u.px.0 as f32).hypot(u.px.1 as f32) / u.ppi;
                (Source::Published(u.source), Some(Published::Diagonal(diagonal)))
            } else {
                (Source::Generic(u.source), None)
            };
            head_unit(u.id, u.name, u.platform, source, published, u.px, u.ppi, shape)
        })
        .collect()
}

/// A head unit with no row: the screen at its density in the same housing, keys, knob and dash.
pub(super) fn generic(g: &Spec) -> Option<Body> {
    Some(head_unit(g.id, &g.name, g.platform, Source::Generic(GENERIC), None, g.px, g.ppi, g.shape))
}

#[allow(clippy::too_many_arguments)]
fn head_unit(
    id: &str,
    name: &str,
    platform: Platform,
    source: Source,
    published: Option<Published>,
    px: (u32, u32),
    ppi: f32,
    shape: Shape,
) -> Body {
    // Side and top bezel, the control strip under the screen, thickness, edge bevel.
    let (side, top, strip, t, bevel) = (10.0, 8.0, 32.0, 36.0, 1.5);
    let spec = Spec { id, name: name.to_owned(), platform, form_factor: FormFactor::Auto, px, ppi, shape, image_tag: "" };
    let mut screen = spec.screen(2.0);
    let [sw, sh] = screen.size();
    let (w, h) = (sw + 2.0 * side, sh + top + strip);
    // The screen sits above the strip: its centre is lifted by half the difference.
    screen.published = published;
    screen.center = [0.0, (strip - top) / 2.0];
    let strip_y = -h / 2.0 + strip / 2.0;
    let round = |d: f32| Corner { radius: d / 2.0, exponent: 2.0 };
    let soft = Corner { radius: 2.0, exponent: 2.0 };

    let mut extras = Vec::new();
    // Home, back and volume keys, left to right from the housing's left edge, standing 2.75 mm
    // off the front face.
    for i in 0..3 {
        let x = -w / 2.0 + side + 10.0 + i as f32 * 18.0;
        extras.push(Block { role: Role::Button, center: [x, strip_y, t / 2.0 + 1.0], size: [14.0, 9.0, 3.5], corner: soft, bevel: 0.8 });
    }
    // The rotary knob at the strip's right end.
    extras.push(Block {
        role: Role::Knob,
        center: [w / 2.0 - side - 14.0, strip_y, t / 2.0 + 5.0],
        size: [22.0, 22.0, 14.0],
        corner: round(22.0),
        bevel: 1.5,
    });
    // The dash: a slab behind the unit's front, wider than it on every side, with three vent slots
    // each side of the unit standing 1 mm off the dash.
    let dash_z = t / 2.0 - 6.0 - 5.0;
    extras.push(Block {
        role: Role::Trim,
        center: [0.0, 0.0, dash_z],
        size: [w + 80.0, h + 60.0, 10.0],
        corner: Corner { radius: 8.0, exponent: 2.0 },
        bevel: 0.0,
    });
    for side_sign in [-1.0, 1.0] {
        for row in [-1.0, 0.0, 1.0] {
            extras.push(Block {
                role: Role::Trim,
                center: [side_sign * (w / 2.0 + 20.0), row * 9.0, dash_z + 5.0],
                size: [24.0, 3.5, 2.0],
                corner: round(3.5),
                bevel: 0.5,
            });
        }
    }
    Body {
        id: id.to_owned(),
        aliases: Vec::new(),
        name: name.to_owned(),
        platform,
        form_factor: FormFactor::Auto,
        source,
        size: [w, h, t],
        corner: Corner { radius: 6.0, exponent: 2.0 },
        bevel,
        screens: vec![screen],
        bump: None,
        buttons: Vec::new(),
        crown: None,
        fold: None,
        extras,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form_factor::Kind;

    fn close(a: f32, b: f32, tolerance: f32) -> bool {
        (a - b).abs() <= tolerance
    }

    fn unit(id: &str) -> Arc<Body> {
        get(id).unwrap_or_else(|| panic!("{id} has a row"))
    }

    /// Whether a block's footprint (x, y) overlaps a screen's rect.
    fn overlaps(x: &Block, s: &Screen) -> bool {
        let [l, r, b, t] = s.rect();
        let (xl, xr) = (x.center[0] - x.size[0] / 2.0, x.center[0] + x.size[0] / 2.0);
        let (yb, yt) = (x.center[1] - x.size[1] / 2.0, x.center[1] + x.size[1] / 2.0);
        xl < r && xr > l && yb < t && yt > b
    }

    #[test]
    fn the_default_head_unit_is_the_screen_at_its_density() {
        let b = unit("automotive_1024p_landscape");
        let s = &b.screens[0];
        assert_eq!(s.px, (1024, 768));
        let [w, h] = s.size();
        assert!(close(w, 162.56, 0.01) && close(h, 121.92, 0.01), "{w}x{h}");
        assert!(close(w / h, 4.0 / 3.0, 1e-3));
        // The housing is the screen plus its bezels and the control strip.
        assert!(b.size[0] > w && b.size[1] > h);
        assert!(b.shows((1024, 768)));
    }

    #[test]
    fn every_row_draws_its_own_pixels_in_its_housing() {
        for u in &UNITS {
            let b = unit(u.id);
            assert_eq!(b.platform, u.platform);
            let s = &b.screens[0];
            assert_eq!(s.px, u.px, "{}", b.id);
            let [l, r, bottom, t] = s.rect();
            let [w, h, _] = b.size;
            assert!(l > -w / 2.0 && r < w / 2.0 && bottom > -h / 2.0 && t < h / 2.0, "{}: the screen is in the housing", b.id);
        }
        // Landscape, portrait and ultrawide are all there.
        let aspect = |id: &str| {
            let [w, h] = unit(id).screens[0].size();
            w / h
        };
        assert!(aspect("automotive_1080p_landscape") > 1.0 && aspect("automotive_portrait") < 1.0 && aspect("automotive_ultrawide") > 2.5);
        // CarPlay's 800x480 at a nominal 7.5 inch panel.
        let carplay = unit("CarPlay-800x480");
        assert_eq!(carplay.platform, Platform::Ios);
        assert!(close(carplay.screens[0].diagonal_inches(), 7.5, 0.05));
        assert!(carplay.screens[0].diagonal_inches() > 7.0 && carplay.screens[0].diagonal_inches() < 8.0);
    }

    #[test]
    fn keys_and_knob_stand_outside_the_screen_on_the_front() {
        for b in all().iter().filter(|b| b.form_factor == FormFactor::Auto).cloned().chain(generic_unit()) {
            let s = &b.screens[0];
            let count = |role| b.extras.iter().filter(|x| x.role == role).count();
            assert_eq!((count(Role::Button), count(Role::Knob)), (3, 1), "{}: home, back, volume and a knob", b.id);
            for x in b.extras.iter().filter(|x| matches!(x.role, Role::Button | Role::Knob)) {
                assert!(!overlaps(x, s), "{}: {:?} at {:?} is clear of the screen", b.id, x.role, x.center);
                // On the front face, within the housing's outline, standing proud of it.
                let [w, h, t] = b.size;
                assert!(x.center[2] + x.size[2] / 2.0 > t / 2.0, "{}: {:?} stands off the front", b.id, x.role);
                assert!(x.center[0].abs() + x.size[0] / 2.0 < w / 2.0 && x.center[1].abs() + x.size[1] / 2.0 < h / 2.0, "{}", b.id);
            }
            let knob = b.extras.iter().find(|x| x.role == Role::Knob).expect("knob");
            assert_eq!(knob.size[0], knob.size[1], "{}: the knob is round", b.id);
            assert_eq!(knob.corner.radius, knob.size[0] / 2.0);
        }
    }

    #[test]
    fn the_dash_is_a_trim_slab_behind_the_unit_with_vents() {
        let b = unit("automotive_1024p_landscape");
        let trims: Vec<&Block> = b.extras.iter().filter(|x| x.role == Role::Trim).collect();
        let dash = trims[0];
        assert!(dash.size[0] > b.size[0] && dash.size[1] > b.size[1], "the dash surrounds the unit");
        assert!(dash.center[2] + dash.size[2] / 2.0 < b.size[2] / 2.0, "the dash is behind the unit's front");
        let vents = &trims[1..];
        assert_eq!(vents.len(), 6);
        for v in vents {
            // Each slot is clear of the unit's sides, on the dash's face, standing off it.
            assert!(v.center[0].abs() - v.size[0] / 2.0 > b.size[0] / 2.0);
            assert!(v.center[0].abs() + v.size[0] / 2.0 < dash.size[0] / 2.0);
            assert!(v.center[2] + v.size[2] / 2.0 > dash.center[2] + dash.size[2] / 2.0);
        }
    }

    #[test]
    fn simee_kinds_and_unlisted_screens_get_a_head_unit() {
        for platform in Platform::ALL {
            let Ok(k) = Kind::new(platform, FormFactor::Auto) else { continue };
            let p = k.profile();
            let b = for_profile(&p).unwrap_or_else(|| panic!("{platform:?} has a body"));
            assert_eq!(b.screens[0].px, (p.width, p.height));
            assert!(b.shows((p.width, p.height)));
        }
        let android = Kind::new(Platform::Android, FormFactor::Auto).expect("automotive").profile();
        // The SDK profile's AVD gets its row; a different screen gets a generic body for it.
        assert_eq!(for_android("automotive_1024p_landscape", &android).expect("row").id, "automotive_1024p_landscape");
        let odd = android.with_screen(1920, 720, Density::Dpi(213));
        let g = for_android("automotive_1024p_landscape", &odd).expect("generic");
        assert!(g.id.starts_with("android-auto-1920x720") && g.shows((1920, 720)));
        assert!(matches!(g.source, Source::Generic(_)));
    }

    fn generic_unit() -> Option<Arc<Body>> {
        let android = Kind::new(Platform::Android, FormFactor::Auto).expect("automotive").profile();
        for_profile(&android.with_screen(1920, 720, Density::Dpi(213)))
    }

    #[test]
    fn carplay_device_type_gets_its_row() {
        let info = DeviceTypeInfo {
            identifier: "com.apple.CoreSimulator.SimDeviceType.CarPlay-800x480".into(),
            name: "CarPlay (800x480)".into(),
            model: String::new(),
            product_family: "CarPlay".into(),
            screen_width: 800.0,
            screen_height: 480.0,
            screen_scale: 2.0,
            platform: Platform::Ios,
            form_factor: Some(FormFactor::Auto),
        };
        assert_eq!(for_apple_type(&info).expect("a head unit").id, "CarPlay-800x480");
    }
}
