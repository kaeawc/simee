//! What the GPU gets from a body: its mesh as one interleaved vertex buffer, and each part's
//! material, from what the part is (`Role`) and what the body is made of (`Finish`).

use sim_core::body::{Display, Finish, Mesh, Role, Surface};

/// Bytes per vertex: position (3 f32, mm), normal (3 f32), UV (2 f32).
pub const STRIDE: usize = 32;

/// The mesh's vertices, interleaved as [`STRIDE`] describes, little-endian f32.
pub fn vertices(mesh: &Mesh) -> Vec<u8> {
    let mut out = Vec::with_capacity(mesh.positions.len() * STRIDE);
    for (i, p) in mesh.positions.iter().enumerate() {
        let n = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
        let uv = mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
        for v in p.iter().chain(&n).chain(&uv) {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// The mesh's indices, u32 little-endian.
pub fn indices(mesh: &Mesh) -> Vec<u8> {
    mesh.indices.iter().flat_map(|i| i.to_le_bytes()).collect()
}

/// What a draw is, for the shader (`Draw.base.a` in scene.wgsl).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A lit surface.
    Lit,
    /// The screen in use: its frame, with only the cover glass's reflection over it.
    Live,
    /// The floor under the body, darkened by its contact shadow.
    Shadow,
    /// A camera lens: lit, with a bulging coated element in a dark barrel drawn from its UVs.
    Lens,
}

impl Kind {
    pub fn code(self) -> f32 {
        match self {
            Kind::Lit => 0.0,
            Kind::Live => 1.0,
            Kind::Shadow => 2.0,
            Kind::Lens => 3.0,
        }
    }
}

/// How a part is shaded: a metallic/roughness material (scene.wgsl).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Linear RGB: a metal's reflectance, anything else's diffuse colour.
    pub base: [f32; 3],
    /// 1 for a metal (it only mirrors, tinted by `base`), 0 otherwise.
    pub metallic: f32,
    /// 0 a mirror, 1 fully matte.
    pub roughness: f32,
    /// What a non-metal mirrors at normal incidence (glass is about 0.04).
    pub reflectance: f32,
    pub kind: Kind,
}

impl Material {
    /// Shows the live screen texture instead of a base colour.
    #[cfg(test)]
    pub fn live(&self) -> bool {
        self.kind == Kind::Live
    }
}

const fn lit(base: [f32; 3], metallic: f32, roughness: f32, reflectance: f32) -> Material {
    Material { base, metallic, roughness, reflectance, kind: Kind::Lit }
}

/// Cover glass over black: the border round a screen, and a screen that is off.
const BLACK_GLASS: Material = lit([0.003, 0.003, 0.004], 0.0, 0.03, 0.045);
/// The screen in use: its frame under the same glass.
const LIVE_GLASS: Material = Material { kind: Kind::Live, ..BLACK_GLASS };
/// A camera lens: coated glass over a dark barrel, a stronger mirror than plain glass.
const CAMERA: Material = Material { kind: Kind::Lens, ..lit([0.002, 0.003, 0.008], 0.0, 0.03, 0.14) };
/// A screen's cutout: the camera behind the glass, darker and duller than the border.
const CUTOUT: Material = lit([0.001, 0.001, 0.0015], 0.0, 0.35, 0.012);
/// The gap round a key: unlit.
const RECESS: Material = lit([0.004, 0.004, 0.005], 0.0, 0.9, 0.02);
/// A flash's diffuser.
const FLASH: Material = lit([0.80, 0.74, 0.56], 0.0, 0.45, 0.04);
/// Elastomer: a band, a head strap.
const STRAP: Material = lit([0.035, 0.037, 0.043], 0.0, 0.7, 0.035);
/// Soft trim: a light seal, a dash surround.
const TRIM: Material = lit([0.022, 0.023, 0.026], 0.0, 0.8, 0.03);
/// Dark satin metal: a TV's stand, a knob.
const STAND: Material = lit([0.20, 0.20, 0.21], 1.0, 0.38, 0.04);

/// A surface of `colour`: how metallic, rough and reflective each kind is.
pub fn surface(surface: Surface, colour: [f32; 3]) -> Material {
    let (metallic, roughness, reflectance) = match surface {
        Surface::Aluminium => (1.0, 0.40, 0.04),
        Surface::Polished => (1.0, 0.10, 0.04),
        Surface::Titanium => (1.0, 0.32, 0.04),
        Surface::Glass => (0.0, 0.05, 0.05),
        Surface::MatteGlass => (0.0, 0.52, 0.04),
        Surface::Ceramic => (0.0, 0.08, 0.06),
        Surface::Plastic => (0.0, 0.45, 0.04),
        Surface::Fabric => (0.0, 0.95, 0.02),
    };
    lit(colour, metallic, roughness, reflectance)
}

/// The material for a part with `role` of a body finished as `finish`, when `live` is the screen
/// showing the device's frame.
pub fn material(role: Role, live: Display, finish: &Finish) -> Material {
    match role {
        // The band and everything cut from it.
        Role::Shell | Role::Button | Role::Crown | Role::Bump => surface(finish.frame, finish.frame_colour),
        Role::Back => surface(finish.back, finish.back_colour),
        Role::Glass | Role::Lens => BLACK_GLASS,
        Role::Screen(d) if d == live => LIVE_GLASS,
        Role::Screen(_) => BLACK_GLASS,
        Role::Cutout => CUTOUT,
        Role::Recess => RECESS,
        Role::Camera => CAMERA,
        Role::Flash => FLASH,
        Role::Strap => STRAP,
        Role::Trim => TRIM,
        Role::Stand | Role::Knob => STAND,
    }
}

/// The floor's draw: the shader makes its colour from the stage's and the body's footprint.
pub const FLOOR: Material = Material { base: [0.0; 3], metallic: 0.0, roughness: 1.0, reflectance: 0.0, kind: Kind::Shadow };

/// A square in the floor's plane (y = 0, facing up), one unit each way from the origin: four
/// vertices as [`vertices`] lays them out, and its two triangles' indices from `first`.
pub fn floor_quad(first: u32) -> (Vec<u8>, Vec<u8>) {
    let mut v = Vec::with_capacity(4 * STRIDE);
    for [x, z] in [[-1.0f32, -1.0], [-1.0, 1.0], [1.0, 1.0], [1.0, -1.0]] {
        for c in [x, 0.0, z, 0.0, 1.0, 0.0, (x + 1.0) / 2.0, (z + 1.0) / 2.0] {
            v.extend_from_slice(&c.to_le_bytes());
        }
    }
    let i = [0u32, 1, 2, 0, 2, 3].iter().flat_map(|k| (first + k).to_le_bytes()).collect();
    (v, i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::body::{self, Part};

    fn f32_at(bytes: &[u8], i: usize) -> f32 {
        f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
    }

    #[test]
    fn vertex_buffer_interleaves_position_normal_uv() {
        let mesh = Mesh {
            positions: vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]],
            normals: vec![[0.0, 0.0, 1.0], [1.0, 0.0, 0.0]],
            uvs: vec![[0.25, 0.75], [1.0, 0.0]],
            indices: vec![0, 1, 0],
            parts: vec![Part { role: Role::Shell, panel: 0, first: 0, count: 3, vertex_first: 0, vertex_count: 2 }],
        };
        let v = vertices(&mesh);
        assert_eq!(v.len(), 2 * STRIDE);
        let second: Vec<f32> = (8..16).map(|i| f32_at(&v, i)).collect();
        assert_eq!(second, [4.0, 5.0, 6.0, 1.0, 0.0, 0.0, 1.0, 0.0]);
        assert_eq!(f32_at(&v, 6), 0.25);
        assert_eq!(indices(&mesh), [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a_real_body_fills_whole_vertices_and_its_parts_cover_the_indices() {
        let b = body::get("iPhone-17").expect("the table has the iPhone 17");
        let mesh = body::cached_mesh(&b);
        assert_eq!(vertices(&mesh).len(), mesh.positions.len() * STRIDE);
        assert_eq!(indices(&mesh).len(), mesh.indices.len() * 4);
        let covered: u32 = mesh.parts.iter().map(|p| p.count).sum();
        assert_eq!(covered as usize, mesh.indices.len());
        assert!(mesh.parts.iter().any(|p| p.role == Role::Screen(Display::Main)));
    }

    const F: Finish = Finish::GRAPHITE;

    #[test]
    fn only_the_screen_in_use_shows_the_frame() {
        assert!(material(Role::Screen(Display::Inner), Display::Inner, &F).live());
        assert!(!material(Role::Screen(Display::Outer), Display::Inner, &F).live());
        assert!(!material(Role::Shell, Display::Main, &F).live());
        assert_eq!(material(Role::Button, Display::Main, &F), material(Role::Shell, Display::Main, &F));
        // The screen in use and the glass round it are one sheet: the same gloss over both, so a
        // reflection runs across the screen's edge unbroken.
        let (live, border) = (material(Role::Screen(Display::Main), Display::Main, &F), material(Role::Glass, Display::Main, &F));
        assert_eq!((live.roughness, live.reflectance), (border.roughness, border.reflectance));
        assert_eq!(material(Role::Screen(Display::Outer), Display::Inner, &F), border, "a screen that is off is black glass");
    }

    #[test]
    fn extra_blocks_are_solid_finishes() {
        for role in [Role::Trim, Role::Strap, Role::Stand, Role::Knob, Role::Lens, Role::Camera, Role::Flash, Role::Recess] {
            assert!(!material(role, Display::Main, &F).live(), "{role:?}");
        }
        assert_ne!(material(Role::Strap, Display::Main, &F), material(Role::Shell, Display::Main, &F), "a strap is matte");
        assert!(material(Role::Strap, Display::Main, &F).roughness > 0.5);
        assert_eq!(material(Role::Lens, Display::Main, &F), material(Role::Glass, Display::Main, &F), "a lens is glass");
    }

    #[test]
    fn a_bodys_finish_decides_its_band_and_back() {
        // Metal bands are metallic, smoother when polished; glass backs are not metal, glossy or matte.
        let pro = body::get("iPhone-16-Pro").expect("row");
        let band = material(Role::Shell, Display::Main, &pro.finish);
        assert_eq!((band.metallic, band.base), (1.0, pro.finish.frame_colour));
        assert_eq!(material(Role::Bump, Display::Main, &pro.finish), band, "the camera housing is cut from the band's metal");
        let back = material(Role::Back, Display::Main, &pro.finish);
        assert_eq!((back.metallic, back.base), (0.0, pro.finish.back_colour));
        assert!(back.roughness > 0.4, "matte glass");
        let pixel = body::get("pixel_9").expect("row");
        assert!(material(Role::Back, Display::Main, &pixel.finish).roughness < 0.1, "glossy glass");
        assert!(surface(Surface::Polished, [0.5; 3]).roughness < surface(Surface::Titanium, [0.5; 3]).roughness);
        assert!(surface(Surface::Titanium, [0.5; 3]).roughness < surface(Surface::Aluminium, [0.5; 3]).roughness);
        for s in [Surface::Glass, Surface::MatteGlass, Surface::Ceramic, Surface::Plastic, Surface::Fabric] {
            assert_eq!(surface(s, [0.5; 3]).metallic, 0.0, "{s:?}");
        }
        // Every body's every part has a material in range.
        for b in body::all() {
            for part in &body::cached_mesh(b).parts {
                let m = material(part.role, Display::Main, &b.finish);
                assert!(
                    (0.0..=1.0).contains(&m.roughness)
                        && (0.0..=1.0).contains(&m.metallic)
                        && m.base.iter().all(|c| (0.0..=1.0).contains(c)),
                    "{} {:?}",
                    b.id,
                    part.role
                );
            }
        }
    }

    #[test]
    fn the_floor_quad_is_two_triangles_facing_up() {
        let (v, i) = floor_quad(10);
        assert_eq!((v.len(), i.len()), (4 * STRIDE, 6 * 4));
        // Every vertex lies in y = 0 with its normal +y.
        for k in 0..4 {
            assert_eq!((f32_at(&v, k * 8 + 1), f32_at(&v, k * 8 + 4)), (0.0, 1.0));
        }
        assert_eq!(u32::from_le_bytes(i[..4].try_into().unwrap()), 10);
        assert_eq!(FLOOR.kind.code(), 2.0);
        assert_eq!((Kind::Lit.code(), Kind::Live.code(), Kind::Lens.code()), (0.0, 1.0, 3.0));
        assert_eq!(material(Role::Camera, Display::Main, &F).kind, Kind::Lens);
    }
}
