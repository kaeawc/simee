//! The body table: one row per device type, with published outer dimensions and display sizes
//! where the maker publishes them, and generic bodies derived from simee's own screens otherwise.
//!
//! Corner radii, camera bumps and button positions are not published by anyone; they are
//! estimates, and each row's source note says so. No row carries a logo or a vendor asset.
//!
//! Phones, tablets and foldables live here. Each other form factor has a module of its own
//! ([`watch`], [`tv`], [`car`], [`headset`]) with the same three items, which this table and
//! [`super::has_body`] ask and nothing else needs to know about:
//! - `ENABLED: bool`: the form factor has bodies;
//! - `rows() -> Vec<Body>`: its rows of the table (published devices);
//! - `generic(&Spec) -> Option<Body>`: a body for a screen no row has.

use super::*;
use super::{car, headset, tv, watch};
use crate::duo;
use crate::fold;
use crate::form_factor::Kind;

const APPLE: &str =
    "Apple tech specs: outer size, display diagonal and pixel density; corner radii, camera bump and button positions estimated";
const PIXEL_FOLD: &str = "Google Pixel Fold tech specs: unfolded size, both displays' diagonals and pixel densities; corner radii, camera bar and buttons estimated";
const SURFACE_DUO: &str =
    "Microsoft Surface Duo tech specs: open size, display diagonal and pixel density; corner radii and buttons estimated";
pub(super) const GENERIC: &str =
    "no published dimensions: derived from the screen simee reports (its pixels at its density), with generic bezels";
const IPHONE_DUO: &str = "no published dimensions: derived from its two screens as the simulator reports them (duo.rs) at an assumed 460 ppi, with generic bezels";

/// The screen outline simee's model gives a kind.
pub(super) fn reference_shape(platform: Platform, form_factor: FormFactor) -> Shape {
    Kind::new(platform, form_factor).map(|k| k.profile().shape).unwrap_or(Shape::Rounded)
}

pub(super) fn rows() -> Vec<Body> {
    let mut out: Vec<Body> = IPHONES.iter().map(iphone).collect();
    out.extend(IPADS.iter().map(ipad));
    if watch::ENABLED {
        out.extend(watch::rows());
    }
    out.push(pixel_fold());
    out.push(surface_duo());
    out.push(iphone_duo());
    if tv::ENABLED {
        out.extend(tv::rows());
    }
    if car::ENABLED {
        out.extend(car::rows());
    }
    if headset::ENABLED {
        out.extend(headset::rows());
    }
    // The AVD form factors simee creates, from their reference profiles: every one with a body.
    for f in FormFactor::ALL.into_iter().filter(|f| has_body(*f)) {
        let Ok(k) = Kind::new(Platform::Android, f) else { continue };
        let p = k.profile();
        let folded = p.fold.map(|f| (f.folded_width, f.folded_height));
        let hinge = HingeSpec::default();
        let id = generic_id(Platform::Android, f, (p.width, p.height), p.density, p.shape, folded, hinge);
        out.extend(generic(&id, Platform::Android, f, (p.width, p.height), p.density, p.shape, folded, hinge));
    }
    out
}

// --- iPhone ---

struct Phone {
    id: &'static str,
    name: &'static str,
    /// Width, height, depth, mm.
    size: [f32; 3],
    px: (u32, u32),
    ppi: f32,
    inches: f32,
    screen_radius: f32,
    cutout: Option<CutoutKind>,
    /// Size, corner radius and height of the camera bump; `bar` runs it across the top.
    bump: ([f32; 2], f32, f32),
    bar: bool,
    /// The Action button (else the ring/silent switch).
    action: bool,
    camera_control: bool,
}

const fn phone(id: &'static str, name: &'static str, size: [f32; 3], px: (u32, u32), ppi: f32, inches: f32) -> Phone {
    Phone {
        id,
        name,
        size,
        px,
        ppi,
        inches,
        screen_radius: 10.3,
        cutout: Some(CutoutKind::Island),
        bump: ([19.5, 35.0], 9.75, 1.8),
        bar: false,
        action: true,
        camera_control: true,
    }
}

const IPHONES: [Phone; 10] = [
    Phone {
        bump: ([0.0, 40.0], 8.0, 1.6),
        bar: true,
        ..phone("iPhone-17-Pro", "iPhone 17 Pro", [71.9, 150.0, 8.75], (1206, 2622), 460.0, 6.3)
    },
    Phone {
        bump: ([0.0, 43.0], 8.0, 1.6),
        bar: true,
        ..phone("iPhone-17-Pro-Max", "iPhone 17 Pro Max", [78.0, 163.4, 8.75], (1320, 2868), 460.0, 6.9)
    },
    phone("iPhone-17", "iPhone 17", [71.5, 149.6, 7.95], (1206, 2622), 460.0, 6.3),
    Phone { bump: ([0.0, 22.0], 8.0, 2.6), bar: true, ..phone("iPhone-Air", "iPhone Air", [74.7, 156.2, 5.64], (1260, 2736), 460.0, 6.5) },
    Phone {
        screen_radius: 7.8,
        cutout: Some(CutoutKind::Notch),
        bump: ([14.0, 14.0], 4.5, 1.4),
        camera_control: false,
        ..phone("iPhone-16e", "iPhone 16e", [71.5, 146.7, 7.8], (1170, 2532), 460.0, 6.1)
    },
    Phone { screen_radius: 9.1, ..phone("iPhone-16", "iPhone 16", [71.6, 147.6, 7.8], (1179, 2556), 460.0, 6.1) },
    Phone { screen_radius: 9.1, ..phone("iPhone-16-Plus", "iPhone 16 Plus", [77.8, 160.9, 7.8], (1290, 2796), 460.0, 6.7) },
    Phone { bump: ([37.0, 37.0], 9.5, 2.4), ..phone("iPhone-16-Pro", "iPhone 16 Pro", [71.5, 149.6, 8.25], (1206, 2622), 460.0, 6.3) },
    Phone {
        bump: ([39.5, 39.5], 9.5, 2.4),
        ..phone("iPhone-16-Pro-Max", "iPhone 16 Pro Max", [77.6, 163.0, 8.25], (1320, 2868), 460.0, 6.9)
    },
    Phone {
        screen_radius: 0.0,
        cutout: None,
        bump: ([9.5, 9.5], 4.75, 0.9),
        action: false,
        camera_control: false,
        ..phone("iPhone-SE-3rd-generation", "iPhone SE (3rd generation)", [67.3, 138.4, 7.3], (750, 1334), 326.0, 4.7)
    },
];

fn iphone(p: &Phone) -> Body {
    let [w, h, t] = p.size;
    let mut screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px: p.px,
        ppi: p.ppi,
        center: [0.0, 0.0],
        // simee models every iPhone screen as rounded; the SE's has square corners.
        shape: if p.screen_radius > 0.0 { reference_shape(Platform::Ios, FormFactor::Phone) } else { Shape::Rectangle },
        corner_radius: p.screen_radius,
        cutout: None,
        published: Some(Published::Diagonal(p.inches)),
    };
    let [sw, sh] = screen.size();
    let top = sh / 2.0;
    screen.cutout = p.cutout.map(|kind| match kind {
        // The Dynamic Island: about 126x37 points, 11 points down.
        CutoutKind::Island => Cutout { kind, center: [0.0, top - 1.8 - 3.1], size: [20.9, 6.2] },
        CutoutKind::Notch | CutoutKind::PunchHole => Cutout { kind, center: [0.0, top - 2.7], size: [26.0, 5.4] },
    });
    let bezel = ((w - sw) / 2.0).min((h - sh) / 2.0);
    let ([bw, bh], br, height) = p.bump;
    let bump = if p.bar {
        let bw = w - 5.0;
        Bump { center: [0.0, h / 2.0 - 2.5 - bh / 2.0], size: [bw, bh], corner: Corner { radius: br, exponent: 3.0 }, height }
    } else {
        // Top left as you look at the back: top right from the front.
        let round = (br - bw.min(bh) / 2.0).abs() < 0.01;
        Bump {
            center: [w / 2.0 - 3.0 - bw / 2.0, h / 2.0 - 3.0 - bh / 2.0],
            size: [bw, bh],
            corner: Corner { radius: br, exponent: if round { 2.0 } else { 3.0 } },
            height,
        }
    };
    let side = |name, side, from_top: f32, length| Button { name, side, at: h / 2.0 - from_top, length, depth: 0.6, width: 2.4 };
    let mut buttons = vec![
        side(if p.action { "action" } else { "ring-silent" }, Side::Left, 30.0, 7.0),
        side("volume-up", Side::Left, 44.0, 10.0),
        side("volume-down", Side::Left, 57.0, 10.0),
        side("side", Side::Right, 48.0, 16.0),
    ];
    if p.camera_control {
        buttons.push(Button { depth: 0.2, ..side("camera-control", Side::Right, h - 52.0, 13.0) });
    }
    Body {
        id: p.id.to_owned(),
        aliases: Vec::new(),
        name: p.name.to_owned(),
        platform: Platform::Ios,
        form_factor: FormFactor::Phone,
        source: Source::Published(APPLE),
        size: [w, h, t],
        corner: Corner { radius: p.screen_radius.max(7.0) + bezel, exponent: 3.0 },
        bevel: 0.8,
        screens: vec![screen],
        bump: Some(bump),
        buttons,
        crown: None,
        fold: None,
        extras: Vec::new(),
    }
}

// --- iPad ---

struct Tablet {
    id: &'static str,
    aliases: &'static [&'static str],
    name: &'static str,
    size: [f32; 3],
    px: (u32, u32),
    ppi: f32,
    inches: f32,
    /// Camera bump size and height (a round bump when square).
    bump: ([f32; 2], f32),
    /// The volume keys sit on the top edge beside the top button (else on the right side).
    volume_on_top: bool,
}

const IPADS: [Tablet; 6] = [
    Tablet {
        id: "iPad-Pro-11-inch-M4",
        aliases: &["iPad-Pro-11-inch-M4-8GB", "iPad-Pro-11-inch-M4-16GB"],
        name: "iPad Pro 11-inch (M4)",
        size: [177.5, 249.7, 5.3],
        px: (1668, 2420),
        ppi: 264.0,
        inches: 11.0,
        bump: ([18.5, 30.0], 1.1),
        volume_on_top: false,
    },
    Tablet {
        id: "iPad-Pro-13-inch-M4",
        aliases: &["iPad-Pro-13-inch-M4-8GB", "iPad-Pro-13-inch-M4-16GB"],
        name: "iPad Pro 13-inch (M4)",
        size: [215.5, 281.6, 5.1],
        px: (2064, 2752),
        ppi: 264.0,
        inches: 13.0,
        bump: ([18.5, 30.0], 1.1),
        volume_on_top: false,
    },
    Tablet {
        id: "iPad-Air-11-inch-M3",
        aliases: &["iPad-Air-11-inch-M2"],
        name: "iPad Air 11-inch (M3)",
        size: [178.5, 247.6, 6.1],
        px: (1640, 2360),
        ppi: 264.0,
        inches: 11.0,
        bump: ([9.0, 9.0], 0.8),
        volume_on_top: true,
    },
    Tablet {
        id: "iPad-Air-13-inch-M3",
        aliases: &["iPad-Air-13-inch-M2"],
        name: "iPad Air 13-inch (M3)",
        size: [214.9, 280.6, 6.1],
        px: (2048, 2732),
        ppi: 264.0,
        inches: 13.0,
        bump: ([9.0, 9.0], 0.8),
        volume_on_top: true,
    },
    Tablet {
        id: "iPad-mini-A17-Pro",
        aliases: &[],
        name: "iPad mini (A17 Pro)",
        size: [134.8, 195.4, 6.3],
        px: (1488, 2266),
        ppi: 326.0,
        inches: 8.3,
        bump: ([9.5, 9.5], 0.8),
        volume_on_top: true,
    },
    Tablet {
        id: "iPad-A16",
        aliases: &[],
        name: "iPad (A16)",
        size: [179.5, 248.6, 7.0],
        px: (1640, 2360),
        ppi: 264.0,
        inches: 11.0,
        bump: ([9.0, 9.0], 0.8),
        volume_on_top: true,
    },
];

fn ipad(p: &Tablet) -> Body {
    let [w, h, t] = p.size;
    let screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px: p.px,
        ppi: p.ppi,
        center: [0.0, 0.0],
        shape: reference_shape(Platform::Ios, FormFactor::Tablet),
        corner_radius: 3.5,
        cutout: None,
        published: Some(Published::Diagonal(p.inches)),
    };
    let [sw, sh] = screen.size();
    let bezel = ((w - sw) / 2.0).min((h - sh) / 2.0);
    let ([bw, bh], height) = p.bump;
    let round = (bw - bh).abs() < 0.01;
    let bump = Bump {
        center: [w / 2.0 - 6.0 - bw / 2.0, h / 2.0 - 6.0 - bh / 2.0],
        size: [bw, bh],
        corner: if round { Corner { radius: bw / 2.0, exponent: 2.0 } } else { Corner { radius: 7.0, exponent: 3.0 } },
        height,
    };
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.5, width: 2.2 };
    let mut buttons = vec![key("top", Side::Top, w / 2.0 - 22.0, 15.0)];
    if p.volume_on_top {
        buttons.push(key("volume-up", Side::Top, -w / 2.0 + 48.0, 12.0));
        buttons.push(key("volume-down", Side::Top, -w / 2.0 + 32.0, 12.0));
    } else {
        buttons.push(key("volume-up", Side::Right, h / 2.0 - 30.0, 12.0));
        buttons.push(key("volume-down", Side::Right, h / 2.0 - 46.0, 12.0));
    }
    Body {
        id: p.id.to_owned(),
        aliases: p.aliases.to_vec(),
        name: p.name.to_owned(),
        platform: Platform::Ios,
        form_factor: FormFactor::Tablet,
        source: Source::Published(APPLE),
        size: [w, h, t],
        corner: Corner { radius: screen.corner_radius + bezel, exponent: 3.0 },
        bevel: 0.8,
        screens: vec![screen],
        bump: Some(bump),
        buttons,
        crown: None,
        fold: None,
        extras: Vec::new(),
    }
}

// --- Foldables ---

/// How a foldable's hinge runs, from its AVD's `fold::Hinge`: what a generic body is built to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct HingeSpec {
    /// The fold runs top to bottom (a book), else across (a flip).
    pub vertical: bool,
    /// The angles the hinge turns through, degrees.
    pub range: (f32, f32),
    /// The width of the hinge between two real screens, in pixels of the unfolded display: 0 for
    /// one flexible screen.
    pub gap_px: u32,
}

impl Default for HingeSpec {
    /// What a foldable with no hinge description gets: a book over 0-180° with one flexible screen.
    fn default() -> Self {
        HingeSpec { vertical: true, range: (0.0, HingeAxis::FLAT), gap_px: 0 }
    }
}

impl HingeSpec {
    pub(super) fn from_hinge(h: &fold::Hinge) -> Self {
        let sane = h.range.0.is_finite() && h.range.1.is_finite() && h.range.0 >= 0.0 && h.range.1 <= 360.0 && h.range.0 < h.range.1;
        let range = if sane { h.range } else { Self::default().range };
        // A physical hinge with width leaves a gap between two screens; a fold in one screen has none.
        let gap_px = match (h.physical, h.area) {
            (true, Some(a)) => {
                if h.vertical {
                    a.width
                } else {
                    a.height
                }
            }
            _ => 0,
        };
        HingeSpec { vertical: h.vertical, range, gap_px }
    }

    /// The hinge's axis and the gap between the screens' halves, for a body `size` (width,
    /// height, thickness, mm) whose inner screen is `inner`.
    fn axis(&self, [_, _, t]: [f32; 3], inner: &Screen) -> (HingeAxis, f32) {
        let axis = HingeAxis { vertical: self.vertical, at: 0.0, range: self.range, fold_in_z: t / 2.0, fold_out_z: -t / 2.0 };
        let [iw, ih] = inner.size();
        let gap = self.gap_px as f32 * MM_PER_INCH / inner.ppi;
        // A hinge wider than a quarter of the screen is a bad description, not a device.
        (axis, gap.min(if self.vertical { iw } else { ih } / 4.0))
    }
}

/// A two-panel body folding about a hinge through its centre, the inner screen across both on the
/// front, the outer screen (if any) on the back: of panel 0 for a book (a vertical hinge, panels
/// left and right), of the lid (panel 1, on top) for a flip (a horizontal hinge).
#[allow(clippy::too_many_arguments)]
fn book(
    id: &str,
    name: &str,
    platform: Platform,
    source: Source,
    size: [f32; 3],
    corner: Corner,
    bevel: f32,
    inner: Screen,
    outer: Option<Screen>,
    hinge: HingeSpec,
) -> Body {
    let [w, h, _] = size;
    let hinge_corner = bevel + MIN_RADIUS;
    let r = corner.radius;
    let panels = if hinge.vertical {
        let half = w / 2.0;
        [
            Panel { center: [-half / 2.0, 0.0], size: [half, h], radii: [hinge_corner, r, r, hinge_corner] },
            Panel { center: [half / 2.0, 0.0], size: [half, h], radii: [r, hinge_corner, hinge_corner, r] },
        ]
    } else {
        let half = h / 2.0;
        [
            Panel { center: [0.0, -half / 2.0], size: [w, half], radii: [hinge_corner, hinge_corner, r, r] },
            Panel { center: [0.0, half / 2.0], size: [w, half], radii: [r, r, hinge_corner, hinge_corner] },
        ]
    };
    let (axis, screen_gap) = hinge.axis(size, &inner);
    let mut screens = vec![inner];
    screens.extend(outer);
    Body {
        id: id.to_owned(),
        aliases: Vec::new(),
        name: name.to_owned(),
        platform,
        form_factor: FormFactor::Foldable,
        source,
        size,
        corner,
        bevel,
        screens,
        bump: None,
        buttons: Vec::new(),
        crown: None,
        fold: Some(FoldBody { panels, hinge: axis, screen_gap }),
        extras: Vec::new(),
    }
}

fn foldable_screen(display: Display, face: Face, px: (u32, u32), ppi: f32, center: [f32; 2], shape: Shape, corner_radius: f32) -> Screen {
    Screen { display, face, px, ppi, center, shape, corner_radius, cutout: None, published: None }
}

fn pixel_fold() -> Body {
    let d = fold::pixel_fold();
    let folded = d.hinge.folded.map_or((1080, 2092), |r| (r.width, r.height));
    let (w, h, t) = (158.7, 139.7, 5.8);
    let shape = reference_shape(Platform::Android, FormFactor::Foldable);
    let inner = Screen {
        published: Some(Published::Diagonal(7.6)),
        ..foldable_screen(Display::Inner, Face::Front, (d.lcd.0, d.lcd.1), 380.0, [0.0, 0.0], shape, 3.5)
    };
    let mut outer = Screen {
        published: Some(Published::Diagonal(5.8)),
        ..foldable_screen(Display::Outer, Face::Back, folded, 408.0, [-w / 4.0, 0.0], Shape::Rounded, 5.5)
    };
    outer.cutout = Some(Cutout { kind: CutoutKind::PunchHole, center: [0.0, outer.size()[1] / 2.0 - 4.0], size: [3.2, 3.2] });
    let mut b = book(
        d.id,
        d.name,
        Platform::Android,
        Source::Published(PIXEL_FOLD),
        [w, h, t],
        Corner { radius: 9.0, exponent: 2.0 },
        0.7,
        inner,
        Some(outer),
        HingeSpec::from_hinge(&d.hinge),
    );
    // The camera bar across the top of the other panel's back.
    b.bump = Some(Bump {
        center: [w / 4.0, h / 2.0 - 12.0],
        size: [w / 2.0 - 8.0, 16.0],
        corner: Corner { radius: 8.0, exponent: 2.0 },
        height: 2.6,
    });
    let key = |name, at, length| Button { name, side: Side::Right, at, length, depth: 0.5, width: 2.4 };
    b.buttons = vec![key("power", h / 2.0 - 28.0, 14.0), key("volume", h / 2.0 - 50.0, 22.0)];
    b
}

fn surface_duo() -> Body {
    let d = fold::dual_screen();
    let (w, h, t) = (186.9, 145.2, 4.8);
    let shape = reference_shape(Platform::Android, FormFactor::Foldable);
    // simee draws the two 1350x1800 screens and the hinge between them as one display; the body
    // has two screens with the hinge's 84 px between them.
    let inner = Screen {
        published: Some(Published::Diagonal(8.1)),
        ..foldable_screen(Display::Inner, Face::Front, (d.lcd.0, d.lcd.1), 401.0, [0.0, 0.0], shape, 1.0)
    };
    let mut b = book(
        d.id,
        d.name,
        Platform::Android,
        Source::Published(SURFACE_DUO),
        [w, h, t],
        Corner { radius: 6.0, exponent: 2.0 },
        0.6,
        inner,
        None,
        HingeSpec::from_hinge(&d.hinge),
    );
    let key = |name, at, length| Button { name, side: Side::Right, at, length, depth: 0.4, width: 2.0 };
    b.buttons = vec![key("power", h / 2.0 - 30.0, 12.0), key("volume", h / 2.0 - 52.0, 20.0)];
    b
}

/// A generic book fold around an inner screen and an optional outer one, both at `ppi`.
#[allow(clippy::too_many_arguments)]
fn generic_book(
    id: &str,
    name: &str,
    platform: Platform,
    note: &'static str,
    inner: (u32, u32),
    outer: Option<(u32, u32)>,
    ppi: f32,
    shape: Shape,
    hinge: HingeSpec,
    exponent: f32,
) -> Body {
    let (side, top, t, bevel) = (2.5, 4.0, 6.0, 0.6);
    let radius = |shape| if shape == Shape::Rounded { 4.0 } else { 0.0 };
    let inner = foldable_screen(Display::Inner, Face::Front, inner, ppi, [0.0, 0.0], shape, radius(shape));
    let [iw, ih] = inner.size();
    let outer_screen = |center| outer.map(|px| foldable_screen(Display::Outer, Face::Back, px, ppi, center, Shape::Rounded, 4.0));
    let outer_size = outer_screen([0.0, 0.0]).map(|s| s.size());
    let corner = Corner { radius: 7.0, exponent };
    let note = Source::Generic(note);
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.5, width: 2.4 };
    if !hinge.vertical {
        // A flip: the screen's top half is the lid, with the cover screen and the camera on its
        // back, side by side along its top.
        let panel = outer_size.map_or(ih / 2.0 + top, |[_, oh]| (ih / 2.0 + top).max(oh + 2.0 * top));
        let w = iw.max(outer_size.map_or(0.0, |[ow, _]| ow)) + 2.0 * side;
        let h = 2.0 * panel;
        let row = panel - top - outer_size.map_or(12.0, |[_, oh]| oh) / 2.0;
        let outer = outer_screen([-w / 2.0 + side + outer_size.map_or(0.0, |[ow, _]| ow) / 2.0, row]);
        let mut b = book(id, name, platform, note, [w, h, t], corner, bevel, inner, outer, hinge);
        let free = w - 2.0 * side - outer_size.map_or(0.0, |[ow, _]| ow + 2.0);
        if free >= 8.0 {
            let width = free.min(22.0);
            b.bump = Some(Bump {
                center: [w / 2.0 - side - width / 2.0, row],
                size: [width, 12.0],
                corner: Corner { radius: 4.0, exponent: 2.0 },
                height: 1.2,
            });
        }
        b.buttons = vec![key("power", Side::Right, 0.3 * h, 12.0), key("volume", Side::Right, 0.1 * h, 20.0)];
        return b;
    }
    let panel = outer_size.map_or(iw / 2.0 + side, |[ow, _]| (iw / 2.0 + side).max(ow + 2.0 * side));
    let h = ih.max(outer_size.map_or(0.0, |[_, oh]| oh)) + 2.0 * top;
    let mut b = book(id, name, platform, note, [2.0 * panel, h, t], corner, bevel, inner, outer_screen([-panel / 2.0, 0.0]), hinge);
    b.bump = Some(Bump {
        center: [panel / 2.0, h / 2.0 - 10.0],
        size: [panel - 10.0, 12.0],
        corner: Corner { radius: 6.0, exponent: 2.0 },
        height: 1.6,
    });
    b.buttons = vec![key("power", Side::Right, h / 2.0 - 0.2 * h, 12.0), key("volume", Side::Right, h / 2.0 - 0.4 * h, 20.0)];
    b
}

fn iphone_duo() -> Body {
    // The inner screen as the simulator stores it is 2007x2853, a landscape panel stored portrait;
    // the body carries it the way it lies unfolded, 2853x2007 (see [`Screen`] for how a frame
    // goes onto it). One panel across the hinge, so one face.
    generic_book(
        "iPhone-Duo",
        "iPhone Duo",
        Platform::Ios,
        IPHONE_DUO,
        (duo::INNER_PX.1, duo::INNER_PX.0),
        Some(duo::OUTER_PX),
        460.0,
        Shape::Rounded,
        HingeSpec { range: (duo::CLOSED as f32, duo::OPEN as f32), ..HingeSpec::default() },
        3.0,
    )
}

// --- Generic bodies ---

/// The density a generic body assumes for an Apple screen, which reports only its scale.
fn assumed_ppi(form_factor: FormFactor, scale: u32) -> f32 {
    match (form_factor, scale) {
        (FormFactor::Tablet, _) => 264.0,
        (_, 3..) => 460.0,
        (_, 2) => 326.0,
        _ => 163.0,
    }
}

/// A generic body's key: everything it is built from.
pub(super) fn generic_id(
    platform: Platform,
    form_factor: FormFactor,
    (w, h): (u32, u32),
    density: Density,
    shape: Shape,
    folded: Option<(u32, u32)>,
    hinge: HingeSpec,
) -> String {
    let density = match density {
        Density::Dpi(d) => format!("{d}dpi"),
        Density::Scale(s) => format!("{s}x"),
    };
    let shape = match shape {
        Shape::Rectangle => "rectangle",
        Shape::Rounded => "rounded",
        Shape::Round => "round",
    };
    let folded = folded.map(|(fw, fh)| format!("-folded-{fw}x{fh}")).unwrap_or_default();
    // The default hinge (a book over 0-180° with one flexible screen) has no part in the key.
    let hinge = if hinge == HingeSpec::default() {
        String::new()
    } else {
        let (lo, hi) = hinge.range;
        format!("-{}-hinge-{lo}-{hi}-gap{}", if hinge.vertical { "vertical" } else { "horizontal" }, hinge.gap_px)
    };
    format!("{}-{}-{w}x{h}-{density}-{shape}{folded}{hinge}", platform.as_str(), form_factor.as_str())
}

/// What a generic body is built from: the screen simee reports, at its density. A form factor's
/// module builds its generic body from this ([`watch::generic`], [`tv::generic`],
/// [`car::generic`], [`headset::generic`]).
pub(super) struct Spec<'a> {
    /// The body's id ([`generic_id`]): the table's key for it.
    pub id: &'a str,
    /// `Generic <platform> <form factor> (<w>x<h>)`.
    pub name: String,
    pub platform: Platform,
    pub form_factor: FormFactor,
    /// The screen, pixels, in the body's natural orientation.
    pub px: (u32, u32),
    /// Pixels per inch: the dpi simee reports, or for an Apple scale, the density assumed for it
    /// ([`assumed_ppi`]).
    pub ppi: f32,
    pub shape: Shape,
}

impl Spec<'_> {
    /// The main screen on the front face, centred: a round screen's corners are its radius, a
    /// rounded one's `radius`, a rectangle's square.
    pub fn screen(&self, radius: f32) -> Screen {
        let mut screen = Screen {
            display: Display::Main,
            face: Face::Front,
            px: self.px,
            ppi: self.ppi,
            center: [0.0, 0.0],
            shape: self.shape,
            corner_radius: 0.0,
            cutout: None,
            published: None,
        };
        let [sw, sh] = screen.size();
        screen.corner_radius = match self.shape {
            Shape::Round => sw.min(sh) / 2.0,
            Shape::Rounded => radius,
            Shape::Rectangle => 0.0,
        };
        screen
    }
}

/// A body for a screen with no published dimensions: the screen at its density inside generic
/// bezels for its form factor, from the form factor's module where it has one. None for a form
/// factor with no body.
#[allow(clippy::too_many_arguments)]
pub(super) fn generic(
    id: &str,
    platform: Platform,
    form_factor: FormFactor,
    px: (u32, u32),
    density: Density,
    shape: Shape,
    folded: Option<(u32, u32)>,
    hinge: HingeSpec,
) -> Option<Body> {
    let ppi = match density {
        Density::Dpi(d) => d.max(1) as f32,
        Density::Scale(s) => assumed_ppi(form_factor, s),
    };
    let name = format!("Generic {} {} ({}x{})", platform.label(), form_factor.as_str(), px.0, px.1);
    let spec = Spec { id, name, platform, form_factor, px, ppi, shape };
    match form_factor {
        FormFactor::Foldable => Some(generic_book(id, &spec.name, platform, GENERIC, px, folded, ppi, shape, hinge, 2.0)),
        FormFactor::Phone | FormFactor::Tablet => Some(generic_slab(&spec)),
        FormFactor::Watch => watch::generic(&spec),
        FormFactor::Tv => tv::generic(&spec),
        FormFactor::Auto => car::generic(&spec),
        FormFactor::Vision => headset::generic(&spec),
    }
}

/// A generic phone or tablet: the screen in even bezels, a camera bump and two keys.
fn generic_slab(g: &Spec) -> Body {
    // Side bezel, top bezel, thickness, screen corner radius, edge bevel.
    let (side, top, t, radius, bevel) = match g.form_factor {
        FormFactor::Tablet => (8.0, 8.0, 6.5, 5.0, 0.8),
        _ => (3.0, 4.5, 8.5, 4.0, 0.8),
    };
    let screen = g.screen(radius);
    let [sw, sh] = screen.size();
    let (size, corner) = if g.shape == Shape::Round {
        let d = sw.max(sh) + 2.0 * side;
        ([d, d, t], Corner { radius: d / 2.0, exponent: 2.0 })
    } else {
        let (w, h) = (sw + 2.0 * side, sh + 2.0 * top);
        ([w, h, t], Corner { radius: screen.corner_radius.max(2.0) + side, exponent: 2.0 })
    };
    let [w, h, _] = size;
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.6, width: 2.6 };
    let (bump, buttons) = match g.form_factor {
        FormFactor::Tablet => {
            let bump = Bump {
                center: [w / 2.0 - 12.0, h / 2.0 - 12.0],
                size: [12.0, 12.0],
                corner: Corner { radius: 6.0, exponent: 2.0 },
                height: 0.8,
            };
            (bump, vec![key("power", Side::Top, -w / 2.0 + 30.0, 14.0), key("volume", Side::Top, w / 2.0 - 40.0, 25.0)])
        }
        _ => {
            let bump = Bump {
                center: [w / 2.0 - 15.0, h / 2.0 - 10.0],
                size: [22.0, 12.0],
                corner: Corner { radius: 4.0, exponent: 2.0 },
                height: 1.2,
            };
            (bump, vec![key("power", Side::Right, 0.25 * h, 12.0), key("volume", Side::Right, 0.05 * h, 24.0)])
        }
    };
    Body {
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
        crown: None,
        fold: None,
        extras: Vec::new(),
    }
}
