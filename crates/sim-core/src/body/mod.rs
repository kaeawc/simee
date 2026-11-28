//! Device bodies for the desktop's 3D view (#151): what each device type looks like from the
//! outside, as plain data, and the mesh generated from it ([`mesh`], cached by [`cached_mesh`]).
//!
//! A [`Body`] is in millimetres, in a right-handed frame with its origin at the body's centre: x
//! to the right and y up as the screen faces you in the device's natural orientation (portrait for
//! phones, a foldable unfolded), z out of the front face. Items on the back (the camera bump, a
//! foldable's outer screen) are placed by the same x and y, as seen from the front.
//!
//! Screens come from simee's own screen model: the pixel size simee reports for the type
//! ([`Kind::profile`], [`fold::devices`], the iPhone Duo's two screens in [`duo`]) at the panel's
//! density, so a body's screen rect is the screen simee draws. The table ([`all`]) has one row per
//! device type with published dimensions and says where each number came from ([`Source`]); a type
//! it doesn't know gets a generic body derived from the screen it reports ([`for_apple_type`],
//! [`for_android`]). Dimensions only: no logos, no vendor assets.
//!
//! Phones, tablets and foldables are in `table`; each other form factor has a module of its own
//! (`watch`, `tv`, `car`, `headset`) that says whether it has bodies yet ([`has_body`]) and
//! builds its rows and generic bodies, with parts a phone has no use for (straps, a stand, a
//! knob, lenses) as [`Body::extras`]. TVs, car head units and headsets have no body yet.
//!
//! [`Kind::profile`]: crate::form_factor::Kind::profile
//! [`fold::devices`]: crate::fold::devices
//! [`duo`]: crate::duo

mod car;
mod headset;
mod mesh;
mod table;
#[cfg(test)]
mod tests;
mod tv;
mod watch;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub use mesh::{CUTOUT_LIFT, MIN_RADIUS, Mesh, Part, Role, cached_mesh, mesh};

use crate::device::DeviceTypeInfo;
use crate::fold::Hinge;
use crate::form_factor::{Density, FormFactor, Profile, Shape};
use crate::platform::Platform;

/// Millimetres per inch.
pub const MM_PER_INCH: f32 = 25.4;

/// Where a body's numbers come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The maker's published dimensions; the note names the source and what was estimated.
    Published(&'static str),
    /// No published dimensions: derived from the screen simee reports, with generic bezels.
    Generic(&'static str),
}

impl Source {
    pub fn note(self) -> &'static str {
        match self {
            Self::Published(n) | Self::Generic(n) => n,
        }
    }
}

/// A rounded corner: its radius (mm) and the superellipse exponent of its curve (2 is a circular
/// arc; higher is squarer, like Apple's continuous corners).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Corner {
    pub radius: f32,
    pub exponent: f32,
}

/// Which of a device's screens this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Display {
    /// The only screen.
    Main,
    /// A foldable's unfolded screen.
    Inner,
    /// A foldable's cover screen, in use folded.
    Outer,
}

/// Which face of the body a screen is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    Front,
    Back,
}

/// A side of the body, as seen from the front.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// What cuts into the top of a screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutoutKind {
    /// A notch from the top edge.
    Notch,
    /// A pill below the top edge (the Dynamic Island).
    Island,
    /// A round camera hole.
    PunchHole,
}

/// A cutout: its kind, its centre relative to the screen's centre and its size, mm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cutout {
    pub kind: CutoutKind,
    pub center: [f32; 2],
    pub size: [f32; 2],
}

/// A size the maker publishes for a screen, kept to check the drawn screen against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Published {
    /// The diagonal, inches (phones, tablets, foldables).
    Diagonal(f32),
    /// The display area, square millimetres (Apple Watch).
    Area(f32),
}

/// One screen on the body.
///
/// The face is drawn upright in the body's natural orientation and the mesh's UVs (`Role::Screen`)
/// follow it: (0, 0) is the face's top-left, u grows to the right, v grows down. [`px`](Self::px)
/// is that face's pixel size, so a frame whose size is `px` goes on the face as is.
///
/// A screen whose panel is stored on its side is the exception: the iPhone Duo's inner screen
/// (`iPhone-Duo`, [`Display::Inner`]) is a landscape panel the simulator holds a quarter turn from
/// upright, so its frame arrives 2007x2853 (portrait) while the face, and `px`, are 2853x2007.
/// The scene turns the frame a quarter turn clockwise onto the face: the frame's top edge runs
/// down the face's right side, its left edge along the face's top, and its right edge (where
/// `simee swipe --edge right` goes home) is the face's bottom. A face UV (u, v) samples the frame
/// at (v, 1 - u); the frame's (u, v) lands on the face at (1 - v, u). The `docs/ios-versions.md`
/// iPhone Duo section has the framebuffer side of this.
#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub display: Display,
    pub face: Face,
    /// The screen in pixels as simee reports it, in the body's natural orientation.
    pub px: (u32, u32),
    /// The panel's pixels per inch: the size of the screen on the body.
    pub ppi: f32,
    /// The screen's centre, body coordinates (as seen from the front for a back-face screen).
    pub center: [f32; 2],
    /// simee's screen outline for the type.
    pub shape: Shape,
    /// The corner radius of a [`Shape::Rounded`] screen, mm.
    pub corner_radius: f32,
    pub cutout: Option<Cutout>,
    pub published: Option<Published>,
}

impl Screen {
    /// Width and height, mm.
    pub fn size(&self) -> [f32; 2] {
        [self.px.0 as f32 * MM_PER_INCH / self.ppi, self.px.1 as f32 * MM_PER_INCH / self.ppi]
    }

    /// Left, right, bottom and top edges, body coordinates.
    pub fn rect(&self) -> [f32; 4] {
        let [w, h] = self.size();
        [self.center[0] - w / 2.0, self.center[0] + w / 2.0, self.center[1] - h / 2.0, self.center[1] + h / 2.0]
    }

    /// The diagonal, inches.
    pub fn diagonal_inches(&self) -> f32 {
        let [w, h] = self.size();
        w.hypot(h) / MM_PER_INCH
    }
}

/// The camera bump (or a watch's sensor dome) on the back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bump {
    /// Its centre, as seen from the front, mm.
    pub center: [f32; 2],
    pub size: [f32; 2],
    pub corner: Corner,
    /// How far it stands proud of the back, mm.
    pub height: f32,
}

/// A side key, drawn as a small rounded box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Button {
    pub name: &'static str,
    pub side: Side,
    /// Its centre along the side: y on the left and right, x on the top and bottom, mm.
    pub at: f32,
    /// Along the side, mm.
    pub length: f32,
    /// How far it stands proud of the side, mm.
    pub depth: f32,
    /// Across the side (along z), mm.
    pub width: f32,
}

/// A watch's crown (the Digital Crown, a Wear OS rotary crown), drawn as a short cylinder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crown {
    pub side: Side,
    pub at: f32,
    pub diameter: f32,
    /// How far it stands proud of the side, mm.
    pub length: f32,
}

/// One rigid half of a foldable, flat (unfolded) in body coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panel {
    pub center: [f32; 2],
    pub size: [f32; 2],
    /// Corner radii, top-right, top-left, bottom-left, bottom-right, mm: small at the hinge.
    pub radii: [f32; 4],
}

/// A foldable's hinge: the axis panel 1 turns about relative to panel 0, and how far it turns.
/// Angles follow simee's hinge (`fold::Hinge`, `duo`): 0 closed screen to screen, 180 flat, 360
/// folded back to back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HingeAxis {
    /// The axis runs along y (a book fold, `fold::Hinge::vertical`), else along x (a flip).
    pub vertical: bool,
    /// Where the axis crosses x (vertical) or y (horizontal), mm.
    pub at: f32,
    /// The angles the hinge turns through, degrees.
    pub range: (f32, f32),
    /// The pivot's z while folding in (below 180°): the front face, where the screens meet.
    pub fold_in_z: f32,
    /// The pivot's z while folding back (above 180°): the back face.
    pub fold_out_z: f32,
}

impl HingeAxis {
    /// The angle the panels lie flat at, degrees.
    pub const FLAT: f32 = 180.0;

    /// Panel 1's transform at hinge `angle` (clamped to the range), column-major: panel 0 stays
    /// put, panel 1's mesh (authored flat) turns about the axis. Normals turn by its upper 3x3.
    pub fn transform(&self, angle: f32) -> [[f32; 4]; 4] {
        let angle = angle.clamp(self.range.0, self.range.1);
        let z = if angle <= Self::FLAT { self.fold_in_z } else { self.fold_out_z };
        let (s, c) = (Self::FLAT - angle).to_radians().sin_cos();
        // Folding in brings panel 1's front up and over panel 0: about -y for a book (panel 1 on
        // the right), about +x for a flip (panel 1 on top).
        let (cols, pivot) = if self.vertical {
            ([[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]], [self.at, 0.0, z])
        } else {
            ([[1.0, 0.0, 0.0], [0.0, c, s], [0.0, -s, c]], [0.0, self.at, z])
        };
        let rotated = |i: usize| cols[0][i] * pivot[0] + cols[1][i] * pivot[1] + cols[2][i] * pivot[2];
        let t = [pivot[0] - rotated(0), pivot[1] - rotated(1), pivot[2] - rotated(2)];
        [
            [cols[0][0], cols[0][1], cols[0][2], 0.0],
            [cols[1][0], cols[1][1], cols[1][2], 0.0],
            [cols[2][0], cols[2][1], cols[2][2], 0.0],
            [t[0], t[1], t[2], 1.0],
        ]
    }
}

/// A foldable's two panels and the hinge between them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldBody {
    pub panels: [Panel; 2],
    pub hinge: HingeAxis,
    /// The gap between the inner display's two halves at the hinge, mm: 0 for one flexible
    /// display, drawn as one face across the hinge (a Pixel Fold); the hinge's width for a device
    /// with two real screens (a Surface Duo). Each half stays on its panel either way.
    pub screen_gap: f32,
}

impl FoldBody {
    /// The side of panel `panel` the hinge is on, as seen from the front: panel 0's right for a
    /// book fold (a vertical hinge), its top for a flip.
    pub fn hinge_side(&self, panel: usize) -> Side {
        match (self.hinge.vertical, panel == 0) {
            (true, true) => Side::Right,
            (true, false) => Side::Left,
            (false, true) => Side::Top,
            (false, false) => Side::Bottom,
        }
    }
}

/// A rounded box on the body that is none of the parts above: a strap, a stand, a knob, a lens, a
/// light seal, a key strip. Drawn as its own part (a closed, outward-facing box), with the
/// panel its centre is on; it may stand off the body (a TV's stand) or sink into it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Block {
    /// What it is, for its material: one of the solid roles, never `Shell`, `Screen` or `Cutout`.
    pub role: Role,
    /// Its centre, body coordinates, mm.
    pub center: [f32; 3],
    /// Width (x), height (y) and depth (z), mm.
    pub size: [f32; 3],
    /// Its outline's corners (in the x/y plane).
    pub corner: Corner,
    /// The radius its front and back edges are rounded over, mm.
    pub bevel: f32,
}

/// A device type's body.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    /// The device type: an Apple device type identifier's last segment (`iPhone-17-Pro`), an
    /// AVD's `hw.device.name` (`pixel_fold`), or a generic body's key.
    pub id: String,
    /// Other identifiers the same body answers to.
    pub aliases: Vec<&'static str>,
    pub name: String,
    pub platform: Platform,
    pub form_factor: FormFactor,
    pub source: Source,
    /// Width (x), height (y) and thickness (z), mm; a foldable's unfolded.
    pub size: [f32; 3],
    pub corner: Corner,
    /// The radius the front and back edges are rounded over, mm.
    pub bevel: f32,
    pub screens: Vec<Screen>,
    pub bump: Option<Bump>,
    pub buttons: Vec<Button>,
    pub crown: Option<Crown>,
    pub fold: Option<FoldBody>,
    /// Parts beyond the above ([`Block`]): straps, a stand, a knob, lenses.
    pub extras: Vec<Block>,
}

impl Body {
    /// The rigid parts: a foldable's two panels, else the whole body.
    pub fn panels(&self) -> Vec<Panel> {
        match &self.fold {
            Some(f) => f.panels.to_vec(),
            None => {
                let [w, h, _] = self.size;
                vec![Panel { center: [0.0, 0.0], size: [w, h], radii: [self.corner.radius; 4] }]
            }
        }
    }

    /// The panel a point (body x, y) belongs to: the side of the hinge it is on.
    pub fn panel_at(&self, [x, y]: [f32; 2]) -> usize {
        match &self.fold {
            Some(f) => usize::from(if f.hinge.vertical { x >= f.hinge.at } else { y >= f.hinge.at }),
            None => 0,
        }
    }

    pub fn screen(&self, display: Display) -> Option<&Screen> {
        self.screens.iter().find(|s| s.display == display)
    }

    /// Whether one of the body's screens is `px` (either orientation): the screen simee reports.
    pub fn shows(&self, px: (u32, u32)) -> bool {
        self.screens.iter().any(|s| s.px == px || s.px == (px.1, px.0))
    }

    fn answers_to(&self, id: &str) -> bool {
        self.id == id || self.aliases.contains(&id)
    }
}

/// Whether a form factor has a body: phones, tablets and foldables always; each other form factor
/// once its module says so (watches do; TVs, car head units and headsets don't yet).
pub fn has_body(form_factor: FormFactor) -> bool {
    match form_factor {
        FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable => true,
        FormFactor::Watch => watch::ENABLED,
        FormFactor::Tv => tv::ENABLED,
        FormFactor::Auto => car::ENABLED,
        FormFactor::Vision => headset::ENABLED,
    }
}

/// Every body in the table, built once.
pub fn all() -> &'static [Arc<Body>] {
    static ALL: OnceLock<Vec<Arc<Body>>> = OnceLock::new();
    ALL.get_or_init(|| table::rows().into_iter().map(Arc::new).collect())
}

/// The table's body for a device type id: `iPhone-17-Pro`, a full CoreSimulator identifier
/// (`com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro`) or an AVD's `pixel_fold`.
pub fn get(id: &str) -> Option<Arc<Body>> {
    let id = id.strip_prefix("com.apple.CoreSimulator.SimDeviceType.").unwrap_or(id);
    all().iter().find(|b| b.answers_to(id)).cloned()
}

/// The body for an Apple device type as simee lists it: the table's row when it knows the type and
/// the row's screen is the one the type reports, else a generic body from that screen. None for a
/// form factor with no body.
pub fn for_apple_type(info: &DeviceTypeInfo) -> Option<Arc<Body>> {
    let px = (info.screen_width.round() as u32, info.screen_height.round() as u32);
    let known = get(&info.identifier).or_else(|| all().iter().find(|b| b.name == info.name).cloned());
    if let Some(b) = known.filter(|b| b.platform == Platform::Ios && b.shows(px)) {
        return Some(b);
    }
    let form_factor = info.form_factor.filter(|f| has_body(*f))?;
    let density = Density::Scale(info.screen_scale.round().max(1.0) as u32);
    let shape = table::reference_shape(Platform::Ios, form_factor);
    generic(Platform::Ios, form_factor, px, density, shape, None, table::HingeSpec::default())
}

/// The body for an Android virtual device: the table's row for its hardware definition
/// (`hw.device.name`) when the AVD's screen is that row's, else [`for_profile`].
pub fn for_android(device_name: &str, profile: &Profile) -> Option<Arc<Body>> {
    for_android_hinge(device_name, profile, None)
}

/// [`for_android`] with the AVD's own hinge (`fold::Hinge::from_config` on its `config.ini`): a
/// foldable the table doesn't list folds the way that hinge says.
pub fn for_android_hinge(device_name: &str, profile: &Profile, hinge: Option<&Hinge>) -> Option<Arc<Body>> {
    let known = get(device_name).filter(|b| {
        b.platform == Platform::Android && b.form_factor == profile.kind.form_factor && b.shows((profile.width, profile.height))
    });
    known.or_else(|| for_profile_hinge(profile, hinge))
}

/// A generic body for a profile's screen (its density, shape and fold), cached by its key; the
/// reference profiles' bodies are rows of the table. A foldable's hinge is a vertical one turning
/// through 0-180°: see [`for_profile_hinge`] for any other.
pub fn for_profile(profile: &Profile) -> Option<Arc<Body>> {
    for_profile_hinge(profile, None)
}

/// [`for_profile`] with a foldable's hinge, as the AVD describes it (`fold::Hinge`): its
/// orientation (a vertical hinge is a book, a horizontal one a flip), the angles it turns
/// through, and for a physical hinge with an area, the gap it leaves between two real screens. A
/// profile that isn't a foldable ignores it, and `None` is a vertical hinge over 0-180° with one
/// flexible screen.
pub fn for_profile_hinge(profile: &Profile, hinge: Option<&Hinge>) -> Option<Arc<Body>> {
    let folded = profile.fold.map(|f| (f.folded_width, f.folded_height));
    let hinge = hinge.map(table::HingeSpec::from_hinge).unwrap_or_default();
    generic(profile.kind.platform, profile.kind.form_factor, (profile.width, profile.height), profile.density, profile.shape, folded, hinge)
}

fn generic(
    platform: Platform,
    form_factor: FormFactor,
    px: (u32, u32),
    density: Density,
    shape: Shape,
    folded: Option<(u32, u32)>,
    hinge: table::HingeSpec,
) -> Option<Arc<Body>> {
    if !has_body(form_factor) || px.0 == 0 || px.1 == 0 {
        return None;
    }
    let hinge = if form_factor == FormFactor::Foldable { hinge } else { table::HingeSpec::default() };
    let id = table::generic_id(platform, form_factor, px, density, shape, folded, hinge);
    if let Some(b) = get(&id) {
        return Some(b);
    }
    static GENERIC: OnceLock<Mutex<HashMap<String, Arc<Body>>>> = OnceLock::new();
    let mut map = GENERIC.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(b) = map.get(&id) {
        return Some(Arc::clone(b));
    }
    let body = Arc::new(table::generic(&id, platform, form_factor, px, density, shape, folded, hinge)?);
    map.insert(id, Arc::clone(&body));
    Some(body)
}
