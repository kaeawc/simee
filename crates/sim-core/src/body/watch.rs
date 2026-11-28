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
pub(super) const SIZES_BY_DPI: bool = true;

const APPLE_WATCH: &str = "Apple tech specs: case size and display area; corner radii, crown, sensor dome and button positions estimated";

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
}

const fn apple_watch(id: &'static str, name: &'static str, size: [f32; 3], px: (u32, u32), area: f32, corner: f32) -> Watch {
    Watch { id, name, size, px, area, corner, screen_radius: corner - 3.4, bevel: 2.4, crown: 5.5, action: false }
}

const WATCHES: [Watch; 7] = [
    apple_watch("Apple-Watch-Series-10-46mm", "Apple Watch Series 10 (46mm)", [39.0, 46.0, 9.7], (416, 496), 1220.0, 12.5),
    apple_watch("Apple-Watch-Series-10-42mm", "Apple Watch Series 10 (42mm)", [36.0, 42.0, 9.7], (374, 446), 989.0, 11.5),
    apple_watch("Apple-Watch-Series-11-46mm", "Apple Watch Series 11 (46mm)", [39.0, 46.0, 9.7], (416, 496), 1220.0, 12.5),
    apple_watch("Apple-Watch-Series-11-42mm", "Apple Watch Series 11 (42mm)", [36.0, 42.0, 9.7], (374, 446), 989.0, 11.5),
    Watch {
        screen_radius: 6.0,
        bevel: 2.0,
        crown: 7.0,
        action: true,
        ..apple_watch("Apple-Watch-Ultra-2-49mm", "Apple Watch Ultra 2 (49mm)", [44.0, 49.0, 14.4], (410, 502), 1185.0, 9.5)
    },
    Watch {
        bevel: 2.6,
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
        bevel: 2.6,
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
    WATCHES.iter().map(watch).collect()
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
    let mut buttons = vec![Button { name: "side", side: Side::Right, at: -0.12 * h, length: 0.17 * h, depth: 0.6, width: 3.5 }];
    if p.action {
        buttons.push(Button { name: "action", side: Side::Left, at: 0.1 * h, length: 0.25 * h, depth: 0.8, width: 4.0 });
    }
    let dome = 0.65 * w.min(h);
    Body {
        id: p.id.to_owned(),
        aliases: Vec::new(),
        name: p.name.to_owned(),
        platform: Platform::Ios,
        form_factor: FormFactor::Watch,
        source: Source::Published(APPLE_WATCH),
        size: [w, h, t],
        corner: Corner { radius: p.corner, exponent: 3.0 },
        bevel: p.bevel,
        screens: vec![screen],
        bump: Some(Bump { center: [0.0, 0.0], size: [dome, dome], corner: Corner { radius: dome / 2.0, exponent: 2.0 }, height: 0.8 }),
        buttons,
        crown: Some(Crown { side: Side::Right, at: 0.15 * h, diameter: p.crown, length: 2.6 }),
        fold: None,
        extras: Vec::new(),
    }
}

/// A watch with no published dimensions: the screen at its density in a generic case, round
/// for a round screen (a crown at 3 o'clock), else rounded (a crown and a button).
pub(super) fn generic(g: &Spec) -> Option<Body> {
    // Side bezel, thickness, screen corner radius, edge bevel.
    let (side, t, radius, bevel) = (3.5, 11.0, 6.0, 2.0);
    let screen = g.screen(radius);
    let [sw, sh] = screen.size();
    let (size, corner) = if g.shape == Shape::Round {
        let d = sw.max(sh) + 2.0 * side;
        ([d, d, t], Corner { radius: d / 2.0, exponent: 2.0 })
    } else {
        let (w, h) = (sw + 2.0 * side, sh + 2.0 * side);
        ([w, h, t], Corner { radius: screen.corner_radius.max(2.0) + side, exponent: 2.0 })
    };
    let [w, h, _] = size;
    let dome = 0.6 * w.min(h);
    let bump = Bump { center: [0.0, 0.0], size: [dome, dome], corner: Corner { radius: dome / 2.0, exponent: 2.0 }, height: 0.8 };
    let buttons = if g.shape == Shape::Round {
        Vec::new()
    } else {
        vec![Button { name: "button", side: Side::Right, at: -0.2 * h, length: 0.15 * h, depth: 0.6, width: 2.6 }]
    };
    let crown = Crown { side: Side::Right, at: if g.shape == Shape::Round { 0.0 } else { 0.1 * h }, diameter: 5.0, length: 2.5 };
    Some(Body {
        id: g.id.to_owned(),
        aliases: Vec::new(),
        name: g.name.clone(),
        platform: g.platform,
        form_factor: g.form_factor,
        source: Source::Generic(GENERIC),
        size,
        corner,
        bevel,
        screens: vec![screen],
        bump: Some(bump),
        buttons,
        crown: Some(crown),
        fold: None,
        extras: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::fingerprint;
    use super::*;
    use crate::form_factor::Kind;

    /// Every watch body as it was before the watch code moved out of `table.rs` (#500): the same
    /// fields and the same mesh, bit for bit. Straps and new rows change these on purpose.
    #[test]
    fn watch_bodies_are_unchanged_by_the_move() {
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
        let before: [(&str, u64); 12] = [
            ("Apple-Watch-Series-10-46mm", 0xe562c72b9d79a29c),
            ("Apple-Watch-Series-10-42mm", 0xb306a051b2cb6d73),
            ("Apple-Watch-Series-11-46mm", 0xcd3c0316e0beccda),
            ("Apple-Watch-Series-11-42mm", 0x9356f601233624a1),
            ("Apple-Watch-Ultra-2-49mm", 0xf08f243fb73a5e87),
            ("Apple-Watch-SE-44mm-2nd-generation", 0xa2f814e29e207184),
            ("Apple-Watch-SE-40mm-2nd-generation", 0x6f20b32051cc0331),
            ("android-watch-454x454-320dpi-round", 0x9ef09c3b96fb12c5),
            ("android-watch-454x454-320dpi-rectangle", 0xd7ccac733aed1cb7),
            ("android-watch-454x454-320dpi-rounded", 0xed710dcb1f73e0c6),
            ("android-watch-384x384-320dpi-round", 0xdaa6e6f96d365ea6),
            ("ios-watch-416x496-2x-rounded", 0x3f292bb57dc004e2),
        ];
        let got: Vec<(&str, u64)> = bodies.iter().map(|b| (b.id.as_str(), fingerprint(b))).collect();
        assert_eq!(got, before);
    }
}
