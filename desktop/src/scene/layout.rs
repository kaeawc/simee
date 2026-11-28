//! What the GPU gets from a body: its mesh as one interleaved vertex buffer, and each part's
//! material (the neutral finishes #153 left to the desktop).

use sim_core::body::{Display, Mesh, Role};

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

/// How a part is shaded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Linear RGB.
    pub base: [f32; 3],
    /// Specular strength and Blinn-Phong exponent.
    pub specular: f32,
    pub shininess: f32,
    /// Shows the live screen texture (emissive) instead of the base colour.
    pub live: bool,
}

/// Satin graphite: the body and its keys.
const SHELL: Material = Material { base: [0.045, 0.047, 0.052], specular: 0.22, shininess: 24.0, live: false };
/// Glossy, darker: the camera bump's glass and the screen's cutout.
const GLOSS: Material = Material { base: [0.012, 0.012, 0.014], specular: 0.55, shininess: 90.0, live: false };
/// A screen that is off: black glass.
const DARK_GLASS: Material = Material { base: [0.004, 0.004, 0.005], specular: 0.45, shininess: 140.0, live: false };
/// The screen in use: its frame, under a faint glass highlight.
const LIVE_GLASS: Material = Material { base: [0.0; 3], specular: 0.18, shininess: 160.0, live: true };
/// Matte elastomer: straps, a light seal, a dash surround.
const MATTE: Material = Material { base: [0.03, 0.031, 0.034], specular: 0.06, shininess: 8.0, live: false };

/// The material for a part with `role`, when `live` is the screen showing the device's frame.
pub fn material(role: Role, live: Display) -> Material {
    match role {
        Role::Shell | Role::Button | Role::Crown | Role::Stand | Role::Knob => SHELL,
        Role::Strap | Role::Trim => MATTE,
        Role::Bump | Role::Cutout | Role::Lens => GLOSS,
        Role::Screen(d) if d == live => LIVE_GLASS,
        Role::Screen(_) => DARK_GLASS,
    }
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
            parts: vec![Part { role: Role::Shell, panel: 0, first: 0, count: 3 }],
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

    #[test]
    fn only_the_screen_in_use_shows_the_frame() {
        assert!(material(Role::Screen(Display::Inner), Display::Inner).live);
        assert!(!material(Role::Screen(Display::Outer), Display::Inner).live);
        assert!(!material(Role::Shell, Display::Main).live);
        assert_eq!(material(Role::Button, Display::Main), material(Role::Shell, Display::Main));
    }

    #[test]
    fn extra_blocks_are_solid_finishes() {
        for role in [Role::Trim, Role::Strap, Role::Stand, Role::Knob, Role::Lens] {
            assert!(!material(role, Display::Main).live, "{role:?}");
        }
        assert_eq!(material(Role::Strap, Display::Main), material(Role::Trim, Display::Main));
        assert_ne!(material(Role::Strap, Display::Main), material(Role::Shell, Display::Main), "a strap is matte");
        assert_eq!(material(Role::Lens, Display::Main), material(Role::Bump, Display::Main), "a lens is glass");
    }
}
