//! The body table: one row per device type, with published outer dimensions and display sizes
//! where the maker publishes them, and generic bodies derived from simee's own screens otherwise.
//!
//! Corner radii, camera housings, lens sizes and button positions are not published by anyone;
//! they are estimates, and each row's source note says so. A row's finish ([`Finish`]) is a
//! material and a colour close to one the maker sells. No row carries a logo or a vendor asset.
//!
//! Phones, tablets and foldables live here. Each other form factor has a module of its own
//! ([`watch`], [`tv`], [`car`], [`headset`]) with the same four items, which this table and
//! [`super::has_body`] ask and nothing else needs to know about:
//! - `ENABLED: bool`: the form factor has bodies;
//! - `rows() -> Vec<Body>`: its rows of the table (published devices);
//! - `generic(&Spec) -> Option<Body>`: a body for a screen no row has;
//! - `SIZES_BY_DPI: bool`: its generic body's screen is drawn at the dpi simee reports (the shared
//!   reference-body test checks that size only when true; a TV or headset sized another way says false).

use super::*;
use super::{car, headset, tv, watch};
use crate::duo;
use crate::fold;
use crate::form_factor::Kind;

const APPLE: &str = "Apple tech specs (apple.com/iphone/compare, apple.com/ipad/compare): outer size, display diagonal and pixel density; corner radii, frame edge, camera housing, lenses and button positions estimated";
const PIXEL: &str = "Google Pixel tech specs (store.google.com, each phone's Tech Specs): outer size, display diagonal, resolution and pixel density; corner radii, frame edge, camera bar, lenses, punch hole and button positions estimated";
const PIXEL_TABLET: &str = "Google Pixel Tablet tech specs (store.google.com): outer size, display diagonal, resolution and pixel density; corner radii, camera and button positions estimated";
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
    out.extend(PIXELS.iter().map(pixel));
    out.extend(IPADS.iter().map(ipad));
    out.push(pixel_tablet());
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
        out.extend(generic(&id, Platform::Android, f, (p.width, p.height), p.density, p.shape, folded, hinge, ""));
    }
    out
}

// --- Finishes ---

const fn finish(frame: Surface, frame_colour: [f32; 3], back: Surface, back_colour: [f32; 3]) -> Finish {
    Finish::new(frame, frame_colour, back, back_colour)
}

/// Silver anodized aluminium, back and band.
const SILVER: Finish = finish(Surface::Aluminium, [0.74, 0.74, 0.75], Surface::Aluminium, [0.74, 0.74, 0.75]);
/// Dark anodized aluminium, back and band.
const SPACE_BLACK: Finish = finish(Surface::Aluminium, [0.055, 0.055, 0.06], Surface::Aluminium, [0.055, 0.055, 0.06]);
/// A warm white glass back in a pale satin band.
const PORCELAIN: Finish = finish(Surface::Aluminium, [0.62, 0.58, 0.52], Surface::Glass, [0.72, 0.69, 0.63]);
/// A black glass back in a dark band.
const OBSIDIAN: Finish = finish(Surface::Aluminium, [0.06, 0.06, 0.065], Surface::Glass, [0.016, 0.017, 0.02]);

// --- Camera parts ---

/// A camera lens on the back: a ring of the housing's metal standing `rim` proud of `z` (the face
/// it sits on), and the lens glass inside it.
fn lens(at: [f32; 2], z: f32, diameter: f32, rim: f32) -> [Block; 2] {
    [Block::on_back(Role::Bump, at, z, diameter, rim, 0.3), Block::on_back(Role::Camera, at, z - rim, diameter - 2.4, 0.1, 0.3)]
}

/// A flash or sensor window lying flush on the back at `z`.
fn flash(at: [f32; 2], z: f32, diameter: f32) -> Block {
    Block::on_back(Role::Flash, at, z, diameter, 0.08, 0.3)
}

// --- iPhone ---

/// How an iPhone's cameras are housed.
#[derive(Clone, Copy)]
enum Housing {
    /// A plateau across the back's top, `0` mm tall, with `1` lenses at its end (iPhone 17 Pro,
    /// iPhone Air).
    Plateau(f32, u8),
    /// A square island with three lenses (iPhone 16 Pro), its side mm.
    Square(f32),
    /// An upright pill with two lenses (iPhone 16, 17): width and height, mm.
    Pill([f32; 2]),
    /// One lens in a round bump of this diameter, mm.
    Single(f32),
}

struct Phone {
    id: &'static str,
    name: &'static str,
    /// Width, height, depth, mm.
    size: [f32; 3],
    px: (u32, u32),
    ppi: f32,
    inches: f32,
    /// The display's corner radius as a circular corner reads, mm (drawn continuous).
    screen_radius: f32,
    cutout: Option<CutoutKind>,
    housing: Housing,
    /// How far the housing stands proud of the back, mm.
    housing_height: f32,
    /// The radius the band's edges are rounded over, mm.
    edge: f32,
    /// The Action button (else the ring/silent switch).
    action: bool,
    camera_control: bool,
    finish: Finish,
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
        housing: Housing::Pill([16.5, 35.0]),
        housing_height: 1.9,
        edge: 1.3,
        action: true,
        camera_control: true,
        finish: SILVER,
    }
}

const IPHONES: [Phone; 10] = [
    // Cosmic orange: an aluminium unibody, the plateau cut from it.
    Phone {
        housing: Housing::Plateau(38.0, 3),
        housing_height: 2.0,
        edge: 1.4,
        finish: finish(Surface::Aluminium, [0.80, 0.22, 0.045], Surface::Aluminium, [0.80, 0.22, 0.045]),
        ..phone("iPhone-17-Pro", "iPhone 17 Pro", [71.9, 150.0, 8.75], (1206, 2622), 460.0, 6.3)
    },
    Phone {
        housing: Housing::Plateau(40.0, 3),
        housing_height: 2.0,
        edge: 1.4,
        ..phone("iPhone-17-Pro-Max", "iPhone 17 Pro Max", [78.0, 163.4, 8.75], (1320, 2868), 460.0, 6.9)
    },
    // Mist blue: colour-infused matte glass in an aluminium band.
    Phone {
        finish: finish(Surface::Aluminium, [0.36, 0.45, 0.60], Surface::MatteGlass, [0.30, 0.40, 0.58]),
        ..phone("iPhone-17", "iPhone 17", [71.5, 149.6, 7.95], (1206, 2622), 460.0, 6.3)
    },
    // Cloud white: a glass back in a polished titanium band.
    Phone {
        housing: Housing::Plateau(21.0, 1),
        housing_height: 2.6,
        edge: 1.5,
        finish: finish(Surface::Polished, [0.78, 0.78, 0.77], Surface::Glass, [0.80, 0.80, 0.79]),
        ..phone("iPhone-Air", "iPhone Air", [74.7, 156.2, 5.64], (1260, 2736), 460.0, 6.5)
    },
    Phone {
        screen_radius: 7.8,
        cutout: Some(CutoutKind::Notch),
        housing: Housing::Single(14.0),
        housing_height: 1.4,
        camera_control: false,
        finish: finish(Surface::Aluminium, [0.76, 0.76, 0.76], Surface::MatteGlass, [0.80, 0.80, 0.79]),
        ..phone("iPhone-16e", "iPhone 16e", [71.5, 146.7, 7.8], (1170, 2532), 460.0, 6.1)
    },
    // Ultramarine and teal: colour-infused glass.
    Phone {
        screen_radius: 9.1,
        finish: finish(Surface::Aluminium, [0.22, 0.27, 0.62], Surface::MatteGlass, [0.16, 0.20, 0.60]),
        ..phone("iPhone-16", "iPhone 16", [71.6, 147.6, 7.8], (1179, 2556), 460.0, 6.1)
    },
    Phone {
        screen_radius: 9.1,
        finish: finish(Surface::Aluminium, [0.30, 0.50, 0.48], Surface::MatteGlass, [0.26, 0.48, 0.46]),
        ..phone("iPhone-16-Plus", "iPhone 16 Plus", [77.8, 160.9, 7.8], (1290, 2796), 460.0, 6.7)
    },
    // Natural and desert titanium over matte glass.
    Phone {
        housing: Housing::Square(37.0),
        housing_height: 2.4,
        edge: 1.1,
        finish: finish(Surface::Titanium, [0.47, 0.44, 0.40], Surface::MatteGlass, [0.50, 0.47, 0.43]),
        ..phone("iPhone-16-Pro", "iPhone 16 Pro", [71.5, 149.6, 8.25], (1206, 2622), 460.0, 6.3)
    },
    Phone {
        housing: Housing::Square(39.5),
        housing_height: 2.4,
        edge: 1.1,
        finish: finish(Surface::Titanium, [0.56, 0.45, 0.36], Surface::MatteGlass, [0.60, 0.50, 0.42]),
        ..phone("iPhone-16-Pro-Max", "iPhone 16 Pro Max", [77.6, 163.0, 8.25], (1320, 2868), 460.0, 6.9)
    },
    // Midnight: a glass back in a rounded aluminium band.
    Phone {
        screen_radius: 0.0,
        cutout: None,
        housing: Housing::Single(9.5),
        housing_height: 0.9,
        edge: 2.0,
        action: false,
        camera_control: false,
        finish: finish(Surface::Aluminium, [0.05, 0.055, 0.07], Surface::Glass, [0.014, 0.016, 0.024]),
        ..phone("iPhone-SE-3rd-generation", "iPhone SE (3rd generation)", [67.3, 138.4, 7.3], (750, 1334), 326.0, 4.7)
    },
];

/// The camera housing of an iPhone `w` by `h` by `t`: its bump and the lenses, flash and sensor on
/// it. Everything is placed as seen from the front, so the back's top left (where the lenses are)
/// is at +x.
fn iphone_cameras(p: &Phone) -> (Bump, Vec<Block>) {
    let [w, h, t] = p.size;
    let top = -t / 2.0 - p.housing_height;
    let back = -t / 2.0;
    let mut parts = Vec::new();
    let bump = match p.housing {
        Housing::Plateau(tall, lenses) => {
            let (bw, margin) = (w - 6.4, 3.2);
            let cy = h / 2.0 - margin - tall / 2.0;
            let radius = (p.screen_radius + 2.6 - margin).max(4.0).min(tall / 2.0);
            if lenses >= 3 {
                let d = 0.41 * tall;
                let (x, dy) = (bw / 2.0 - 0.29 * tall, 0.24 * tall);
                parts.extend(lens([x, cy + dy], top, d, 1.0));
                parts.extend(lens([x, cy - dy], top, d, 1.0));
                parts.extend(lens([x - 0.445 * tall, cy], top, d, 1.0));
                parts.push(flash([-bw / 2.0 + 0.26 * tall, cy + dy], top, 6.2));
                parts.push(Block::on_back(Role::Camera, [-bw / 2.0 + 0.26 * tall, cy - dy], top, 5.6, 0.08, 0.3));
            } else {
                parts.extend(lens([bw / 2.0 - tall / 2.0, cy], top, 0.66 * tall, 0.8));
                parts.push(flash([-bw / 2.0 + tall / 2.0, cy], top, 5.6));
            }
            Bump { center: [0.0, cy], size: [bw, tall], corner: Corner::continuous(radius), height: p.housing_height }
        }
        Housing::Square(side) => {
            let c = [w / 2.0 - 3.6 - side / 2.0, h / 2.0 - 3.6 - side / 2.0];
            let (d, q) = (0.41 * side, 0.225 * side);
            parts.extend(lens([c[0] + q, c[1] + q], top, d, 1.0));
            parts.extend(lens([c[0] + q, c[1] - q], top, d, 1.0));
            parts.extend(lens([c[0] - q, c[1]], top, d, 1.0));
            parts.push(flash([c[0] - q, c[1] + 0.3 * side], top, 5.6));
            parts.push(Block::on_back(Role::Camera, [c[0] - q, c[1] - 0.3 * side], top, 5.0, 0.08, 0.3));
            Bump { center: c, size: [side, side], corner: Corner::continuous(0.27 * side), height: p.housing_height }
        }
        Housing::Pill([bw, bh]) => {
            let c = [w / 2.0 - 5.2 - bw / 2.0, h / 2.0 - 5.2 - bh / 2.0];
            let d = bw - 2.6;
            let dy = (bh - bw) / 2.0;
            parts.extend(lens([c[0], c[1] + dy], top, d, 0.8));
            parts.extend(lens([c[0], c[1] - dy], top, d, 0.8));
            parts.push(flash([c[0] - bw / 2.0 - 7.0, c[1]], back, 5.6));
            Bump { center: c, size: [bw, bh], corner: Corner::round(bw / 2.0), height: p.housing_height }
        }
        Housing::Single(d) => {
            let c = [w / 2.0 - 5.5 - d / 2.0, h / 2.0 - 5.5 - d / 2.0];
            parts.push(Block::on_back(Role::Camera, c, top, d - 2.6, 0.1, 0.3));
            parts.push(flash([c[0] - d / 2.0 - 5.5, c[1]], back, 4.6));
            Bump { center: c, size: [d, d], corner: Corner::round(d / 2.0), height: p.housing_height }
        }
    };
    (bump, parts)
}

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
    let (bump, extras) = iphone_cameras(p);
    // Keys stand 0.35 mm proud; the Camera Control is a sapphire strip flush with the band.
    let side = |name, side, from_top: f32, length| Button { name, side, at: h / 2.0 - from_top, length, depth: 0.35, width: 2.6 };
    let mut buttons = vec![
        side(if p.action { "action" } else { "ring-silent" }, Side::Left, 30.5, 6.4),
        side("volume-up", Side::Left, 44.0, 10.5),
        side("volume-down", Side::Left, 57.5, 10.5),
        side("side", Side::Right, 48.0, 17.5),
    ];
    if p.camera_control {
        buttons.push(Button { depth: 0.06, ..side("camera-control", Side::Right, h - 51.0, 14.5) });
    }
    Body {
        id: p.id.to_owned(),
        aliases: Vec::new(),
        name: p.name.to_owned(),
        platform: Platform::Ios,
        form_factor: FormFactor::Phone,
        source: Source::Published(APPLE),
        size: [w, h, t],
        // A square-cornered screen (the SE's) sits in a body with plain round corners.
        corner: if p.screen_radius > 0.0 { Corner::continuous(p.screen_radius + bezel) } else { Corner::round(9.0) },
        bevel: p.edge.min(t / 2.0 - 1.4),
        screens: vec![screen],
        bump: Some(bump),
        buttons,
        crown: None,
        fold: None,
        extras,
        finish: p.finish,
    }
}

// --- Pixel ---

/// A Pixel phone: Google's published size and display, and its camera bar.
struct Pixel {
    id: &'static str,
    name: &'static str,
    /// Width, height, depth, mm.
    size: [f32; 3],
    px: (u32, u32),
    ppi: f32,
    inches: f32,
    /// The camera bar: `true` a pill that floats clear of the sides (Pixel 9), else a visor that
    /// runs into the band on both sides (Pixel 8).
    island: bool,
    lenses: u8,
    finish: Finish,
}

/// The ids are the SDK's hardware profile names (`hw.device.name` in an AVD made for the device).
const PIXELS: [Pixel; 5] = [
    Pixel {
        id: "pixel_9",
        name: "Pixel 9",
        size: [72.0, 152.8, 8.5],
        px: (1080, 2424),
        ppi: 422.0,
        inches: 6.3,
        island: true,
        lenses: 2,
        finish: PORCELAIN,
    },
    // Hazel: matte glass in a polished band.
    Pixel {
        id: "pixel_9_pro",
        name: "Pixel 9 Pro",
        size: [72.0, 152.8, 8.5],
        px: (1280, 2856),
        ppi: 495.0,
        inches: 6.3,
        island: true,
        lenses: 3,
        finish: finish(Surface::Polished, [0.42, 0.44, 0.41], Surface::MatteGlass, [0.20, 0.23, 0.21]),
    },
    Pixel {
        id: "pixel_9_pro_xl",
        name: "Pixel 9 Pro XL",
        size: [76.6, 162.8, 8.5],
        px: (1344, 2992),
        ppi: 486.0,
        inches: 6.8,
        island: true,
        lenses: 3,
        finish: finish(Surface::Polished, [0.10, 0.10, 0.105], Surface::MatteGlass, [0.022, 0.023, 0.026]),
    },
    Pixel {
        id: "pixel_8",
        name: "Pixel 8",
        size: [70.8, 150.5, 8.9],
        px: (1080, 2400),
        ppi: 428.0,
        inches: 6.2,
        island: false,
        lenses: 2,
        finish: OBSIDIAN,
    },
    Pixel {
        id: "pixel_8_pro",
        name: "Pixel 8 Pro",
        size: [76.5, 162.6, 8.8],
        px: (1344, 2992),
        ppi: 489.0,
        inches: 6.7,
        island: false,
        lenses: 3,
        finish: finish(Surface::Polished, [0.66, 0.64, 0.60], Surface::MatteGlass, [0.70, 0.67, 0.61]),
    },
];

/// A Pixel-style camera bar across the back of a phone `w` wide whose back is at `back` (z): the
/// bar, its black cover glass, `lenses` lenses under it and a flash beside it. `cy` is the bar's
/// centre line. As seen from the front, so the lenses (the back's left) are at +x.
fn camera_bar(w: f32, cy: f32, back: f32, island: bool, lenses: u8) -> (Bump, Vec<Block>) {
    let (bw, bh, height) = if island { (w - 19.0, 20.5, 3.0) } else { (w - 1.2, 19.0, 2.8) };
    let top = back - height;
    let n = f32::from(lenses.max(1));
    let (d, pitch) = (11.2, 13.4);
    // The cover glass: one pill over the bar's whole face, the lenses toward the back's left
    // under it and the flash at its other end.
    let gh = bh - 4.6;
    let gw = if island { bw - 4.6 } else { bw - 13.0 };
    let glass = top - 0.15;
    let mut parts = vec![Block::new(Role::Lens, [0.0, cy, top + 0.15], [gw, gh, 0.6], Corner::round(gh / 2.0), 0.1)];
    let first = gw / 2.0 - gh / 2.0 - 0.6;
    for i in 0..lenses.max(1) {
        parts.push(Block::on_back(Role::Camera, [first - f32::from(i) * pitch, cy], glass, d, 0.08, 0.2));
    }
    let free = gw - gh / 2.0 - 0.6 - (n - 1.0) * pitch - d / 2.0;
    parts.push(flash([-gw / 2.0 + (free / 2.0).clamp(4.0, 9.0), cy], glass, 4.6));
    let corner = if island { Corner::round(bh / 2.0) } else { Corner::round(2.5) };
    (Bump { center: [0.0, cy], size: [bw, bh], corner, height }, parts)
}

fn pixel(p: &Pixel) -> Body {
    let [w, h, t] = p.size;
    let mut screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px: p.px,
        ppi: p.ppi,
        center: [0.0, 0.0],
        shape: reference_shape(Platform::Android, FormFactor::Phone),
        corner_radius: 8.6,
        cutout: None,
        published: Some(Published::Diagonal(p.inches)),
    };
    let [sw, sh] = screen.size();
    // The front camera's punch hole, centred near the top.
    screen.cutout = Some(Cutout { kind: CutoutKind::PunchHole, center: [0.0, sh / 2.0 - 4.4], size: [3.7, 3.7] });
    let bezel = ((w - sw) / 2.0).min((h - sh) / 2.0);
    let (bump, extras) = camera_bar(w, h / 2.0 - 23.5, -t / 2.0, p.island, p.lenses);
    // Power above volume on the right side.
    let key = |name, from_top: f32, length| Button { name, side: Side::Right, at: h / 2.0 - from_top, length, depth: 0.4, width: 2.5 };
    Body {
        id: p.id.to_owned(),
        aliases: Vec::new(),
        name: p.name.to_owned(),
        platform: Platform::Android,
        form_factor: FormFactor::Phone,
        source: Source::Published(PIXEL),
        size: [w, h, t],
        corner: Corner::continuous(screen.corner_radius + bezel),
        bevel: 1.5,
        screens: vec![screen],
        bump: Some(bump),
        buttons: vec![key("power", 45.0, 10.5), key("volume", 68.0, 21.0)],
        crown: None,
        fold: None,
        extras,
        finish: p.finish,
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
    finish: Finish,
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
        finish: SPACE_BLACK,
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
        finish: SILVER,
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
        finish: finish(Surface::Aluminium, [0.42, 0.50, 0.60], Surface::Aluminium, [0.42, 0.50, 0.60]),
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
        finish: finish(Surface::Aluminium, [0.70, 0.66, 0.60], Surface::Aluminium, [0.70, 0.66, 0.60]),
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
        finish: finish(Surface::Aluminium, [0.38, 0.36, 0.48], Surface::Aluminium, [0.38, 0.36, 0.48]),
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
        finish: SILVER,
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
        corner: if round { Corner::round(bw / 2.0) } else { Corner::continuous(5.6) },
        height,
    };
    // One lens in a round bump; a wide lens, a LiDAR window and a flash on the Pro's island.
    let top = -t / 2.0 - height;
    let [cx, cy] = bump.center;
    let extras = if round {
        vec![Block::on_back(Role::Camera, [cx, cy], top, bw - 2.4, 0.1, 0.3)]
    } else {
        let mut parts = lens([cx, cy + bh / 2.0 - bw / 2.0], top, bw - 5.0, 0.7).to_vec();
        parts.push(Block::on_back(Role::Camera, [cx, cy - 4.5], top, 5.0, 0.08, 0.3));
        parts.push(flash([cx, cy - bh / 2.0 + 5.5], top, 4.6));
        parts
    };
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.35, width: 2.2 };
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
        corner: Corner::continuous(screen.corner_radius + bezel),
        bevel: 1.0,
        screens: vec![screen],
        bump: Some(bump),
        buttons,
        crown: None,
        fold: None,
        extras,
        finish: p.finish,
    }
}

/// The Pixel Tablet, landscape as it docks: 258 x 169 x 8.1 mm around a 10.95 inch 2560x1600
/// display at 276 ppi, with one camera in the back's top corner.
fn pixel_tablet() -> Body {
    let (w, h, t) = (258.0, 169.0, 8.1);
    let screen = Screen {
        display: Display::Main,
        face: Face::Front,
        px: (2560, 1600),
        ppi: 276.0,
        center: [0.0, 0.0],
        shape: reference_shape(Platform::Android, FormFactor::Tablet),
        corner_radius: 6.0,
        cutout: None,
        published: Some(Published::Diagonal(10.95)),
    };
    let [sw, sh] = screen.size();
    let bezel = ((w - sw) / 2.0).min((h - sh) / 2.0);
    let c = [w / 2.0 - 13.0, h / 2.0 - 13.0];
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.4, width: 2.6 };
    Body {
        id: "pixel_tablet".to_owned(),
        aliases: Vec::new(),
        name: "Pixel Tablet".to_owned(),
        platform: Platform::Android,
        form_factor: FormFactor::Tablet,
        source: Source::Published(PIXEL_TABLET),
        size: [w, h, t],
        corner: Corner::continuous(screen.corner_radius + bezel),
        bevel: 2.4,
        screens: vec![screen],
        bump: Some(Bump { center: c, size: [10.0, 10.0], corner: Corner::round(5.0), height: 0.5 }),
        buttons: vec![key("power", Side::Top, -w / 2.0 + 24.0, 16.0), key("volume", Side::Left, h / 2.0 - 32.0, 26.0)],
        crown: None,
        fold: None,
        extras: vec![Block::on_back(Role::Camera, c, -t / 2.0 - 0.5, 7.4, 0.1, 0.3)],
        // Porcelain: a matte coated aluminium shell.
        finish: finish(Surface::MatteGlass, [0.66, 0.63, 0.57], Surface::MatteGlass, [0.70, 0.67, 0.61]),
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
        finish: Finish::GRAPHITE,
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
    // The camera bar across the top of the other panel's back: a visor that floats clear of the
    // panel's edges, three lenses under one cover glass.
    let half = w / 2.0;
    let (mut bump, mut extras) = camera_bar(half + 11.0, h / 2.0 - 15.0, -t / 2.0, true, 3);
    bump.center[0] = w / 4.0;
    bump.height = 2.6;
    for x in &mut extras {
        x.center[0] += w / 4.0;
        x.center[2] += 0.4;
    }
    b.bump = Some(bump);
    b.extras = extras;
    let key = |name, at, length| Button { name, side: Side::Right, at, length, depth: 0.4, width: 2.4 };
    b.buttons = vec![key("power", h / 2.0 - 28.0, 12.0), key("volume", h / 2.0 - 52.0, 21.0)];
    // Porcelain: matte glass in a polished band.
    b.finish = finish(Surface::Polished, [0.66, 0.64, 0.60], Surface::MatteGlass, [0.70, 0.67, 0.61]);
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
    let key = |name, at, length| Button { name, side: Side::Right, at, length, depth: 0.35, width: 2.0 };
    b.buttons = vec![key("power", h / 2.0 - 30.0, 12.0), key("volume", h / 2.0 - 52.0, 20.0)];
    // Glacier: white glass on both outer faces.
    b.finish = finish(Surface::Plastic, [0.74, 0.74, 0.74], Surface::Glass, [0.80, 0.80, 0.80]);
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
    let (side, top, t, bevel) = (2.5, 3.0, 5.6, 1.0);
    let radius = |shape| if shape == Shape::Rounded { 4.0 } else { 0.0 };
    let inner = foldable_screen(Display::Inner, Face::Front, inner, ppi, [0.0, 0.0], shape, radius(shape));
    let [iw, ih] = inner.size();
    let outer_screen = |center| outer.map(|px| foldable_screen(Display::Outer, Face::Back, px, ppi, center, Shape::Rounded, 4.0));
    let outer_size = outer_screen([0.0, 0.0]).map(|s| s.size());
    let corner = Corner { radius: 7.0, exponent };
    let note = Source::Generic(note);
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.4, width: 2.4 };
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
    /// The system image's tag (`ai-glasses`, `android-xr`, `google_apis`, `google-tv`, ...), or ""
    /// when it isn't known (an Apple device, a profile without an image): what tells a headset
    /// from glasses.
    #[allow(dead_code, reason = "headset.rs reads it to tell glasses from headsets (#504)")]
    pub image_tag: &'a str,
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
    image_tag: &str,
) -> Option<Body> {
    let ppi = match density {
        Density::Dpi(d) => d.max(1) as f32,
        Density::Scale(s) => assumed_ppi(form_factor, s),
    };
    let name = format!("Generic {} {} ({}x{})", platform.label(), form_factor.as_str(), px.0, px.1);
    let spec = Spec { id, name, platform, form_factor, px, ppi, shape, image_tag };
    match form_factor {
        FormFactor::Foldable => Some(generic_book(id, &spec.name, platform, GENERIC, px, folded, ppi, shape, hinge, 2.0)),
        FormFactor::Phone | FormFactor::Tablet => Some(generic_slab(&spec)),
        FormFactor::Watch => watch::generic(&spec),
        FormFactor::Tv => tv::generic(&spec),
        FormFactor::Auto => car::generic(&spec),
        FormFactor::Vision => headset::generic(&spec),
    }
}

/// A generic phone or tablet: the screen under cover glass in thin, even bezels, a camera bar (a
/// tablet: one lens) and two keys.
fn generic_slab(g: &Spec) -> Body {
    // Bezel (the band's edge included), thickness, screen corner radius, edge radius.
    let (bezel, t, radius, bevel) = match g.form_factor {
        FormFactor::Tablet => (8.5, 6.5, 5.0, 1.2),
        _ => (3.0, 8.4, 7.5, 1.4),
    };
    let screen = g.screen(radius);
    let [sw, sh] = screen.size();
    let (size, corner) = if g.shape == Shape::Round {
        let d = sw.max(sh) + 2.0 * bezel;
        ([d, d, t], Corner::round(d / 2.0))
    } else {
        // Continuous corners round a rounded screen; a square-cornered one keeps plain ones, which
        // stay clear of its corners.
        let corner = if g.shape == Shape::Rounded { Corner::continuous(radius + bezel) } else { Corner::round(2.0 + bezel) };
        ([sw + 2.0 * bezel, sh + 2.0 * bezel, t], corner)
    };
    let [w, h, _] = size;
    let key = |name, side, at, length| Button { name, side, at, length, depth: 0.4, width: 2.5 };
    let (bump, extras, buttons) = match g.form_factor {
        FormFactor::Tablet => {
            let c = [w / 2.0 - 12.0, h / 2.0 - 12.0];
            let bump = Bump { center: c, size: [11.0, 11.0], corner: Corner::round(5.5), height: 0.6 };
            let keys = vec![key("power", Side::Top, -w / 2.0 + 30.0, 14.0), key("volume", Side::Top, w / 2.0 - 40.0, 25.0)];
            (bump, vec![Block::on_back(Role::Camera, c, -t / 2.0 - 0.6, 8.2, 0.1, 0.3)], keys)
        }
        _ => {
            // A camera bar where the phone is wide enough for one, else one lens in a round bump.
            let keys = vec![key("power", Side::Right, 0.21 * h, 10.5), key("volume", Side::Right, 0.05 * h, 21.0)];
            if w >= 56.0 && h >= 90.0 {
                let (bump, extras) = camera_bar(w, h / 2.0 - 23.5, -t / 2.0, true, 2);
                (bump, extras, keys)
            } else {
                let d = (0.3 * w.min(h)).clamp(4.0, 12.0);
                let c = [w / 2.0 - bezel - d, h / 2.0 - bezel - d];
                let bump = Bump { center: c, size: [d, d], corner: Corner::round(d / 2.0), height: 1.0 };
                (bump, vec![Block::on_back(Role::Camera, c, -t / 2.0 - 1.0, d - 2.2, 0.1, 0.3)], keys)
            }
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
        extras,
        finish: Finish::GRAPHITE,
    }
}
