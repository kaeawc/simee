//! Turning, projecting and lighting the stone: a [`Pose`] in, flat-shaded [`Facet`]s out, in
//! painter's order, in a box from -1 to 1 with y down (SVG's `viewBox="-1 -1 2 2"`).

use std::f32::consts::PI;

use super::cut::{Part, Solid, normalize};

/// A stone's colour: a ramp from its darkest facet to its brightest, in sRGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tone {
    /// simee, and Android devices.
    Emerald,
    /// iOS devices.
    Sapphire,
    /// Warnings.
    Citrine,
    /// Errors.
    Ruby,
    /// Devices that are off.
    Smoke,
}

impl Tone {
    pub const ALL: [Tone; 5] = [Tone::Emerald, Tone::Sapphire, Tone::Citrine, Tone::Ruby, Tone::Smoke];

    pub fn name(self) -> &'static str {
        match self {
            Tone::Emerald => "emerald",
            Tone::Sapphire => "sapphire",
            Tone::Citrine => "citrine",
            Tone::Ruby => "ruby",
            Tone::Smoke => "smoke",
        }
    }

    /// Six stops, dark to light.
    pub fn ramp(self) -> [[u8; 3]; 6] {
        match self {
            Tone::Emerald => {
                [[0x01, 0x15, 0x10], [0x03, 0x39, 0x2a], [0x07, 0x6d, 0x49], [0x11, 0xa4, 0x66], [0x56, 0xdd, 0x9b], [0xdf, 0xfc, 0xec]]
            }
            Tone::Sapphire => {
                [[0x02, 0x0c, 0x22], [0x08, 0x26, 0x60], [0x12, 0x4a, 0xa8], [0x2c, 0x7a, 0xe3], [0x88, 0xbc, 0xff], [0xe3, 0xef, 0xff]]
            }
            Tone::Citrine => {
                [[0x1d, 0x11, 0x02], [0x58, 0x36, 0x05], [0xa1, 0x69, 0x0b], [0xdf, 0x9f, 0x1a], [0xf7, 0xd2, 0x6b], [0xff, 0xf7, 0xd9]]
            }
            Tone::Ruby => {
                [[0x1c, 0x03, 0x07], [0x56, 0x0a, 0x17], [0x9c, 0x15, 0x29], [0xde, 0x33, 0x48], [0xff, 0x89, 0x94], [0xff, 0xe4, 0xe7]]
            }
            Tone::Smoke => {
                [[0x0f, 0x13, 0x12], [0x25, 0x2c, 0x29], [0x44, 0x50, 0x4a], [0x6c, 0x7a, 0x73], [0xa6, 0xb2, 0xac], [0xe8, 0xee, 0xeb]]
            }
        }
    }

    /// The ramp at `x` from 0 (darkest) to 1 (lightest), in sRGB 0–255.
    pub fn at(self, x: f32) -> [f32; 3] {
        let ramp = self.ramp();
        let x = x.clamp(0.0, 1.0) * (ramp.len() - 1) as f32;
        let i = (x.floor() as usize).min(ramp.len() - 2);
        let (a, b, t) = (ramp[i], ramp[i + 1], x - i as f32);
        std::array::from_fn(|k| f32::from(a[k]) + (f32::from(b[k]) - f32::from(a[k])) * t)
    }
}

/// How facets are coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    /// Smoothly lit facets with a specular highlight: the mark at 48 px and up.
    Shaded,
    /// Four tones and a hard highlight: favicons and icons under 48 px.
    Flat,
    /// One colour (black), facets told apart only by opacity: macOS template images, which the
    /// system tints for the menu bar.
    Template,
}

/// How the stone is framed and lit, whatever it is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    /// Tilt toward the viewer, in radians: shows a little of the crown.
    pub tilt: f32,
    /// The stone's longest side as a fraction of the box, leaving room to bounce and shake.
    pub pad: f32,
    /// Perspective strength: 0 is orthographic.
    pub perspective: f32,
    pub finish: Finish,
}

impl Style {
    /// The mark as simee draws it.
    pub const MARK: Style = Style { tilt: 0.22, pad: 0.8, perspective: 0.12, finish: Finish::Shaded };

    pub const fn finish(self, finish: Finish) -> Style {
        Style { finish, ..self }
    }

    pub const fn pad(self, pad: f32) -> Style {
        Style { pad, ..self }
    }

    pub const fn tilt(self, tilt: f32) -> Style {
        Style { tilt, ..self }
    }
}

/// Where the stone is and how it is lit at one moment. [`super::Motion::pose`] makes these.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// Turn about the vertical axis.
    pub yaw: f32,
    /// Tilt toward the viewer.
    pub pitch: f32,
    /// Roll in the picture plane.
    pub roll: f32,
    /// Offset in the box (-1 to 1, y down).
    pub offset: [f32; 2],
    /// How far the two pyramids stand apart, and how far each is turned the other way.
    pub split: f32,
    pub split_turn: f32,
    /// Direction toward the light (normalized when drawn).
    pub light: [f32; 3],
    pub tone: Tone,
    /// A second tone and how far toward it, 0 to 1.
    pub blend: Option<(Tone, f32)>,
    /// Extra brightness, 0 to 1.
    pub flash: f32,
    /// Fills with `tone` from the bottom up to this fraction; above it the stone is smoke.
    pub fill: Option<f32>,
}

impl Pose {
    /// The resting turn: a vertex faces the viewer, so a ridge runs up the middle between a lit
    /// side and a shaded one.
    pub const REST_YAW: f32 = -PI / 2.0;
    /// Upper left, in front.
    pub const LIGHT: [f32; 3] = [-0.55, 0.7, 0.6];
    /// A weaker light from the lower right, in front: the glow a cut stone throws back through
    /// its pavilion, which keeps the lower pyramid apart from a dark ground.
    pub const GLOW: [f32; 3] = [0.6, -0.45, 0.65];

    pub fn rest(style: &Style) -> Pose {
        Pose {
            yaw: Pose::REST_YAW,
            pitch: style.tilt,
            roll: 0.0,
            offset: [0.0, 0.0],
            split: 0.0,
            split_turn: 0.0,
            light: Pose::LIGHT,
            tone: Tone::Emerald,
            blend: None,
            flash: 0.0,
            fill: None,
        }
    }
}

/// One visible facet, its corners in the box and its colour (straight sRGB alpha).
#[derive(Clone, Debug, PartialEq)]
pub struct Facet {
    pub points: Vec<[f32; 2]>,
    pub rgba: [u8; 4],
}

struct Turn {
    sa: f32,
    ca: f32,
    sp: f32,
    cp: f32,
    sr: f32,
    cr: f32,
}

impl Turn {
    fn new(yaw: f32, pitch: f32, roll: f32) -> Turn {
        let ((sa, ca), (sp, cp), (sr, cr)) = (yaw.sin_cos(), pitch.sin_cos(), roll.sin_cos());
        Turn { sa, ca, sp, cp, sr, cr }
    }

    fn apply(&self, v: [f32; 3]) -> [f32; 3] {
        let x1 = v[0] * self.ca + v[2] * self.sa;
        let z1 = -v[0] * self.sa + v[2] * self.ca;
        let y1 = v[1] * self.cp - z1 * self.sp;
        let z2 = v[1] * self.sp + z1 * self.cp;
        [x1 * self.cr - y1 * self.sr, x1 * self.sr + y1 * self.cr, z2]
    }
}

/// The stone's visible facets for `pose`, back to front. The stone is opaque: facets facing away
/// are left out, and neighbouring facets share their edges exactly, so a rasterizer that samples
/// each pixel once (or a fill drawn with its own colour as an outline) shows no seams.
pub fn render(solid: &Solid, pose: &Pose, style: &Style) -> Vec<Facet> {
    let fit = 2.0 * style.pad / solid.height.max(solid.width);
    let mid = solid.middle();
    let crown = Turn::new(pose.yaw + pose.split_turn, pose.pitch, pose.roll);
    let pavilion = Turn::new(pose.yaw - pose.split_turn, pose.pitch, pose.roll);
    let project = |turn: &Turn, dy: f32| -> Vec<[f32; 3]> {
        solid
            .vertices
            .iter()
            .map(|v| {
                let r = turn.apply([v.at[0], v.at[1] - mid + dy, v.at[2]]);
                let f = 1.0 / (1.0 - r[2] * style.perspective);
                [r[0] * f * fit + pose.offset[0], -r[1] * f * fit + pose.offset[1], r[2]]
            })
            .collect()
    };
    let up = project(&crown, pose.split);
    let down = if pose.split == 0.0 && pose.split_turn == 0.0 { up.clone() } else { project(&pavilion, -pose.split) };

    let mut visible: Vec<(Vec<[f32; 2]>, f32, [f32; 3])> = Vec::with_capacity(solid.faces.len());
    for face in &solid.faces {
        let pts: Vec<[f32; 3]> = face
            .corners
            .iter()
            .map(|&i| {
                let part = if face.part == Part::Girdle { solid.vertices[i].part } else { face.part };
                if part == Part::Pavilion { down[i] } else { up[i] }
            })
            .collect();
        // Counter-clockwise from outside is clockwise on a y-down screen: a negative area faces us.
        let area: f32 = (0..pts.len())
            .map(|k| {
                let (a, b) = (pts[k], pts[(k + 1) % pts.len()]);
                a[0] * b[1] - b[0] * a[1]
            })
            .sum();
        if area >= 0.0 {
            continue;
        }
        let z = pts.iter().map(|p| p[2]).sum::<f32>() / pts.len() as f32;
        let normal = if face.part == Part::Pavilion { pavilion.apply(face.normal) } else { crown.apply(face.normal) };
        visible.push((pts.iter().map(|p| [p[0], p[1]]).collect(), z, normal));
    }
    visible.sort_by(|a, b| a.1.total_cmp(&b.1));

    let light = normalize(pose.light);
    let glow = normalize(Pose::GLOW);
    let level = pose.fill.map(|f| {
        let h = solid.height * fit * pose.pitch.cos();
        h / 2.0 - f.clamp(0.0, 1.0) * h + pose.offset[1]
    });
    let mut out = Vec::with_capacity(visible.len());
    for (points, _, n) in visible {
        let lit = |tone| shade(n, light, glow, tone, pose, style.finish);
        match level {
            None => out.push(Facet { points, rgba: lit(pose.tone) }),
            Some(y) => {
                for (part, tone) in [(clip(&points, y, false), Tone::Smoke), (clip(&points, y, true), pose.tone)] {
                    if part.len() >= 3 {
                        out.push(Facet { points: part, rgba: lit(tone) });
                    }
                }
            }
        }
    }
    out
}

fn shade(n: [f32; 3], l: [f32; 3], glow: [f32; 3], tone: Tone, pose: &Pose, finish: Finish) -> [u8; 4] {
    let d = n[0] * l[0] + n[1] * l[1] + n[2] * l[2];
    let back = (n[0] * glow[0] + n[1] * glow[1] + n[2] * glow[2]).max(0.0);
    let (diffuse, sky) = (d.max(0.0), 0.5 + 0.5 * n[1]);
    if finish == Finish::Template {
        let v = 0.25 + 0.6 * diffuse + 0.15 * sky;
        let a = if v > 0.72 {
            1.0
        } else if v > 0.5 {
            0.7
        } else {
            0.45
        };
        return [0, 0, 0, (a * 255.0f32).round() as u8];
    }
    let mut spec = (2.0 * d * n[2] - l[2]).max(0.0).powi(16);
    let mut v = 0.04 + 0.66 * diffuse + 0.2 * sky + 0.24 * back + pose.flash * 0.35;
    if finish == Finish::Flat {
        v = (v.clamp(0.0, 1.0) * 3.0).round() / 3.0 * 0.9 + 0.06;
        spec = if spec > 0.45 { 0.55 } else { 0.0 };
    }
    let mut c = tone.at(v * 0.88);
    if let Some((other, k)) = pose.blend
        && tone != Tone::Smoke
    {
        c = mix(c, other.at(v * 0.88), k);
    }
    c = mix(c, [255.0; 3], (spec * 0.75 + pose.flash * 0.12).clamp(0.0, 1.0));
    [c[0].round() as u8, c[1].round() as u8, c[2].round() as u8, 255]
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|k| a[k] + (b[k] - a[k]) * t)
}

/// The part of `poly` below (`below`, larger y) or above the line y = `level`.
fn clip(poly: &[[f32; 2]], level: f32, below: bool) -> Vec<[f32; 2]> {
    let inside = |p: [f32; 2]| if below { p[1] >= level } else { p[1] <= level };
    let mut out = Vec::with_capacity(poly.len() + 2);
    for k in 0..poly.len() {
        let (a, b) = (poly[k], poly[(k + 1) % poly.len()]);
        if inside(a) {
            out.push(a);
        }
        if inside(a) != inside(b) {
            let t = (level - a[1]) / (b[1] - a[1]);
            out.push([a[0] + (b[0] - a[0]) * t, level]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mark::Cut;

    fn bounds(facets: &[Facet]) -> [f32; 4] {
        facets
            .iter()
            .flat_map(|f| f.points.iter())
            .fold([f32::MAX, f32::MAX, f32::MIN, f32::MIN], |b, p| [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])])
    }

    #[test]
    fn the_resting_mark_is_taller_than_wide_and_inside_the_box() {
        let s = Solid::new(&Cut::SIMEE);
        let facets = render(&s, &Pose::rest(&Style::MARK), &Style::MARK);
        let [x0, y0, x1, y1] = bounds(&facets);
        assert!(x0 > -1.0 && y0 > -1.0 && x1 < 1.0 && y1 < 1.0);
        assert!((y1 - y0) / (x1 - x0) > 1.4, "{:?}", [x0, y0, x1, y1]);
        // A vertex faces the viewer: the silhouette is symmetric about the middle.
        assert!((x0 + x1).abs() < 0.02, "{x0} {x1}");
    }

    #[test]
    fn about_half_the_facets_face_the_viewer() {
        let s = Solid::new(&Cut::SIMEE);
        let n = render(&s, &Pose::rest(&Style::MARK), &Style::MARK).len();
        assert!((24..=36).contains(&n), "{n} of {}", s.faces.len());
    }

    #[test]
    fn the_light_falls_from_the_upper_left() {
        let s = Solid::new(&Cut::SIMEE);
        let facets = render(&s, &Pose::rest(&Style::MARK), &Style::MARK);
        let luma = |f: &Facet| f.rgba[..3].iter().map(|&c| u32::from(c)).sum::<u32>();
        let centre_x = |f: &Facet| f.points.iter().map(|p| p[0]).sum::<f32>() / f.points.len() as f32;
        let left: u32 = facets.iter().filter(|f| centre_x(f) < -0.05).map(luma).max().unwrap_or(0);
        let right: u32 = facets.iter().filter(|f| centre_x(f) > 0.05).map(luma).max().unwrap_or(0);
        assert!(left > right, "left {left} right {right}");
    }

    #[test]
    fn a_template_is_black_with_three_opacities() {
        let s = Solid::new(&Cut::SIMEE);
        let style = Style::MARK.finish(Finish::Template);
        let facets = render(&s, &Pose::rest(&style), &style);
        assert!(facets.iter().all(|f| f.rgba[..3] == [0, 0, 0] && [255, 179, 115].contains(&f.rgba[3])));
        assert!(facets.iter().any(|f| f.rgba[3] == 255) && facets.iter().any(|f| f.rgba[3] < 255));
    }

    #[test]
    fn flat_facets_use_at_most_five_colours() {
        let s = Solid::new(&Cut::SIMEE);
        let style = Style::MARK.finish(Finish::Flat);
        let mut colours: Vec<_> = render(&s, &Pose::rest(&style), &style).iter().map(|f| f.rgba).collect();
        colours.sort_unstable();
        colours.dedup();
        assert!(colours.len() <= 5, "{colours:?}");
    }

    #[test]
    fn a_fill_splits_the_stone_into_smoke_above_and_its_tone_below() {
        let s = Solid::new(&Cut::SIMEE);
        let mut pose = Pose::rest(&Style::MARK);
        pose.fill = Some(0.5);
        let facets = render(&s, &pose, &Style::MARK);
        // Smoke is grey (green within a few levels of red); emerald is far greener than red.
        let green = |f: &&Facet| i32::from(f.rgba[1]) - i32::from(f.rgba[0]);
        let tops: Vec<_> = facets.iter().filter(|f| f.points.iter().all(|p| p[1] < -0.3)).collect();
        let bottoms: Vec<_> = facets.iter().filter(|f| f.points.iter().all(|p| p[1] > 0.3)).collect();
        assert!(!tops.is_empty() && !bottoms.is_empty());
        assert!(tops.iter().all(|f| green(f) < 20), "the top half is grey");
        assert!(bottoms.iter().all(|f| green(f) > 20), "the bottom half is green");
    }

    #[test]
    fn ramps_run_dark_to_light() {
        for t in Tone::ALL {
            let luma = |c: [f32; 3]| c.iter().sum::<f32>();
            assert!(luma(t.at(0.0)) < luma(t.at(0.5)) && luma(t.at(0.5)) < luma(t.at(1.0)), "{}", t.name());
        }
    }
}
