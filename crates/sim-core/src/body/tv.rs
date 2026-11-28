//! TV bodies: a television on a stand, for Apple TV and Android TV.
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. A television is a thin panel slab with a narrow bezel and a pedestal and base as
//! `Role::Stand` [`super::Block`]s below it; the Apple TV rows also put the Apple TV 4K box and the
//! Siri Remote on the surface beside the set.
//!
//! A TV's screen is drawn at its panel's size, not at the density simee reports: Android TV's
//! 320 dpi is a UI density (a 1080p AVD is `xhdpi`, though its panel is 55 inches), and tvOS reports
//! scale 1. The diagonal is 55 inches, nominal, as the SDK's TV definitions say, and the density
//! follows from it.

use super::table::{Spec, reference_shape};
use super::*;

/// TVs have bodies ([`super::has_body`]).
pub(super) const ENABLED: bool = true;

/// A TV's generic body is a nominal 55 inch set whatever density simee reports ([`super::has_body`]'s
/// sibling check in `tests.rs`): its screen is drawn by diagonal, not by dpi.
#[cfg(test)]
pub(super) const SIZES_BY_DPI: bool = false;

/// The nominal panel diagonal, inches: the SDK's TV definitions (`tv.xml`) are all 55".
const DIAGONAL: f32 = 55.0;

const ANDROID_TV: &str =
    "Android SDK tv.xml: diagonal and resolution; bezel, thickness and stand estimated (no maker publishes a stand for the emulated set)";
const APPLE_TV: &str = "no television published: a nominal 55 inch 16:9 set on a generic stand, with the Apple TV 4K box and Siri Remote at Apple's tech specs (apple.com/apple-tv-4k/specs); bezel, stand and the box's and remote's corner radii estimated";
const TV_GENERIC: &str = "no published dimensions: a nominal 55 inch television at the screen's pixels (the density follows from the diagonal), with generic bezels and stand";

/// Bezel on every side, mm: the thin frame of an edge-lit panel.
const BEZEL: f32 = 9.0;
/// Panel thickness, mm.
const THICKNESS: f32 = 14.0;

/// The Apple TV 4K (3rd generation, Wi-Fi + Ethernet): width, height, depth, mm.
const APPLE_TV_BOX: [f32; 3] = [93.24, 31.0, 93.24];
/// The Siri Remote (3rd generation) lying on its back: length, thickness, width, mm.
const SIRI_REMOTE: [f32; 3] = [136.0, 9.25, 35.0];

/// One TV row: a device type id, its name and its panel's pixels.
struct Tv {
    id: &'static str,
    name: &'static str,
    px: (u32, u32),
}

const ANDROID: [Tv; 3] = [
    Tv { id: "tv_720p", name: "Television (720p)", px: (1280, 720) },
    Tv { id: "tv_1080p", name: "Television (1080p)", px: (1920, 1080) },
    Tv { id: "tv_4k", name: "Television (4K)", px: (3840, 2160) },
];

const APPLE: [Tv; 2] = [
    Tv { id: "Apple-TV-4K-3rd-generation-4K", name: "Apple TV 4K (3rd generation)", px: (3840, 2160) },
    Tv { id: "Apple-TV-4K-3rd-generation-1080p", name: "Apple TV 4K (3rd generation) (at 1080p)", px: (1920, 1080) },
];

/// The TV rows of the table.
pub(super) fn rows() -> Vec<Body> {
    let apple = APPLE.iter().map(|t| television(t.id, t.name, Platform::Ios, Source::Generic(APPLE_TV), t.px, true));
    let android = ANDROID.iter().map(|t| television(t.id, t.name, Platform::Android, Source::Published(ANDROID_TV), t.px, false));
    apple.chain(android).collect()
}

/// A TV with no row: the screen's pixels on a nominal 55 inch panel.
pub(super) fn generic(g: &Spec) -> Option<Body> {
    Some(television(g.id, &g.name, g.platform, Source::Generic(TV_GENERIC), g.px, g.platform == Platform::Ios))
}

fn block(role: Role, center: [f32; 3], size: [f32; 3], radius: f32, bevel: f32) -> Block {
    Block::new(role, center, size, Corner::round(radius), bevel)
}

/// A television at `px` with a 55 inch diagonal, on a pedestal and a base; `apple` adds the Apple
/// TV box and the Siri Remote on the surface beside it. The row's id is `id` (the panel is centred
/// on the body's origin and the stand hangs below it).
fn television(id: &str, name: &str, platform: Platform, source: Source, px: (u32, u32), apple: bool) -> Body {
    let ppi = (px.0 as f32).hypot(px.1 as f32) / DIAGONAL;
    let mut screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px,
        ppi,
        center: [0.0, 0.0],
        shape: reference_shape(platform, FormFactor::Tv),
        corner_radius: 0.0,
        cutout: None,
        published: matches!(source, Source::Published(_)).then_some(Published::Diagonal(DIAGONAL)),
    };
    // The screen is a rectangle however the profile says: a television's corners are square.
    screen.shape = Shape::Rectangle;
    let [sw, sh] = screen.size();
    let (w, h) = (sw + 2.0 * BEZEL, sh + 2.0 * BEZEL);

    // The pedestal reaches up behind the bottom bezel (no further than the bezel, so it stays clear
    // of the screen) and the base lies under it; both are centred on the panel's depth.
    let (neck, neck_w, base, base_w, base_d) = (70.0, 150.0, 12.0, 460.0, 240.0);
    let neck_top = -h / 2.0 + BEZEL - 1.0;
    let neck_bottom = neck_top - neck - 8.0;
    let base_top = neck_bottom + 2.0;
    let floor = base_top - base;
    let mut extras = vec![
        block(Role::Stand, [0.0, (neck_top + neck_bottom) / 2.0, 0.0], [neck_w, neck + 8.0, 24.0], 4.0, 3.0),
        block(Role::Stand, [0.0, (base_top + floor) / 2.0, 0.0], [base_w, base, base_d], 5.0, 3.0),
    ];
    if apple {
        // On the surface, the box to the stand's right and the remote in front of it to the left.
        let [bw, bh, bd] = APPLE_TV_BOX;
        let box_x = base_w / 2.0 + 60.0 + bw / 2.0;
        extras.push(block(Role::Trim, [box_x, floor + bh / 2.0, 0.0], [bw, bh, bd], 6.0, 8.0));
        let [rl, rt, rw] = SIRI_REMOTE;
        extras.push(block(Role::Trim, [-(base_w / 2.0 + 60.0 + rl / 2.0), floor + rt / 2.0, 40.0], [rl, rt, rw], rt / 2.0 - 0.25, 2.0));
    }

    Body {
        id: id.to_owned(),
        aliases: Vec::new(),
        name: name.to_owned(),
        platform,
        form_factor: FormFactor::Tv,
        source,
        size: [w, h, THICKNESS],
        corner: Corner { radius: 3.0, exponent: 2.0 },
        bevel: 1.0,
        screens: vec![screen],
        bump: None,
        buttons: Vec::new(),
        crown: None,
        fold: None,
        extras,
        // A black moulded set: a gloss bezel under the panel's glass, a plastic back.
        finish: Finish::new(Surface::Plastic, [0.012, 0.012, 0.014], Surface::Plastic, [0.016, 0.016, 0.018]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form_factor::Kind;

    fn row(id: &str) -> Arc<Body> {
        get(id).unwrap_or_else(|| panic!("{id} is a row"))
    }

    fn stands(b: &Body) -> Vec<&Block> {
        b.extras.iter().filter(|x| x.role == Role::Stand).collect()
    }

    #[test]
    fn tvs_have_bodies_and_the_rows_are_the_devices_the_sdk_and_simulator_name() {
        assert!(has_body(FormFactor::Tv));
        let ids: Vec<String> = rows().iter().map(|b| b.id.clone()).collect();
        assert_eq!(ids, ["Apple-TV-4K-3rd-generation-4K", "Apple-TV-4K-3rd-generation-1080p", "tv_720p", "tv_1080p", "tv_4k",]);
        for b in rows() {
            assert_eq!(b.form_factor, FormFactor::Tv);
            assert_eq!(b.platform, if b.id.starts_with("tv_") { Platform::Android } else { Platform::Ios });
            assert!(Arc::ptr_eq(&row(&b.id), all().iter().find(|r| r.id == b.id).expect("in the table")));
        }
        // Only the SDK's sets have a published panel; Apple makes none.
        assert!(matches!(row("tv_4k").source, Source::Published(_)));
        assert!(matches!(row("Apple-TV-4K-3rd-generation-4K").source, Source::Generic(_)));
    }

    #[test]
    fn the_face_is_16_by_9_at_the_pixels_simee_reports_and_55_inches() {
        for (id, px) in [
            ("tv_720p", (1280, 720)),
            ("tv_1080p", (1920, 1080)),
            ("tv_4k", (3840, 2160)),
            ("Apple-TV-4K-3rd-generation-4K", (3840, 2160)),
            ("Apple-TV-4K-3rd-generation-1080p", (1920, 1080)),
        ] {
            let b = row(id);
            let s = b.screen(Display::Main).expect("screen");
            assert_eq!(s.px, px, "{id}");
            let [w, h] = s.size();
            assert!((w / h - 16.0 / 9.0).abs() < 1e-3, "{id}: {w}x{h} mm");
            assert!((s.diagonal_inches() - DIAGONAL).abs() < 0.01, "{id}: {} in", s.diagonal_inches());
            // The 4K panel has twice the density of the 1080p one, the same size.
            assert!((w - 1217.6).abs() < 1.0 && (h - 684.9).abs() < 1.0, "{id}: {w}x{h} mm");
            assert!(b.shows(px));
            assert_eq!(s.shape, Shape::Rectangle);
        }
        // The SDK's density for the 4K set (80.10 dpi) is the panel's.
        assert!((row("tv_4k").screens[0].ppi - 80.1).abs() < 0.1);
        assert!((row("tv_1080p").screens[0].ppi - 40.05).abs() < 0.05);
        assert!((row("tv_720p").screens[0].ppi - 26.7).abs() < 0.05);
    }

    #[test]
    fn the_stand_is_below_the_panel_and_clear_of_the_screen() {
        let generics = [
            for_profile(&Kind::new(Platform::Android, FormFactor::Tv).expect("tv").profile()).expect("android"),
            for_profile(&Kind::new(Platform::Ios, FormFactor::Tv).expect("tv").profile()).expect("apple"),
        ];
        for b in rows().into_iter().map(Arc::new).chain(generics) {
            let s = &b.screens[0];
            let [l, r, bottom, top] = s.rect();
            let [bw, bh, _] = b.size;
            assert!(!stands(&b).is_empty(), "{}: a stand", b.id);
            let mut lowest = f32::MAX;
            for x in stands(&b) {
                let [cx, cy, _] = x.center;
                let [sx, sy, _] = x.size;
                // Clear of the screen: below its bottom edge, in the plane of the face.
                let (x_lo, x_hi) = (cx - sx / 2.0, cx + sx / 2.0);
                let overlaps = x_hi > l && x_lo < r && cy + sy / 2.0 > bottom && cy - sy / 2.0 < top;
                assert!(!overlaps, "{}: stand {:?} meets the screen", b.id, x.center);
                assert!(cy + sy / 2.0 <= -bh / 2.0 + BEZEL, "{}: stand stays behind the bezel", b.id);
                assert!(x_lo >= -bw && x_hi <= bw, "{}: stand within the set's width", b.id);
                lowest = lowest.min(cy - sy / 2.0);
            }
            // The pedestal reaches into the body's bottom edge (no gap) and hangs below it.
            let top = stands(&b).iter().map(|x| x.center[1] + x.size[1] / 2.0).fold(f32::MIN, f32::max);
            assert!(top > -bh / 2.0 && lowest < -bh / 2.0, "{}: joins the panel", b.id);
            // Everything else rests on the surface the stand stands on.
            for x in b.extras.iter().filter(|x| x.role != Role::Stand) {
                assert!((x.center[1] - x.size[1] / 2.0 - lowest).abs() < 1e-3, "{}: {:?} rests on the surface", b.id, x.role);
            }
        }
    }

    #[test]
    fn the_apple_tv_box_and_siri_remote_stand_beside_the_set_at_apples_sizes() {
        for id in ["Apple-TV-4K-3rd-generation-4K", "Apple-TV-4K-3rd-generation-1080p"] {
            let b = row(id);
            let sizes: Vec<[f32; 3]> = b.extras.iter().filter(|x| x.role == Role::Trim).map(|x| x.size).collect();
            assert_eq!(sizes, [[93.24, 31.0, 93.24], [136.0, 9.25, 35.0]], "{id}");
            let [bw, ..] = b.size;
            for x in b.extras.iter().filter(|x| x.role == Role::Trim) {
                assert!(x.center[0].abs() - x.size[0] / 2.0 > 0.0, "{id}: apart from the middle");
                // Beside the base, not under it or inside the panel's footprint.
                let base = &stands(&b)[1];
                assert!(x.center[0].abs() - x.size[0] / 2.0 >= base.size[0] / 2.0, "{id}: clear of the base");
                assert!(x.center[0].abs() < bw, "{id}: near the set");
            }
        }
        // The SDK's sets have no accessories.
        assert!(row("tv_1080p").extras.iter().all(|x| x.role == Role::Stand));
    }

    #[test]
    fn an_unlisted_tv_is_a_55_inch_generic_at_the_screen_it_reports() {
        let tv = Kind::new(Platform::Android, FormFactor::Tv).expect("tv").profile();
        // The density an AVD reports is the UI's, not the panel's.
        let b = for_profile(&tv.clone().with_screen(2560, 1440, Density::Dpi(320))).expect("generic");
        assert!(matches!(b.source, Source::Generic(_)));
        assert!(b.shows((2560, 1440)));
        let s = &b.screens[0];
        assert!((s.diagonal_inches() - DIAGONAL).abs() < 0.01, "{} in", s.diagonal_inches());
        assert_eq!(s.published, None);
        assert!(b.extras.iter().all(|x| x.role == Role::Stand), "no Apple accessories on Android");
        // An Apple TV type the table doesn't list gets the box and remote with its generic set.
        let ty = DeviceTypeInfo {
            identifier: "com.apple.CoreSimulator.SimDeviceType.Apple-TV-8K".into(),
            name: "Apple TV 8K".into(),
            model: String::new(),
            product_family: "Apple TV".into(),
            screen_width: 7680.0,
            screen_height: 4320.0,
            screen_scale: 1.0,
            platform: Platform::Ios,
            form_factor: Some(FormFactor::Tv),
        };
        let a = for_apple_type(&ty).expect("generic");
        assert!(a.shows((7680, 4320)) && matches!(a.source, Source::Generic(_)));
        assert_eq!(a.extras.len(), 4);
        assert!(Arc::ptr_eq(&a, &for_apple_type(&ty).expect("again")), "cached");
    }

    #[test]
    fn the_sdk_names_and_apple_types_pick_their_rows() {
        let tv = Kind::new(Platform::Android, FormFactor::Tv).expect("tv").profile();
        for (name, px) in [("tv_720p", (1280, 720)), ("tv_1080p", (1920, 1080)), ("tv_4k", (3840, 2160))] {
            let b = for_android(name, &tv.clone().with_screen(px.0, px.1, Density::Dpi(320))).expect("body");
            assert_eq!(b.id, name);
        }
        // A row's name with another screen is not that row.
        let b = for_android("tv_4k", &tv.clone().with_screen(1920, 1080, Density::Dpi(320))).expect("body");
        assert_ne!(b.id, "tv_4k");
        for (id, px) in [("Apple-TV-4K-3rd-generation-4K", (3840.0, 2160.0)), ("Apple-TV-4K-3rd-generation-1080p", (1920.0, 1080.0))] {
            let ty = DeviceTypeInfo {
                identifier: format!("com.apple.CoreSimulator.SimDeviceType.{id}"),
                name: String::new(),
                model: String::new(),
                product_family: "Apple TV".into(),
                screen_width: px.0,
                screen_height: px.1,
                screen_scale: 1.0,
                platform: Platform::Ios,
                form_factor: Some(FormFactor::Tv),
            };
            assert_eq!(for_apple_type(&ty).expect("row").id, id);
        }
    }
}
