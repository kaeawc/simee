//! Watch bodies: the Apple Watch rows (published case sizes and display areas) and the generic
//! watch (Wear OS, an unlisted Apple Watch) built from the screen simee reports.
//!
//! The form factor's module, as [`super::table`] asks of each: [`ENABLED`], [`rows`] and
//! [`generic`]. Straps, lugs and extra rows go here (as [`super::Body::extras`]), nowhere else.

use super::table::{GENERIC, Spec, reference_shape};
use super::*;

/// Watches have bodies ([`super::has_body`]).
pub(super) const ENABLED: bool = true;

/// Whether the generic body's screen is drawn at the dpi simee reports for the Android reference
/// profile (a watch is sized at the dpi simee reports). When false, the shared test checks only the aspect.
#[cfg(test)]
pub(super) const SIZES_BY_DPI: bool = true;

const APPLE_WATCH: &str = "Apple tech specs (apple.com/watch/compare): case size and display area; corner radii, the crystal's curve, crown, back crystal, button positions, lugs and band estimated (Apple publishes which bands fit each case, not their width in mm)";

const WEAR_SDK: &str = "Android SDK devices.xml (wear.xml): pixel size and xhdpi (320 dpi) density as simee reports them, no published dimensions (the SDK's diagonal-length is not a round screen's diameter); case, domed crystal, bezel, crown, button, lugs and band estimated";

struct Watch {
    id: &'static str,
    name: &'static str,
    /// Case width, height, depth, mm.
    size: [f32; 3],
    px: (u32, u32),
    /// Apple's published display area, mm².
    area: f32,
    corner: f32,
    screen_radius: f32,
    bevel: f32,
    crown: f32,
    /// The Action button on the left (Ultra).
    action: bool,
    /// The band's width at the lugs, mm.
    band: f32,
    finish: Finish,
}

/// Polished natural titanium over a dark ceramic back, the crystal curving down to the band.
const NATURAL: Finish = Finish::new(Surface::Polished, [0.58, 0.56, 0.52], Surface::Ceramic, [0.02, 0.02, 0.022]).wrapped();
/// Jet black: polished anodized aluminium.
const JET: Finish = Finish::new(Surface::Polished, [0.02, 0.02, 0.022], Surface::Ceramic, [0.02, 0.02, 0.022]).wrapped();
/// Starlight satin aluminium.
const STARLIGHT: Finish = Finish::new(Surface::Aluminium, [0.70, 0.67, 0.61], Surface::Ceramic, [0.03, 0.03, 0.032]).wrapped();
/// Brushed titanium under a flat sapphire crystal (no wrap): the Ultra.
const ULTRA: Finish = Finish::new(Surface::Titanium, [0.50, 0.48, 0.44], Surface::Ceramic, [0.025, 0.025, 0.027]);
/// A Wear OS case: polished silver aluminium under a domed crystal.
const WEAR_CASE: Finish = Finish::new(Surface::Polished, [0.66, 0.66, 0.67], Surface::Plastic, [0.03, 0.03, 0.033]).wrapped();

const fn apple_watch(id: &'static str, name: &'static str, size: [f32; 3], px: (u32, u32), area: f32, corner: f32) -> Watch {
    Watch {
        id,
        name,
        size,
        px,
        area,
        corner,
        screen_radius: corner - 3.4,
        bevel: 3.1,
        crown: 6.2,
        action: false,
        band: apple_band(size[1]),
        finish: NATURAL,
    }
}

/// The width of an Apple Watch band at the lugs, mm. Apple publishes two band families (38, 40,
/// 41 and 42mm cases; 44, 45, 46 and 49mm cases) that fit across their members, not a width;
/// retail band guides give about 20mm for the small family and 22mm for the large one, which is
/// what these are (an estimate).
const fn apple_band(case_height: f32) -> f32 {
    if case_height <= 42.0 { 20.0 } else { 22.0 }
}

const WATCHES: [Watch; 10] = [
    apple_watch("Apple-Watch-Series-10-46mm", "Apple Watch Series 10 (46mm)", [39.0, 46.0, 9.7], (416, 496), 1220.0, 12.5),
    apple_watch("Apple-Watch-Series-10-42mm", "Apple Watch Series 10 (42mm)", [36.0, 42.0, 9.7], (374, 446), 989.0, 11.5),
    Watch {
        finish: JET,
        ..apple_watch("Apple-Watch-Series-11-46mm", "Apple Watch Series 11 (46mm)", [39.0, 46.0, 9.7], (416, 496), 1220.0, 12.5)
    },
    Watch {
        finish: JET,
        ..apple_watch("Apple-Watch-Series-11-42mm", "Apple Watch Series 11 (42mm)", [36.0, 42.0, 9.7], (374, 446), 989.0, 11.5)
    },
    Watch {
        screen_radius: 6.0,
        bevel: 1.3,
        crown: 7.2,
        action: true,
        finish: ULTRA,
        ..apple_watch("Apple-Watch-Ultra-2-49mm", "Apple Watch Ultra 2 (49mm)", [44.0, 49.0, 14.4], (410, 502), 1185.0, 9.5)
    },
    // Ultra 3 (Apple tech specs: 49mm case as the Ultra 2's, 422x514 at 1245 mm²) and SE 3 (the
    // SE 2's cases and displays). The ids follow CoreSimulator's naming; a type whose id differs
    // still gets these dimensions by name or falls back to a generic body.
    Watch {
        screen_radius: 6.0,
        bevel: 1.3,
        crown: 7.2,
        action: true,
        finish: ULTRA,
        ..apple_watch("Apple-Watch-Ultra-3-49mm", "Apple Watch Ultra 3 (49mm)", [44.0, 49.0, 14.4], (422, 514), 1245.0, 9.5)
    },
    Watch {
        bevel: 3.0,
        finish: STARLIGHT,
        ..apple_watch("Apple-Watch-SE-3-44mm", "Apple Watch SE 3 (44mm)", [38.0, 44.0, 10.7], (368, 448), 977.0, 11.5)
    },
    Watch {
        bevel: 3.0,
        finish: STARLIGHT,
        ..apple_watch("Apple-Watch-SE-3-40mm", "Apple Watch SE 3 (40mm)", [34.0, 40.0, 10.7], (324, 394), 759.0, 10.5)
    },
    Watch {
        bevel: 3.0,
        finish: STARLIGHT,
        ..apple_watch(
            "Apple-Watch-SE-44mm-2nd-generation",
            "Apple Watch SE (44mm) (2nd generation)",
            [38.0, 44.0, 10.7],
            (368, 448),
            977.0,
            11.5,
        )
    },
    Watch {
        bevel: 3.0,
        finish: STARLIGHT,
        ..apple_watch(
            "Apple-Watch-SE-40mm-2nd-generation",
            "Apple Watch SE (40mm) (2nd generation)",
            [34.0, 40.0, 10.7],
            (324, 394),
            759.0,
            10.5,
        )
    },
];

/// The watch rows of the table.
pub(super) fn rows() -> Vec<Body> {
    let mut out: Vec<Body> = WATCHES.iter().map(watch).collect();
    out.extend(WEAR.iter().map(wear));
    out
}

/// An Android SDK Wear OS hardware profile: (id, name, pixels, round, band width mm). Names and
/// pixels are `wear.xml` in the SDK's `sdklib` (`wearos_small_round` 384x384, `wearos_large_round`
/// 454x454, `wearos_xl_round` 480x480, `wearos_square` 360x360, `wearos_rect` 402x476), all xhdpi.
/// simee infers a round screen unless the id has `square` or `rect`. The bands are estimates.
type WearRow = (&'static str, &'static str, (u32, u32), bool, f32);

const WEAR: [WearRow; 5] = [
    ("wearos_small_round", "Wear OS Small Round", (384, 384), true, 18.0),
    ("wearos_large_round", "Wear OS Large Round", (454, 454), true, 22.0),
    ("wearos_xl_round", "Wear OS XL Round", (480, 480), true, 22.0),
    ("wearos_square", "Wear OS Square", (360, 360), false, 18.0),
    ("wearos_rect", "Wear OS Rectangular", (402, 476), false, 20.0),
];

/// The density the SDK gives every Wear OS profile (xhdpi), dpi.
const WEAR_DPI: f32 = 320.0;

fn wear(&(id, name, px, round, band): &WearRow) -> Body {
    let shape = if round { Shape::Round } else { Shape::Rectangle };
    let screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px,
        ppi: WEAR_DPI,
        center: [0.0, 0.0],
        shape,
        corner_radius: 0.0,
        cutout: None,
        published: None,
    };
    let [sw, sh] = screen.size();
    let screen = Screen { corner_radius: if round { sw.min(sh) / 2.0 } else { 0.0 }, ..screen };
    let mut body = wear_case(Platform::Android, id, name.to_owned(), screen, Source::Generic(WEAR_SDK), 4.5);
    with_band(&mut body, band);
    body
}

fn watch(p: &Watch) -> Body {
    let [w, h, t] = p.size;
    // Apple publishes the display's area, not its density: the width that area gives at the
    // screen's aspect sets the density.
    let width = (p.area * p.px.0 as f32 / p.px.1 as f32).sqrt();
    let screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px: p.px,
        ppi: p.px.0 as f32 * MM_PER_INCH / width,
        center: [0.0, 0.0],
        shape: reference_shape(Platform::Ios, FormFactor::Watch),
        corner_radius: p.screen_radius,
        cutout: None,
        published: Some(Published::Area(p.area)),
    };
    // The side button lies nearly flush below the crown. An Ultra's crown and side button sit in
    // a guard that stands off the case's right side, and its Action button is on the left.
    let guard = if p.action { 1.8 } else { 0.0 };
    let mut buttons =
        vec![Button { name: "side", side: Side::Right, at: -0.14 * h, length: 0.2 * h, depth: guard + 0.25, width: 0.34 * t }];
    if p.action {
        buttons.push(Button { name: "action", side: Side::Left, at: 0.02 * h, length: 0.27 * h, depth: 0.5, width: 0.3 * t });
    }
    let dome = 0.72 * w.min(h);
    let mut body = Body {
        id: p.id.to_owned(),
        aliases: Vec::new(),
        name: p.name.to_owned(),
        platform: Platform::Ios,
        form_factor: FormFactor::Watch,
        source: Source::Published(APPLE_WATCH),
        size: [w, h, t],
        corner: Corner::continuous(p.corner),
        bevel: p.bevel,
        screens: vec![screen],
        bump: None,
        buttons,
        crown: Some(Crown { side: Side::Right, at: 0.16 * h, diameter: p.crown, length: guard + 2.2 }),
        fold: None,
        extras: Vec::new(),
        finish: p.finish,
    };
    body.extras = straps(&body, p.band);
    body.extras.push(back_crystal(t, dome));
    if p.action {
        // The crown guard: a block of the case's metal the crown and side button stand out of.
        let guard_block = [guard + 1.2, 0.56 * h, 0.66 * t];
        body.extras.push(Block::new(Role::Bump, [w / 2.0 + (guard - 1.2) / 2.0, 0.01 * h, 0.0], guard_block, Corner::round(1.4), 1.2));
    }
    body
}

/// The back crystal a watch's sensors sit under: a shallow glass dome on the back.
fn back_crystal(t: f32, diameter: f32) -> Block {
    let mut dome = Block::on_back(Role::Lens, [0.0, 0.0], -t / 2.0, diameter, 0.9, 0.3);
    dome.bevel = 0.8;
    dome
}

/// A watch with no published dimensions: the screen at its density in a generic case, round
/// for a round screen (a crown at 3 o'clock and a side button below it), else rounded (a crown
/// and a button), with lugs and a band top and bottom.
pub(super) fn generic(g: &Spec) -> Option<Body> {
    // Screen corner radius for a rounded screen.
    let screen = g.screen(6.0);
    let mut body = wear_case(g.platform, g.id, g.name.clone(), screen, Source::Generic(GENERIC), 3.5);
    let [w, h, _] = body.size;
    // Apple Watch bands by case height as the rows' ([`apple_band`]); a Wear OS band is about
    // half the case's narrower side (an estimate), 16 to 24mm.
    let band = if g.platform == Platform::Ios { apple_band(h) } else { (w.min(h) * 0.5).clamp(16.0, 24.0).round() };
    with_band(&mut body, band);
    Some(body)
}

/// The case, bump, crown and buttons around a watch screen: `side` is the bezel to the case's
/// edge, mm. Round screens get a round case.
fn wear_case(platform: Platform, id: &str, name: String, screen: Screen, source: Source, side: f32) -> Body {
    let round = screen.shape == Shape::Round;
    // A round case is a pebble: a crystal that domes from the screen's edge down to a band half
    // way through its thickness. A square one is flatter.
    let (t, bevel) = if round { (12.3, (side - MIN_RADIUS - 0.1).min(4.4)) } else { (11.0, (side - MIN_RADIUS - 0.1).min(2.6)) };
    let [sw, sh] = screen.size();
    let (size, corner) = if round {
        let d = sw.max(sh) + 2.0 * side;
        ([d, d, t], Corner::round(d / 2.0))
    } else {
        let corner = if screen.shape == Shape::Rounded {
            Corner::continuous(screen.corner_radius.max(2.0) + side)
        } else {
            Corner::round(2.0 + side)
        };
        ([sw + 2.0 * side, sh + 2.0 * side, t], corner)
    };
    let [w, h, _] = size;
    let buttons = if round {
        vec![round_side_button(w / 2.0)]
    } else {
        vec![Button { name: "button", side: Side::Right, at: -0.2 * h, length: 0.17 * h, depth: 0.3, width: 2.8 }]
    };
    let crown = Crown { side: Side::Right, at: if round { 0.0 } else { 0.1 * h }, diameter: 6.0, length: 2.2 };
    Body {
        id: id.to_owned(),
        aliases: Vec::new(),
        name,
        platform,
        form_factor: FormFactor::Watch,
        source,
        size,
        corner,
        bevel,
        screens: vec![screen],
        bump: None,
        buttons,
        crown: Some(crown),
        fold: None,
        extras: vec![back_crystal(t, 0.62 * w.min(h))],
        finish: WEAR_CASE,
    }
}

/// The side button of a round Wear OS case, below the crown at 3 o'clock (Pixel Watch style).
/// The mesh sets a key into the case's outline where the key is, so it follows the circle.
fn round_side_button(radius: f32) -> Button {
    Button { name: "button", side: Side::Right, at: -0.17 * 2.0 * radius, length: 0.12 * 2.0 * radius, depth: 0.45, width: 2.8 }
}

/// How far a case's outline stands in from its bounding box at a distance `x` from the centre
/// line, at the top and bottom edges, mm (corners taken as circular arcs).
fn edge_inset(size: [f32; 3], corner: Corner, x: f32) -> f32 {
    let r = corner.radius;
    let dx = x - (size[0] / 2.0 - r);
    if dx <= 0.0 {
        0.0
    } else {
        r - (r * r - {
            let d = dx.min(r);
            d * d
        })
        .sqrt()
    }
}

/// Puts the band's parts first among `body`'s extras (the checks and the scene find the lug there).
fn with_band(body: &mut Body, band: f32) {
    let mut extras = straps(body, band);
    extras.append(&mut body.extras);
    body.extras = extras;
}

/// How many segments each end of the band is drawn as, and how long each is, mm.
const BAND_SEGMENTS: usize = 7;
const BAND_SEGMENT: f32 = 7.0;

/// The lugs and the band, above and below the case: for each end a lug that sinks into the case
/// (past the outline's inset at the band's edge), then the band as [`BAND_SEGMENTS`] flat
/// segments that lean further back one after another, so it drapes away behind the case the way
/// a band hangs on a stand.
fn straps(body: &Body, band: f32) -> Vec<Block> {
    let [w, h, t] = body.size;
    let inset = edge_inset(body.size, body.corner, band / 2.0);
    // The lug reaches 3mm past the case's edge at the centre line; the band starts inside it.
    let (lug_len, lug_t) = (inset + 1.5 + 3.0, 0.5 * t);
    let band_t = 2.6;
    let strap_w = band.min(w - 4.0);
    let corner = Corner::round(1.2);
    let mut out = Vec::new();
    for sign in [1.0, -1.0] {
        let edge = sign * h / 2.0;
        // From 1.5mm + inset inside the edge, 3mm beyond it.
        let lug_mid = edge + sign * (3.0 - lug_len / 2.0);
        out.push(Block::new(Role::Strap, [0.0, lug_mid, -0.1 * t], [strap_w, lug_len, lug_t], corner, 1.0));
        // The band leaves the lug's end and leans back a little more with every segment.
        let (mut y, mut z) = (edge + sign * 2.0, -0.1 * t);
        for i in 0..BAND_SEGMENTS {
            let lean = (8.0 + 12.0 * i as f32).to_radians();
            let (dy, dz) = (sign * lean.cos(), -lean.sin());
            let center = [0.0, y + dy * BAND_SEGMENT / 2.0, z + dz * BAND_SEGMENT / 2.0];
            // Each overlaps the last by its thickness, so the bend shows no gap.
            let size = [strap_w - 1.0, BAND_SEGMENT + band_t, band_t];
            out.push(Block { tilt: sign * lean, ..Block::new(Role::Strap, center, size, corner, 1.0) });
            (y, z) = (y + dy * BAND_SEGMENT, z + dz * BAND_SEGMENT);
        }
    }
    out
}

// What holds on every platform: screens, shapes, strap clearances. The fingerprint pins are in
// `tests`, macOS only.
#[cfg(test)]
mod checks {
    use super::*;
    use crate::form_factor::Kind;

    /// The bodies the pins and checks below run over: the table's watches, the generic Wear OS
    /// shapes, a small round Wear OS and an Apple Watch the table doesn't list.
    pub(super) fn watch_bodies() -> Vec<Arc<Body>> {
        let watch = Kind::new(Platform::Android, FormFactor::Watch).expect("Wear OS").profile();
        let with_shape = |shape| {
            let mut p = watch.clone();
            p.shape = shape;
            for_profile(&p).expect("a watch")
        };
        let apple = DeviceTypeInfo {
            identifier: "com.apple.CoreSimulator.SimDeviceType.Apple-Watch-Series-12-46mm".into(),
            name: "Apple Watch Series 12 (46mm)".into(),
            model: String::new(),
            product_family: "Apple Watch".into(),
            screen_width: 416.0,
            screen_height: 496.0,
            screen_scale: 2.0,
            platform: Platform::Ios,
            form_factor: Some(FormFactor::Watch),
        };
        let mut bodies: Vec<Arc<Body>> = all().iter().filter(|b| b.form_factor == FormFactor::Watch).cloned().collect();
        bodies.extend([
            with_shape(Shape::Rectangle),
            with_shape(Shape::Rounded),
            for_profile(&watch.with_screen(384, 384, Density::Dpi(320))).expect("a small round watch"),
            for_apple_type(&apple).expect("an unlisted Apple Watch"),
        ]);
        bodies
    }

    fn box_xy(x: f32, y: f32, w: f32, h: f32) -> [f32; 4] {
        [x - w / 2.0, x + w / 2.0, y - h / 2.0, y + h / 2.0]
    }

    fn overlap(a: [f32; 4], b: [f32; 4]) -> bool {
        a[0] < b[1] && b[0] < a[1] && a[2] < b[3] && b[2] < a[3]
    }

    /// The crown's and each button's footprint on the front view, body coordinates.
    fn key_boxes(b: &Body) -> Vec<(&'static str, [f32; 4])> {
        let mut keys: Vec<(&'static str, Side, f32, f32, f32)> =
            b.buttons.iter().map(|k| (k.name, k.side, k.at, k.length, k.depth)).collect();
        if let Some(c) = &b.crown {
            keys.push(("crown", c.side, c.at, c.diameter, c.length));
        }
        keys.into_iter()
            .map(|(name, side, at, along, out)| {
                let [w, h, _] = b.size;
                let r = match side {
                    Side::Right => box_xy(w / 2.0 + out / 2.0, at, out, along),
                    Side::Left => box_xy(-w / 2.0 - out / 2.0, at, out, along),
                    Side::Top => box_xy(at, h / 2.0 + out / 2.0, along, out),
                    Side::Bottom => box_xy(at, -h / 2.0 - out / 2.0, along, out),
                };
                (name, r)
            })
            .collect()
    }

    const SDK_WEAR: [(&str, (u32, u32), bool); 5] = [
        ("wearos_small_round", (384, 384), true),
        ("wearos_large_round", (454, 454), true),
        ("wearos_xl_round", (480, 480), true),
        ("wearos_square", (360, 360), false),
        ("wearos_rect", (402, 476), false),
    ];

    #[test]
    fn sdk_wear_profiles_have_rows_that_match_the_screen_simee_reports() {
        for (id, px, round) in SDK_WEAR {
            let b = get(id).unwrap_or_else(|| panic!("{id} has a row"));
            assert_eq!((b.platform, b.form_factor), (Platform::Android, FormFactor::Watch), "{id}");
            let s = &b.screens[0];
            assert_eq!(s.px, px, "{id}");
            assert_eq!(s.shape, if round { Shape::Round } else { Shape::Rectangle }, "{id}");
            let [w, h] = s.size();
            assert!((w / h - px.0 as f32 / px.1 as f32).abs() < 1e-4, "{id}: aspect");
            assert!((s.ppi - 320.0).abs() < 1e-3, "{id}: xhdpi");
            // The AVD simee builds from that profile shows the row's screen.
            let mut profile =
                Kind::new(Platform::Android, FormFactor::Watch).expect("Wear OS").profile().with_screen(px.0, px.1, Density::Dpi(320));
            profile.shape = s.shape;
            let avd = for_android(id, &profile).expect("a body");
            assert_eq!(avd.id, id, "an AVD with this hardware profile gets its row");
        }
    }

    #[test]
    fn a_round_screen_is_a_circle() {
        let mut round = 0;
        for b in watch_bodies() {
            let s = &b.screens[0];
            if s.shape != Shape::Round {
                continue;
            }
            round += 1;
            let [w, h] = s.size();
            assert!((w - h).abs() < 1e-3, "{}: square bounds", b.id);
            assert!((s.corner_radius - w / 2.0).abs() < 1e-3, "{}: corner radius is the radius", b.id);
            // The case is a circle around it, the screen inside.
            assert!((b.size[0] - b.size[1]).abs() < 1e-3 && (b.corner.radius - b.size[0] / 2.0).abs() < 1e-3, "{}", b.id);
            assert!(w < b.size[0], "{}: screen inside the case", b.id);
        }
        assert!(round >= 5, "the round watches were checked: {round}");
    }

    #[test]
    fn round_wear_os_has_a_side_button_below_the_crown() {
        for (id, _, round) in SDK_WEAR.into_iter().filter(|w| w.2) {
            let b = get(id).expect("row");
            assert!(round);
            let crown = b.crown.expect("crown");
            let key = b.buttons.iter().find(|k| k.name == "button").unwrap_or_else(|| panic!("{id}: a side button"));
            assert_eq!((key.side, crown.side), (Side::Right, Side::Right), "{id}");
            assert!(key.at + key.length / 2.0 < crown.at - crown.diameter / 2.0, "{id}: below the crown, not touching it");
            // The mesh sets it into the circle where it is: it only says how far it stands proud.
            assert!((0.2..=1.0).contains(&key.depth), "{id}: stands {} mm proud", key.depth);
        }
        let g = for_profile(&Kind::new(Platform::Android, FormFactor::Watch).expect("Wear OS").profile()).expect("generic");
        assert!(g.buttons.iter().any(|k| k.name == "button"), "the generic round Wear OS watch has one too");
    }

    /// A body's band: its lugs and segments.
    fn band(b: &Body) -> Vec<&Block> {
        b.extras.iter().filter(|x| x.role == Role::Strap).collect()
    }

    #[test]
    fn the_band_is_a_lug_and_draped_segments_above_and_below_clear_of_the_keys() {
        for b in watch_bodies() {
            let id = &b.id;
            let band = band(&b);
            assert_eq!(band.len(), 2 * (1 + BAND_SEGMENTS), "{id}: a lug and the band's segments at each end");
            assert!(b.extras.iter().take(band.len()).all(|x| x.role == Role::Strap), "{id}: the band comes first");
            let [w, h, t] = b.size;
            let keys = key_boxes(&b);
            let (mut up, mut down) = (0, 0);
            for x in &band {
                let r = box_xy(x.center[0], x.center[1], x.size[0], x.size[1]);
                assert!(x.size[0] < w, "{id}: narrower than the case");
                assert!(x.size[2] <= t, "{id}: within the case's thickness");
                assert!(x.center[0].abs() < 1e-6, "{id}: on the centre line");
                if x.center[1] > 0.0 {
                    up += 1;
                } else {
                    down += 1;
                }
                for (name, k) in &keys {
                    assert!(!overlap(r, *k), "{id}: band block at {:?} touches the {name}", x.center);
                }
            }
            assert_eq!(up, down, "{id}");
            // Each end: an upright lug, then segments that lean back further one after another,
            // each starting where the last ended, the same on both ends.
            for (sign, end) in [(1.0, &band[..band.len() / 2]), (-1.0, &band[band.len() / 2..])] {
                assert_eq!(end[0].tilt, 0.0, "{id}: the lug is upright");
                let mut last = 0.0;
                for x in &end[1..] {
                    let lean = sign * x.tilt;
                    assert!(lean > last && lean < std::f32::consts::FRAC_PI_2, "{id}: leans {lean} after {last}");
                    assert!(x.center[2] < end[0].center[2] + 1e-3, "{id}: behind the lug");
                    last = lean;
                }
                let far = end.last().expect("a segment");
                assert!(sign * far.center[1] > h / 2.0 + 15.0, "{id}: the band goes well past the case");
            }
            for (a, z) in band[..band.len() / 2].iter().zip(&band[band.len() / 2..]) {
                assert!((a.center[1] + z.center[1]).abs() < 1e-4 && (a.center[2] - z.center[2]).abs() < 1e-4, "{id}: symmetric");
            }
            // The sensors' crystal is on the back, in the middle.
            let crystal = b.extras.iter().find(|x| x.role == Role::Lens).unwrap_or_else(|| panic!("{id}: a back crystal"));
            assert!(crystal.center[2] < 0.0 && crystal.center[0] == 0.0 && crystal.center[1] == 0.0, "{id}");
        }
    }

    #[test]
    fn an_ultra_has_a_crown_guard_and_a_flat_crystal() {
        for id in ["Apple-Watch-Ultra-2-49mm", "Apple-Watch-Ultra-3-49mm"] {
            let b = get(id).expect("row");
            assert!(!b.finish.glass_wraps && b.finish.frame == Surface::Titanium, "{id}");
            let guard = b.extras.iter().find(|x| x.role == Role::Bump).unwrap_or_else(|| panic!("{id}: a crown guard"));
            let reach = guard.center[0] + guard.size[0] / 2.0 - b.size[0] / 2.0;
            assert!(reach > 1.0, "{id}: stands {reach} mm off the right side");
            let crown = b.crown.expect("crown");
            let side = b.buttons.iter().find(|k| k.name == "side").expect("side button");
            assert!(crown.length > reach && side.depth > reach, "{id}: the crown and side button stand out of the guard");
        }
        // The others' crystal curves down to the band.
        assert!(get("Apple-Watch-Series-10-46mm").expect("row").finish.glass_wraps);
        assert!(get("wearos_large_round").expect("row").finish.glass_wraps);
    }

    #[test]
    fn the_lug_covers_the_case_outline_at_its_edges() {
        // On a round case the outline curves away from the lug's corners: the lug must still
        // reach the circle there, or a gap shows between lug and case.
        for b in watch_bodies() {
            let lug = &b.extras[0];
            let half = lug.size[0] / 2.0;
            let inner = lug.center[1] - lug.size[1] / 2.0;
            let outline = b.size[1] / 2.0 - edge_inset(b.size, b.corner, half);
            assert!(inner < outline - 1.0, "{}: lug starts at {inner}, the outline is at {outline}", b.id);
        }
    }

    #[test]
    fn apple_straps_use_the_band_families() {
        let width = |id: &str| get(id).expect("row").extras[1].size[0];
        // Stubs are a millimetre narrower than the lugs.
        assert_eq!(width("Apple-Watch-Series-10-42mm"), 19.0);
        assert_eq!(width("Apple-Watch-SE-40mm-2nd-generation"), 19.0);
        assert_eq!(width("Apple-Watch-Series-10-46mm"), 21.0);
        assert_eq!(width("Apple-Watch-Ultra-3-49mm"), 21.0);
    }

    #[test]
    fn newer_apple_watches_are_listed() {
        let u3 = get("Apple-Watch-Ultra-3-49mm").expect("Ultra 3");
        assert!(u3.shows((422, 514)));
        assert!(u3.buttons.iter().any(|k| k.name == "action"));
        for id in ["Apple-Watch-SE-3-44mm", "Apple-Watch-SE-3-40mm"] {
            assert!(get(id).is_some(), "{id}");
        }
    }
}

// Raw f32 fingerprints differ between platform libms: its pins are macOS only (update them on a
// Mac; anything that should hold everywhere goes in `checks`).
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::checks::watch_bodies;
    use super::*;

    /// FNV-1a over a body's every field (its `Debug`, which prints f32s exactly). Not the mesh's
    /// bits, as `super::super::tests::fingerprint` also takes: those come out of libm's sin and
    /// cos and differ between hosts (the shared phone pin fails on Linux), where these fields
    /// come from IEEE arithmetic and `sqrt` and are the same everywhere. The shared mesh tests
    /// cover the triangles.
    fn fingerprint(b: &Body) -> u64 {
        format!("{b:?}").bytes().fold(0xcbf2_9ce4_8422_2325, |h: u64, x| (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3))
    }

    /// Every watch body's fields. #500 pinned these (with their meshes) as the code moved out of
    /// `table.rs`; straps, the round side button and the new rows (#501) changed them on purpose,
    /// so a change here is a change to a watch's look: update the values with the change.
    #[test]
    fn watch_body_fingerprints() {
        let got: Vec<(String, u64)> = watch_bodies().iter().map(|b| (b.id.clone(), fingerprint(b))).collect();
        let want: Vec<(String, u64)> = PINS.iter().map(|&(id, f)| (id.to_owned(), f)).collect();
        assert_eq!(got, want, "{got:#x?}");
    }

    const PINS: [(&str, u64); 20] = [
        ("Apple-Watch-Series-10-46mm", 0x3043513cffb5e78e),
        ("Apple-Watch-Series-10-42mm", 0x19b22d5239729950),
        ("Apple-Watch-Series-11-46mm", 0x384ed1683299b3c5),
        ("Apple-Watch-Series-11-42mm", 0xf023e13f941c3cb3),
        ("Apple-Watch-Ultra-2-49mm", 0xf9f77e47ec34cdf2),
        ("Apple-Watch-Ultra-3-49mm", 0x674c77dbe8906109),
        ("Apple-Watch-SE-3-44mm", 0x5219e00738440f84),
        ("Apple-Watch-SE-3-40mm", 0x7f4348bbd93957c9),
        ("Apple-Watch-SE-44mm-2nd-generation", 0x8d8d571f7a58fea6),
        ("Apple-Watch-SE-40mm-2nd-generation", 0xcb9c5d90bbe7b0eb),
        ("wearos_small_round", 0x48af1d2da4d6ac07),
        ("wearos_large_round", 0x238f097501148134),
        ("wearos_xl_round", 0x8918188e98e76b4a),
        ("wearos_square", 0x13693e929e2d06c5),
        ("wearos_rect", 0x4fbf9ca8dac38a7b),
        ("android-watch-454x454-320dpi-round", 0x2cf5b75740279887),
        ("android-watch-454x454-320dpi-rectangle", 0x47e780b4e58b53a6),
        ("android-watch-454x454-320dpi-rounded", 0x58943d8f23ed3fe4),
        ("android-watch-384x384-320dpi-round", 0x3389a947c7cf9262),
        ("ios-watch-416x496-2x-rounded", 0xd1532b7afe3f1b8b),
    ];
}
