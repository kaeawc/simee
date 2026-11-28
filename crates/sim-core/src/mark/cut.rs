//! The stone's geometry: rings of vertices from the crown's apex to the pavilion's tip, and the
//! facets between neighbouring rings.

use std::collections::HashSet;
use std::f32::consts::TAU;

/// How the stone is cut. Lengths are in girdle diameters when `radius` is 0.5 (the default).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cut {
    /// Sides of the girdle, 3 or more.
    pub sides: u32,
    /// Height of the crown (the upper pyramid) above the girdle.
    pub crown: f32,
    /// Depth of the pavilion (the lower pyramid) below the girdle.
    pub pavilion: f32,
    /// Girdle radius.
    pub radius: f32,
    /// Height of a straight band between the two pyramids; 0 joins them at one ring.
    pub band: f32,
    /// Turn of the band's lower ring against its upper one, as a fraction of half a facet: 1
    /// makes the band a ring of triangles.
    pub band_twist: f32,
    /// Width of a flat table on top, as a fraction of the girdle radius; 0 is a point.
    pub table: f32,
    /// Width of a flat culet at the bottom, as a fraction of the girdle radius; 0 is a point.
    pub culet: f32,
    /// A second ring of vertices on the crown, at this fraction of its height; 0 for none.
    pub crown_ring: f32,
    /// Turn of the crown ring, as a fraction of half a facet.
    pub crown_twist: f32,
    /// A second ring of vertices on the pavilion, at this fraction of its depth; 0 for none.
    pub pavilion_ring: f32,
    /// Turn of the pavilion ring, as a fraction of half a facet.
    pub pavilion_twist: f32,
    /// Every other girdle vertex pulled in by this fraction: a star-shaped waist.
    pub star: f32,
    /// How far the extra rings stand out of the straight line from girdle to apex, so their
    /// facets catch the light differently.
    pub bulge: f32,
}

impl Cut {
    /// A plain seven-sided bipyramid: the defaults the other cuts change.
    pub const PLAIN: Cut = Cut {
        sides: 7,
        crown: 0.55,
        pavilion: 0.95,
        radius: 0.5,
        band: 0.0,
        band_twist: 0.0,
        table: 0.0,
        culet: 0.0,
        crown_ring: 0.0,
        crown_twist: 0.0,
        pavilion_ring: 0.0,
        pavilion_twist: 0.0,
        star: 0.0,
        bulge: 1.07,
    };

    /// simee's mark: ten sides, a crown half as tall as the pavilion is deep, and a second,
    /// twisted ring on each pyramid. 60 facets, 1.5 times as tall as it is wide.
    pub const SIMEE: Cut = Cut {
        sides: 10,
        crown: 0.5,
        pavilion: 1.0,
        crown_ring: 0.55,
        crown_twist: 1.0,
        pavilion_ring: 0.5,
        pavilion_twist: 1.0,
        ..Cut::PLAIN
    };
}

impl Default for Cut {
    fn default() -> Self {
        Cut::SIMEE
    }
}

/// Which part of the stone a vertex or facet belongs to. The pyramids move apart in
/// [`super::Motion::Boot`]; a vertex on a shared girdle belongs to both, and a facet of a girdle
/// band follows its own vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Crown,
    Pavilion,
    Girdle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    pub at: [f32; 3],
    pub(crate) part: Part,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Face {
    /// Vertex indices, counter-clockwise seen from outside.
    pub corners: Vec<usize>,
    /// Outward unit normal.
    pub normal: [f32; 3],
    pub(crate) part: Part,
}

/// A cut built into vertices and facets, y up, the girdle at y = 0.
#[derive(Clone, Debug, PartialEq)]
pub struct Solid {
    pub vertices: Vec<Vertex>,
    pub faces: Vec<Face>,
    pub sides: u32,
    /// From the crown's top to the pavilion's bottom.
    pub height: f32,
    /// The widest diameter.
    pub width: f32,
    /// The y of the crown's top and of the pavilion's bottom.
    pub top: f32,
    pub bottom: f32,
}

struct Ring {
    y: f32,
    r: f32,
    turn: f32,
    part: Part,
    star: f32,
    at: Vec<usize>,
}

impl Ring {
    fn point(&self) -> bool {
        self.r < 1e-4
    }
}

impl Solid {
    pub fn new(cut: &Cut) -> Solid {
        let n = cut.sides.max(3) as usize;
        let step = TAU / n as f32;
        let r = cut.radius;
        let (upper, lower) = (cut.band / 2.0, -cut.band / 2.0);
        let (top, bottom) = (upper + cut.crown, lower - cut.pavilion);
        let ring = |y, r, turn, part, star| Ring { y, r, turn, part, star, at: Vec::new() };
        let band_turn = if cut.band > 0.0 { cut.band_twist * step / 2.0 } else { 0.0 };
        let (table, culet) = (r * cut.table, r * cut.culet);

        let mut rings = Vec::new();
        if cut.crown_ring > 0.0 {
            let turn = cut.crown_twist * step / 2.0;
            rings.push(ring(top, table, turn, Part::Crown, 0.0));
            let ry = (r + (table - r) * cut.crown_ring) * cut.bulge;
            rings.push(ring(upper + cut.crown_ring * cut.crown, ry, turn, Part::Crown, 0.0));
        } else {
            rings.push(ring(top, table, 0.0, Part::Crown, 0.0));
        }
        let girdle = if cut.band > 0.0 { Part::Crown } else { Part::Girdle };
        rings.push(ring(upper, r, 0.0, girdle, cut.star));
        if cut.band > 0.0 {
            rings.push(ring(lower, r, band_turn, Part::Pavilion, cut.star));
        }
        if cut.pavilion_ring > 0.0 {
            let turn = band_turn + cut.pavilion_twist * step / 2.0;
            let ry = (r + (culet - r) * cut.pavilion_ring) * cut.bulge;
            rings.push(ring(lower - cut.pavilion_ring * cut.pavilion, ry, turn, Part::Pavilion, 0.0));
            rings.push(ring(bottom, culet, turn, Part::Pavilion, 0.0));
        } else {
            rings.push(ring(bottom, culet, band_turn, Part::Pavilion, 0.0));
        }

        let mut vertices = Vec::new();
        for rg in &mut rings {
            if rg.point() {
                rg.at.push(vertices.len());
                vertices.push(Vertex { at: [0.0, rg.y, 0.0], part: rg.part });
                continue;
            }
            for i in 0..n {
                let a = rg.turn + i as f32 * step;
                let rr = if rg.star > 0.0 && i % 2 == 1 { rg.r * (1.0 - rg.star) } else { rg.r };
                rg.at.push(vertices.len());
                vertices.push(Vertex { at: [rr * a.cos(), rg.y, rr * a.sin()], part: rg.part });
            }
        }

        let mut faces = Vec::new();
        let mut add = |corners: Vec<usize>, part| faces.push(Face { corners, normal: [0.0; 3], part });
        let (first, last) = (&rings[0], &rings[rings.len() - 1]);
        if !first.point() {
            add(first.at.clone(), Part::Crown);
        }
        if !last.point() {
            add(last.at.clone(), Part::Pavilion);
        }
        for pair in rings.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let part = match (a.part, b.part) {
                (Part::Girdle, p) | (p, Part::Girdle) => p,
                (p, q) if p == q => p,
                _ => Part::Girdle,
            };
            if a.point() {
                (0..n).for_each(|i| add(vec![a.at[0], b.at[i], b.at[(i + 1) % n]], part));
                continue;
            }
            if b.point() {
                (0..n).for_each(|i| add(vec![a.at[i], a.at[(i + 1) % n], b.at[0]], part));
                continue;
            }
            let offset = ((b.turn - a.turn) % step + step) % step;
            for i in 0..n {
                let (a0, a1, b0, b1) = (a.at[i], a.at[(i + 1) % n], b.at[i], b.at[(i + 1) % n]);
                if offset < 1e-6 {
                    if a.star > 0.0 || b.star > 0.0 {
                        add(vec![a0, a1, b1], part);
                        add(vec![a0, b1, b0], part);
                    } else {
                        add(vec![a0, a1, b1, b0], part);
                    }
                } else {
                    add(vec![a0, a1, b0], part);
                    add(vec![b0, a1, b1], part);
                }
            }
        }

        // Newell's normal, turned outward (the girdle's centre is inside every cut).
        for f in &mut faces {
            let (mut n, mut c) = ([0.0f32; 3], [0.0f32; 3]);
            for k in 0..f.corners.len() {
                let a = vertices[f.corners[k]].at;
                let b = vertices[f.corners[(k + 1) % f.corners.len()]].at;
                n[0] += (a[1] - b[1]) * (a[2] + b[2]);
                n[1] += (a[2] - b[2]) * (a[0] + b[0]);
                n[2] += (a[0] - b[0]) * (a[1] + b[1]);
                (0..3).for_each(|j| c[j] += a[j]);
            }
            let mut n = normalize(n);
            if n[0] * c[0] + n[1] * c[1] + n[2] * c[2] < 0.0 {
                f.corners.reverse();
                n = [-n[0], -n[1], -n[2]];
            }
            f.normal = n;
        }

        let width = 2.0 * vertices.iter().map(|v| v.at[0].hypot(v.at[2])).fold(0.0, f32::max);
        Solid { vertices, faces, sides: n as u32, height: top - bottom, width, top, bottom }
    }

    /// The y the stone turns about: halfway between its top and bottom.
    pub fn middle(&self) -> f32 {
        (self.top + self.bottom) / 2.0
    }

    pub fn edges(&self) -> usize {
        let mut seen = HashSet::new();
        for f in &self.faces {
            for k in 0..f.corners.len() {
                let (a, b) = (f.corners[k], f.corners[(k + 1) % f.corners.len()]);
                seen.insert((a.min(b), a.max(b)));
            }
        }
        seen.len()
    }
}

pub(crate) fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l == 0.0 { v } else { [v[0] / l, v[1] / l, v[2] / l] }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cuts() -> Vec<Cut> {
        let p = Cut::PLAIN;
        vec![
            Cut::SIMEE,
            p,
            Cut { sides: 3, ..p },
            Cut { sides: 8, band: 0.1, ..p },
            Cut { band: 0.16, band_twist: 1.0, ..p },
            Cut { sides: 8, table: 0.5, culet: 0.2, ..p },
            Cut { sides: 12, star: 0.3, ..p },
            Cut { sides: 10, table: 0.35, crown_ring: 0.55, crown_twist: 1.0, pavilion_ring: 0.5, pavilion_twist: 1.0, ..p },
        ]
    }

    /// Every cut is a closed solid: each edge is walked once in each direction by the two facets
    /// that share it, and V − E + F = 2.
    #[test]
    fn every_cut_is_closed_and_consistently_wound() {
        for cut in cuts() {
            let s = Solid::new(&cut);
            let mut directed = HashSet::new();
            for f in &s.faces {
                for k in 0..f.corners.len() {
                    assert!(directed.insert((f.corners[k], f.corners[(k + 1) % f.corners.len()])), "{cut:?}: an edge walked twice one way");
                }
            }
            for &(a, b) in &directed {
                assert!(directed.contains(&(b, a)), "{cut:?}: edge {a}-{b} has one facet");
            }
            assert_eq!(s.vertices.len() + s.faces.len(), s.edges() + 2, "{cut:?}");
        }
    }

    #[test]
    fn normals_point_out_of_the_stone() {
        for cut in cuts() {
            let s = Solid::new(&cut);
            for f in &s.faces {
                let c = f.corners.iter().fold([0.0; 3], |c, &i| {
                    let a = s.vertices[i].at;
                    [c[0] + a[0], c[1] + a[1], c[2] + a[2]]
                });
                let dot = f.normal[0] * c[0] + f.normal[1] * c[1] + f.normal[2] * c[2];
                assert!(dot > 0.0, "{cut:?}");
                let len = f.normal.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!((len - 1.0).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn simees_cut_counts() {
        let s = Solid::new(&Cut::SIMEE);
        assert_eq!(s.sides, 10);
        // Apex, crown ring, girdle, pavilion ring, tip.
        assert_eq!(s.vertices.len(), 32);
        // 10 at the apex, 20 between the crown ring and the girdle, 20 below it, 10 at the tip.
        assert_eq!(s.faces.len(), 60);
        assert!((s.height - 1.5).abs() < 1e-6);
        assert!((s.width - 1.0).abs() < 1e-6, "the girdle is the widest ring");
        assert!(s.height / s.width > 1.4, "taller than wide");
    }

    #[test]
    fn a_table_and_a_culet_are_one_flat_facet_each() {
        let s = Solid::new(&Cut { sides: 8, table: 0.5, culet: 0.2, ..Cut::PLAIN });
        let flat: Vec<_> = s.faces.iter().filter(|f| f.corners.len() == 8).collect();
        assert_eq!(flat.len(), 2);
        assert!(flat.iter().any(|f| f.normal[1] > 0.999) && flat.iter().any(|f| f.normal[1] < -0.999));
    }

    #[test]
    fn fewer_than_three_sides_is_three() {
        assert_eq!(Solid::new(&Cut { sides: 1, ..Cut::PLAIN }).sides, 3);
    }
}
