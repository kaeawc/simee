//! The scene's camera and the device's transform in it (#155), and the matrices they become.
//! Column-major 4x4 matrices (`m[column][row]`), as `sim_core::body::HingeAxis::transform`
//! returns them; right-handed, depth 0..1 (wgpu).
//!
//! A [`Camera`] is a plain value: where it looks from (yaw, pitch), how far it is zoomed and
//! panned, and how much its framing is "the screen at 1:1" rather than "the whole body fitted".
//! It holds nothing about the window or the body, so one value means the same view in any slot,
//! and two values blend ([`Lerp`]): every change of view is a [`super::motion::Motion`] of it.
//! [`Camera::solve`] turns it into matrices for one frame against that frame's [`Framing`].
//! [`Turn`] is the device's own flip and spin in the scene, blended the same way.

use super::motion::{Lerp, mix};
use std::f32::consts::{PI, TAU};

/// A column-major 4x4 matrix.
pub type Mat4 = [[f32; 4]; 4];

#[cfg(test)]
pub const IDENTITY: Mat4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

/// `a * b`: `b` applies first.
pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    std::array::from_fn(|c| std::array::from_fn(|r| (0..4).map(|k| a[k][r] * b[c][k]).sum()))
}

/// Turns about x by `t` radians (counter-clockwise looking down the axis).
pub fn rot_x(t: f32) -> Mat4 {
    let (s, c) = t.sin_cos();
    [[1.0, 0.0, 0.0, 0.0], [0.0, c, s, 0.0], [0.0, -s, c, 0.0], [0.0, 0.0, 0.0, 1.0]]
}

pub fn rot_y(t: f32) -> Mat4 {
    let (s, c) = t.sin_cos();
    [[c, 0.0, -s, 0.0], [0.0, 1.0, 0.0, 0.0], [s, 0.0, c, 0.0], [0.0, 0.0, 0.0, 1.0]]
}

pub fn rot_z(t: f32) -> Mat4 {
    let (s, c) = t.sin_cos();
    [[c, s, 0.0, 0.0], [-s, c, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]]
}

pub fn translate([x, y, z]: [f32; 3]) -> Mat4 {
    [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [x, y, z, 1.0]]
}

/// A right-handed perspective projection onto wgpu's clip space (depth 0 at `near`, 1 at `far`).
pub fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fovy / 2.0).tan();
    [[f / aspect, 0.0, 0.0, 0.0], [0.0, f, 0.0, 0.0], [0.0, 0.0, far / (near - far), -1.0], [0.0, 0.0, near * far / (near - far), 0.0]]
}

/// `m` applied to point `p`, divided through by w.
pub fn apply(m: &Mat4, [x, y, z]: [f32; 3]) -> [f32; 3] {
    let v: [f32; 4] = std::array::from_fn(|r| m[0][r] * x + m[1][r] * y + m[2][r] * z + m[3][r]);
    [v[0] / v[3], v[1] / v[3], v[2] / v[3]]
}

/// `m` applied to direction `v` (no translation).
pub fn turn(m: &Mat4, [x, y, z]: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|r| m[0][r] * x + m[1][r] * y + m[2][r] * z)
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f32; 3], k: f32) -> [f32; 3] {
    a.map(|v| v * k)
}

fn length(a: [f32; 3]) -> f32 {
    a[0].hypot(a[1]).hypot(a[2])
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    std::array::from_fn(|k| mix(a[k], b[k], t))
}

/// `angle` plus whole turns, as close to `from` as it gets: the short way round.
fn nearest_turn(angle: f32, from: f32) -> f32 {
    angle + ((from - angle) / TAU).round() * TAU
}

/// The vertical field of view, radians. One lens for every view: face-on, the screen's plane is
/// square to it, so perspective maps it to the window by a uniform scale (1:1 is exact).
pub const FOVY: f32 = 24.0 * PI / 180.0;
/// What the fitted framing fills of the slot's height (or width, if narrower): every corner of
/// the body's box inside it from wherever the camera looks, so an orbit keeps the body whole
/// (the distance follows the angle continuously; it never jumps).
const FILL: f32 = 0.88;
/// How far outside the body's bounding sphere the eye always stays, as a fraction of its radius:
/// however far a zoom brings the camera in (face on at 1:1 it is close) and wherever an orbit
/// takes it, the eye is never inside the body and the near plane never cuts it.
const CLEARANCE: f32 = 0.05;
/// How far the camera may tilt above or below the body.
pub const PITCH_LIMIT: f32 = 80.0 * PI / 180.0;
/// Zoom, stops (powers of two) either side of the framing.
pub const ZOOM_LIMIT: f32 = 2.0;
/// How far a pan may take the look-at point from the framing's, in slot heights at zoom 0
/// (half the slot: the body's centre stays on screen).
pub const PAN_LIMIT: f32 = 0.5;
/// Orbit per point dragged, radians: a drag across a 400-point stage turns the body ~230°.
pub const ORBIT_PER_POINT: f32 = 0.01;

/// The saved views a key reaches (and `SIMEE_SCENE_CAMERA` starts in).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    /// The default: a little to the right of and above the screen, so its right side and top
    /// edge show.
    #[default]
    ThreeQuarter,
    /// Straight at the screen, the whole body fitted.
    Front,
    /// Edge on from the right and a little behind: the thickness, the side keys and the camera
    /// bump standing off the back.
    Side,
    /// Straight at the back.
    Back,
    /// Straight at the screen, framed so it shows at 1:1 where the flat view draws it: the
    /// "2D" view (#253).
    Face,
}

impl Preset {
    /// `SIMEE_SCENE_CAMERA`: `front`, `side`, `back`, `face`; anything else the 3/4 view.
    pub fn parse(s: Option<&str>) -> Preset {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("front") => Preset::Front,
            Some("side") => Preset::Side,
            Some("back") => Preset::Back,
            Some("face") => Preset::Face,
            _ => Preset::ThreeQuarter,
        }
    }
}

/// A camera pose (see the module docs). All of it blends, so any two views have a path between.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Camera {
    /// About the vertical, radians: positive moves the camera to the right. Unbounded (an orbit
    /// can go round and round); motions take the short way ([`Lerp::near`]).
    pub yaw: f32,
    /// Radians above (positive) or below, within [`PITCH_LIMIT`].
    pub pitch: f32,
    /// Stops in (positive) or out from the framing, within [`ZOOM_LIMIT`].
    pub zoom: f32,
    /// Where the body sits off the framing's centre, x right and y down, in slot heights at
    /// zoom 0: a pan follows the pointer at any zoom, and zooming keeps what's centred centred.
    pub pan: [f32; 2],
    /// 0 frames the whole body (its bounding sphere, centred); 1 frames the live screen so it
    /// lands exactly where the flat view draws it, one backdrop pixel per window pixel; between
    /// blends the two, so face-on and any 3D view move into each other smoothly.
    pub one_to_one: f32,
}

impl Lerp for Camera {
    fn lerp(a: Camera, b: Camera, t: f32) -> Camera {
        Camera {
            yaw: mix(a.yaw, b.yaw, t),
            pitch: mix(a.pitch, b.pitch, t),
            zoom: mix(a.zoom, b.zoom, t),
            pan: [mix(a.pan[0], b.pan[0], t), mix(a.pan[1], b.pan[1], t)],
            one_to_one: mix(a.one_to_one, b.one_to_one, t),
        }
    }

    fn near(self, from: Camera) -> Camera {
        Camera { yaw: nearest_turn(self.yaw, from.yaw), ..self }
    }
}

impl Camera {
    pub fn preset(p: Preset) -> Camera {
        let (yaw, pitch, one_to_one) = match p {
            Preset::ThreeQuarter => (28f32.to_radians(), 14f32.to_radians(), 0.0),
            Preset::Front => (0.0, 0.0, 0.0),
            Preset::Side => (105f32.to_radians(), 8f32.to_radians(), 0.0),
            Preset::Back => (PI, 0.0, 0.0),
            Preset::Face => (0.0, 0.0, 1.0),
        };
        Camera { yaw, pitch, one_to_one, ..Camera::default() }
    }

    /// Orbits by a drag of `dx`, `dy` points: the body turns with the pointer (a drag to the
    /// right swings the camera left), and a drag down tilts its top toward the viewer.
    pub fn orbit(self, dx: f32, dy: f32) -> Camera {
        let pitch = (self.pitch + dy * ORBIT_PER_POINT).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        Camera { yaw: self.yaw - dx * ORBIT_PER_POINT, pitch, ..self }
    }

    /// Pans by `dx`, `dy` points in a slot of `slot` points (width, height): the body follows
    /// the pointer, as far as [`PAN_LIMIT`] (wider across a wide slot).
    pub fn pan_by(self, dx: f32, dy: f32, [w, h]: [f32; 2]) -> Camera {
        let h = h.max(1.0);
        let k = 1.0 / (h * self.zoom.exp2());
        let x_limit = PAN_LIMIT * (w / h).max(1.0);
        let pan = [(self.pan[0] + dx * k).clamp(-x_limit, x_limit), (self.pan[1] + dy * k).clamp(-PAN_LIMIT, PAN_LIMIT)];
        Camera { pan, ..self }
    }

    /// Zooms by `stops` (positive in), within [`ZOOM_LIMIT`].
    pub fn zoom_by(self, stops: f32) -> Camera {
        let stops = if stops.is_finite() { stops } else { 0.0 };
        Camera { zoom: (self.zoom + stops).clamp(-ZOOM_LIMIT, ZOOM_LIMIT), ..self }
    }

    /// The camera's turn, world to view (without the pull-back along z), looking at the back
    /// instead when `back`.
    fn rotation(&self, back: bool) -> Mat4 {
        mul(&rot_x(self.pitch), &rot_y(-self.yaw(back)))
    }

    fn yaw(&self, back: bool) -> f32 {
        if back { self.yaw + PI } else { self.yaw }
    }

    /// A direction given in the camera's frame (x right, y up, z toward the viewer), in the
    /// world.
    pub fn world(&self, v: [f32; 3], back: bool) -> [f32; 3] {
        turn(&mul(&rot_y(self.yaw(back)), &rot_x(-self.pitch)), v)
    }

    /// Projection times view for one frame of `f` in a slot of `size` device pixels, and the
    /// eye (world mm). `back`: the screen shown faces -z, so every yaw is from behind.
    pub fn solve(&self, f: &Framing, size: (u32, u32), back: bool) -> (Mat4, [f32; 3]) {
        let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
        let aspect = w / h;
        let t = (FOVY / 2.0).tan();
        // How far back from the box's centre every corner is inside FILL of the slot.
        let fit = {
            let ty = t * FILL;
            let tx = ty * aspect;
            let rotation = self.rotation(back);
            (0..8).fold(1.0f32, |d, i| {
                let corner = [0, 1, 2].map(|k| if i >> k & 1 == 1 { f.half[k] } else { -f.half[k] });
                let [x, y, z] = turn(&rotation, corner);
                d.max(z + x.abs() / tx).max(z + y.abs() / ty)
            })
        };
        // The distance at which the screen's height covers the flat screen's pixels.
        let exact = f.screen_h.max(0.01) * h / (2.0 * t * f.flat[2].max(1.0));
        let blend = self.one_to_one.clamp(0.0, 1.0);
        let base = (fit.ln() * (1.0 - blend) + exact.ln() * blend).exp();
        // Pans and the flat screen's offset from the slot's centre, as world mm at the target.
        let per_height = 2.0 * base * t;
        let px = (self.pan[0] + blend * f.flat[0] / h) * per_height;
        let py = (self.pan[1] + blend * f.flat[1] / h) * per_height;
        let right = self.world([1.0, 0.0, 0.0], back);
        let up = self.world([0.0, 1.0, 0.0], back);
        let target = add(add(lerp3(f.center, f.screen, blend), scale(right, -px)), scale(up, py));
        let d = (base / self.zoom.exp2()).max(outside(f, target, self.world([0.0, 0.0, 1.0], back)));
        let reach = (length(f.half) + length(add(target, scale(f.center, -1.0)))) * 1.05;
        let (near, far) = ((d - reach).max(d * 0.02).max(0.1), d + reach.max(1.0));
        let view = mul(&mul(&translate([0.0, 0.0, -d]), &self.rotation(back)), &translate(target.map(|c| -c)));
        let eye = add(target, self.world([0.0, 0.0, d], back));
        (mul(&perspective(FOVY, aspect, near, far), &view), eye)
    }
}

/// The least distance back from `target` along `toward_eye` (a unit vector) that keeps the eye
/// outside the body's bounding sphere, [`CLEARANCE`] beyond it: the far side of where that ray
/// crosses the sphere, and where it misses no nearer than the foot of the perpendicular from the
/// centre (a few mm at most: the target is near the body). The two meet at a tangent ray, so it
/// follows the angle and the target continuously: a zoom or an orbit that meets it slides along
/// the sphere instead of jumping.
fn outside(f: &Framing, target: [f32; 3], toward_eye: [f32; 3]) -> f32 {
    let r = length(f.half) * (1.0 + CLEARANCE);
    let o = add(target, scale(f.center, -1.0));
    let along: f32 = (0..3).map(|k| o[k] * toward_eye[k]).sum();
    let gap = along * along - (length(o).powi(2) - r * r);
    (gap.max(0.0).sqrt() - along).max(0.0)
}

/// What a frame's camera frames, world mm (the body as its placement lays it, under its
/// [`Turn`]), and where the flat view draws the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Framing {
    /// The body's box (centre, half extents): the camera orbits its centre and fits it.
    pub center: [f32; 3],
    pub half: [f32; 3],
    /// The live screen's centre, and its height (along world y, as the frame reads upright).
    pub screen: [f32; 3],
    pub screen_h: f32,
    /// The flat screen's centre off the slot's centre (x right, y down) and its height, device
    /// pixels.
    pub flat: [f32; 3],
}

/// The device's own transform in the scene, apart from the guest's orientation: turned over
/// about the vertical (`flip`, radians; π shows the back) and rolled about the view axis
/// (`spin`, radians, counter-clockwise as seen from the front). Both accumulate, so flipping
/// again keeps turning the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Turn {
    pub flip: f32,
    pub spin: f32,
}

impl Lerp for Turn {
    fn lerp(a: Turn, b: Turn, t: f32) -> Turn {
        Turn { flip: mix(a.flip, b.flip, t), spin: mix(a.spin, b.spin, t) }
    }
}

impl Turn {
    /// Turned over: the back faces where the screen did.
    pub fn flipped(self) -> bool {
        (self.flip / PI).round().rem_euclid(2.0) == 1.0
    }

    /// Half a turn further over.
    pub fn flip(self) -> Turn {
        Turn { flip: (self.flip / PI).round() * PI + PI, ..self }
    }

    /// `quarters` quarter turns clockwise as seen from the front (negative counter-clockwise),
    /// from the nearest quarter.
    pub fn spin_by(self, quarters: i32) -> Turn {
        let q = PI / 2.0;
        Turn { spin: ((self.spin / q).round() - quarters as f32) * q, ..self }
    }

    /// The nearest upright, front-facing turn: whole turns of each.
    pub fn home(self) -> Turn {
        Turn { flip: (self.flip / TAU).round() * TAU, spin: (self.spin / TAU).round() * TAU }
    }

    /// The model transform about `center`: rolled, then turned over about the vertical through
    /// the centre (the roll is about the view axis, so a flipped device rolls the same way).
    pub fn matrix(&self, center: [f32; 3]) -> Mat4 {
        let about = mul(&rot_z(self.spin), &rot_y(self.flip));
        mul(&mul(&translate(center), &about), &translate(center.map(|c| -c)))
    }
}

/// The box around `points` (centre, half extents), or a unit box at the origin for none.
pub fn bounds(points: impl Iterator<Item = [f32; 3]>) -> ([f32; 3], [f32; 3]) {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for p in points {
        for k in 0..3 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    if lo[0] > hi[0] {
        return ([0.0; 3], [1.0; 3]);
    }
    (std::array::from_fn(|k| (lo[k] + hi[k]) / 2.0), std::array::from_fn(|k| (hi[k] - lo[k]) / 2.0))
}

/// Whether `p` is inside the quad `q` (corners in order, either winding, convex).
pub fn in_quad(q: &[[f32; 2]; 4], [x, y]: [f32; 2]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        let cross = (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]);
        if cross != 0.0 {
            if sign != 0.0 && cross.signum() != sign {
                return false;
            }
            sign = cross.signum();
        }
    }
    sign != 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn matrices_compose_and_turn_the_right_way() {
        assert!(close(apply(&rot_z(PI / 2.0), [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0]));
        assert!(close(apply(&rot_y(PI / 2.0), [0.0, 0.0, 1.0]), [1.0, 0.0, 0.0]));
        let m = mul(&translate([1.0, 2.0, 3.0]), &rot_x(PI / 2.0));
        assert!(close(apply(&m, [0.0, 1.0, 0.0]), [1.0, 2.0, 4.0]));
        assert!(close(turn(&m, [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]), "a direction doesn't move");
        assert_eq!(mul(&IDENTITY, &m), m);
    }

    #[test]
    fn bounds_frame_the_points() {
        assert_eq!(bounds([[0.0, -2.0, 1.0], [4.0, 2.0, 3.0]].into_iter()), ([2.0, 0.0, 2.0], [2.0, 2.0, 1.0]));
        assert_eq!(bounds(std::iter::empty()), ([0.0; 3], [1.0; 3]));
    }

    /// A phone-sized body at the origin with its screen on the front face, in a slot whose flat
    /// screen is `flat` (offset x, y and height, px).
    fn phone(flat: [f32; 3]) -> Framing {
        let half: [f32; 3] = [36.0, 75.0, 4.0];
        Framing { center: [0.0; 3], half, screen: [0.0, 0.0, 4.0], screen_h: 140.0, flat }
    }

    /// Every corner of a `half` box lands inside clip space, near and far included.
    fn fits(cam: Camera, half: [f32; 3], size: (u32, u32), back: bool) {
        let f = Framing { center: [0.0; 3], half, screen: [0.0, 0.0, half[2]], screen_h: half[1], flat: [0.0, 0.0, 100.0] };
        let (vp, _) = cam.solve(&f, size, back);
        for i in 0..8 {
            let c = [0, 1, 2].map(|k| if i >> k & 1 == 1 { half[k] } else { -half[k] });
            let [x, y, z] = apply(&vp, c);
            assert!(x.abs() <= 1.0 && y.abs() <= 1.0 && (0.0..=1.0).contains(&z), "{c:?} -> {:?} ({cam:?}, {size:?})", [x, y, z]);
        }
    }

    #[test]
    fn every_fitted_view_fits_the_body_in_any_slot_and_any_orbit() {
        let phone = [36.0, 75.0, 4.0];
        let tablet = [125.0, 90.0, 3.0];
        for size in [(500, 1000), (1000, 1000), (1600, 1000)] {
            for back in [false, true] {
                for preset in [Preset::ThreeQuarter, Preset::Front, Preset::Side, Preset::Back] {
                    fits(Camera::preset(preset), phone, size, back);
                    fits(Camera::preset(preset), tablet, size, back);
                }
                // Anywhere an orbit can go, and zoomed all the way out.
                for (yaw, pitch) in [(0.7, 1.3), (-2.0, -1.39), (3.0, 0.0)] {
                    let cam = Camera { yaw, pitch, ..Camera::default() };
                    fits(cam, phone, size, back);
                    fits(cam.zoom_by(-10.0), tablet, size, back);
                }
            }
        }
    }

    #[test]
    fn the_front_view_looks_straight_at_the_screen_and_fills_the_slot() {
        let f = phone([0.0, 0.0, 100.0]);
        let (vp, eye) = Camera::preset(Preset::Front).solve(&f, (600, 1000), false);
        assert!(eye[0].abs() < 1e-3 && eye[1].abs() < 1e-3 && eye[2] > 0.0);
        // The body's top edge sits near (not past) the top of the slot.
        let top = apply(&vp, [0.0, 75.0, 4.0]);
        assert!(top[1] > 0.75 && top[1] <= 0.95, "{top:?}");
        // The 3/4 view stands to the right and above; looking at the back, behind.
        let (_, eye) = Camera::preset(Preset::ThreeQuarter).solve(&f, (600, 1000), false);
        assert!(eye[0] > 0.0 && eye[1] > 0.0 && eye[2] > 0.0);
        let (_, eye) = Camera::preset(Preset::ThreeQuarter).solve(&f, (600, 1000), true);
        assert!(eye[2] < 0.0);
        let (_, eye) = Camera::preset(Preset::Back).solve(&f, (600, 1000), false);
        assert!(eye[2] < 0.0 && eye[0].abs() < 1e-2);
    }

    #[test]
    fn an_off_centre_body_is_looked_at_and_orbited_about_its_centre() {
        let f = Framing {
            center: [50.0, 0.0, 0.0],
            half: [10.0, 10.0, 1.0],
            screen: [50.0, 0.0, 1.0],
            screen_h: 20.0,
            flat: [0.0, 0.0, 100.0],
        };
        for cam in [Camera::preset(Preset::Front), Camera::preset(Preset::Front).orbit(80.0, -30.0)] {
            let (vp, _) = cam.solve(&f, (1000, 1000), false);
            let c = apply(&vp, [50.0, 0.0, 0.0]);
            assert!(c[0].abs() < 1e-4 && c[1].abs() < 1e-4, "{cam:?}: {c:?}");
        }
    }

    /// Where the flat view draws a 1000-px screen, centred 30 px right of and 20 px below the
    /// centre of a 1600x1200 slot.
    const FLAT: [f32; 3] = [30.0, 20.0, 1000.0];

    /// Window pixels of a world point, in a `size` slot.
    fn pixel(vp: &Mat4, p: [f32; 3], (w, h): (u32, u32)) -> [f32; 2] {
        let [x, y, _] = apply(vp, p);
        [(x + 1.0) / 2.0 * w as f32, (1.0 - y) / 2.0 * h as f32]
    }

    #[test]
    fn face_on_puts_the_screen_where_the_flat_view_does_one_pixel_per_pixel() {
        let size = (1600, 1200);
        let f = phone(FLAT);
        let (vp, _) = Camera::preset(Preset::Face).solve(&f, size, false);
        // The screen's top and bottom edges, 140 mm apart, are the flat screen's 1000 px apart,
        // centred where it is: 800 + 30 across, 600 + 20 down.
        let top = pixel(&vp, [0.0, 70.0, 4.0], size);
        let bottom = pixel(&vp, [0.0, -70.0, 4.0], size);
        assert!((bottom[1] - top[1] - 1000.0).abs() < 0.05, "{top:?} {bottom:?}");
        assert!((top[0] - 830.0).abs() < 0.05 && ((top[1] + bottom[1]) / 2.0 - 620.0).abs() < 0.05, "{top:?} {bottom:?}");
        // Uniform across the face: equal mm are equal pixels in x as in y, anywhere on it.
        let a = pixel(&vp, [-30.0, 50.0, 4.0], size);
        let b = pixel(&vp, [30.0, -50.0, 4.0], size);
        assert!(((b[0] - a[0]) - 60.0 / 140.0 * 1000.0).abs() < 0.05, "{a:?} {b:?}");
        assert!(((b[1] - a[1]) - 100.0 / 140.0 * 1000.0).abs() < 0.05, "{a:?} {b:?}");
        // Looking at a back-face screen, it's the same from behind.
        let back = Framing { screen: [0.0, 0.0, -4.0], ..f };
        let (vp, _) = Camera::preset(Preset::Face).solve(&back, size, true);
        let top = pixel(&vp, [0.0, 70.0, -4.0], size);
        let bottom = pixel(&vp, [0.0, -70.0, -4.0], size);
        assert!((bottom[1] - top[1] - 1000.0).abs() < 0.05 && (top[0] - 830.0).abs() < 0.05, "{top:?} {bottom:?}");
    }

    #[test]
    fn face_on_stays_one_to_one_at_any_window_size_and_blends_into_the_fit() {
        for (size, flat) in [((800, 600), [0.0, 0.0, 500.0]), ((2400, 1800), [-12.0, 0.0, 1700.0])] {
            let (vp, _) = Camera::preset(Preset::Face).solve(&phone(flat), size, false);
            let h = pixel(&vp, [0.0, -70.0, 4.0], size)[1] - pixel(&vp, [0.0, 70.0, 4.0], size)[1];
            assert!((h - flat[2]).abs() < 0.05, "{size:?}: {h}");
        }
        // Halfway from the fitted front view to face-on, the screen's height is between the two.
        let size = (1600, 1200);
        let f = phone(FLAT);
        let height = |cam: Camera| {
            let (vp, _) = cam.solve(&f, size, false);
            pixel(&vp, [0.0, -70.0, 4.0], size)[1] - pixel(&vp, [0.0, 70.0, 4.0], size)[1]
        };
        let (fit, face) = (height(Camera::preset(Preset::Front)), height(Camera::preset(Preset::Face)));
        let mid = height(Camera::lerp(Camera::preset(Preset::Front), Camera::preset(Preset::Face), 0.5));
        assert!(fit.min(face) < mid && mid < fit.max(face), "{fit} {mid} {face}");
    }

    #[test]
    fn orbit_turns_with_the_pointer_and_stops_short_of_the_poles() {
        let c = Camera::default().orbit(100.0, 50.0);
        assert!((c.yaw + 1.0).abs() < 1e-6, "a drag right swings the camera left");
        assert!((c.pitch - 0.5).abs() < 1e-6, "a drag down raises it");
        assert_eq!(Camera::default().orbit(0.0, 1e6).pitch, PITCH_LIMIT);
        assert_eq!(Camera::default().orbit(0.0, -1e6).pitch, -PITCH_LIMIT);
        // Yaw has no limit: round and round.
        assert!((Camera::default().orbit(-1000.0, 0.0).yaw - 10.0).abs() < 1e-4);
    }

    #[test]
    fn zoom_and_pan_stay_inside_their_limits() {
        assert_eq!(Camera::default().zoom_by(0.5).zoom, 0.5);
        assert_eq!(Camera::default().zoom_by(9.0).zoom, ZOOM_LIMIT);
        assert_eq!(Camera::default().zoom_by(-9.0).zoom, -ZOOM_LIMIT);
        assert_eq!(Camera::default().zoom_by(f32::NAN).zoom, 0.0);
        // 100 points in a 1000-point-high slot is a tenth of its height; zoomed in a stop, half that.
        let slot = [800.0, 1000.0];
        assert_eq!(Camera::default().pan_by(100.0, -100.0, slot).pan, [0.1, -0.1]);
        assert_eq!(Camera::default().zoom_by(1.0).pan_by(100.0, 0.0, slot).pan, [0.05, 0.0]);
        assert_eq!(Camera::default().pan_by(1e6, 1e6, slot).pan, [PAN_LIMIT, PAN_LIMIT]);
        // A wide slot lets it go further across.
        assert_eq!(Camera::default().pan_by(-1e6, 0.0, [2000.0, 1000.0]).pan, [-1.0, 0.0]);
    }

    #[test]
    fn a_pan_moves_the_body_with_the_pointer_at_any_zoom() {
        let size = (1000, 1000);
        let f = phone([0.0, 0.0, 500.0]);
        for zoom in [0.0, 1.0] {
            let cam = Camera { zoom, ..Camera::default() };
            let (vp, _) = cam.solve(&f, size, false);
            let before = pixel(&vp, [0.0, 0.0, 0.0], size);
            // 50 points in a 500-point slot drawn at 2 px a point: 100 px.
            let (vp, _) = cam.pan_by(50.0, 25.0, [500.0, 500.0]).solve(&f, size, false);
            let after = pixel(&vp, [0.0, 0.0, 0.0], size);
            assert!(
                (after[0] - before[0] - 100.0).abs() < 0.1 && (after[1] - before[1] - 50.0).abs() < 0.1,
                "{zoom}: {before:?} {after:?}"
            );
        }
    }

    #[test]
    fn a_camera_motion_takes_the_short_way_round() {
        let from = Camera { yaw: 6.0, ..Camera::default() };
        let to = Camera::preset(Preset::Front).near(from);
        assert!((to.yaw - TAU).abs() < 1e-5, "{to:?}");
        let mid = Camera::lerp(from, to, 0.5);
        assert!((mid.yaw - (6.0 + TAU) / 2.0).abs() < 1e-5);
        // Endpoints are exact.
        assert_eq!(Camera::lerp(from, to, 0.0), from);
        assert_eq!(Camera::lerp(from, to, 1.0), to);
    }

    #[test]
    fn a_turn_flips_over_and_back_and_spins_by_quarters() {
        let t = Turn::default();
        assert!(!t.flipped());
        let over = t.flip();
        assert!(over.flipped() && (over.flip - PI).abs() < 1e-6);
        let back = over.flip();
        assert!(!back.flipped() && (back.flip - TAU).abs() < 1e-6, "it keeps turning the same way");
        assert_eq!(back.home(), back, "a whole turn over is home already: no turn back");
        assert_eq!(Turn { flip: 0.4, spin: -0.5 }.home(), Turn::default());
        assert!(!over.home().flipped(), "home from the back turns on round to the front");
        // A flip halfway through a flip lands on the next half turn.
        let mid = Turn::lerp(t, over, 0.4);
        assert!((mid.flip().flip - TAU).abs() < 1e-5 || (mid.flip().flip - PI).abs() < 1e-5);
        let cw = t.spin_by(1);
        assert!((cw.spin + PI / 2.0).abs() < 1e-6, "clockwise is negative");
        assert!((cw.spin_by(-1).spin).abs() < 1e-6);
        assert_eq!(Turn { spin: 0.3, flip: 0.0 }.spin_by(0).spin, 0.0, "snaps to the nearest quarter");
        // Flipped about the vertical through the centre: the front face's normal points back.
        let m = Turn { flip: PI, spin: 0.0 }.matrix([10.0, 0.0, 0.0]);
        assert!(close(apply(&m, [10.0, 0.0, 4.0]), [10.0, 0.0, -4.0]));
        assert!(close(apply(&m, [12.0, 0.0, 0.0]), [8.0, 0.0, 0.0]));
        // Spun clockwise a quarter, the top edge points right.
        let m = cw.matrix([0.0; 3]);
        assert!(close(apply(&m, [0.0, 1.0, 0.0]), [1.0, 0.0, 0.0]));
    }

    #[test]
    fn a_point_is_in_a_quad_of_either_winding() {
        let q = [[0.0, 0.0], [10.0, 0.0], [10.0, 20.0], [0.0, 20.0]];
        assert!(in_quad(&q, [5.0, 5.0]));
        assert!(!in_quad(&q, [11.0, 5.0]));
        assert!(!in_quad(&q, [5.0, -1.0]));
        let mut r = q;
        r.reverse();
        assert!(in_quad(&r, [5.0, 19.0]));
        // A skewed (projected) quad.
        let s = [[0.0, 0.0], [10.0, 2.0], [9.0, 12.0], [1.0, 10.0]];
        assert!(in_quad(&s, [5.0, 6.0]));
        assert!(!in_quad(&s, [9.9, 0.5]));
        assert!(!in_quad(&[[0.0; 2]; 4], [0.0, 0.0]), "a degenerate quad holds nothing");
    }
}
