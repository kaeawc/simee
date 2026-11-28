//! The simee mark: an eight-sided gem (girdle Ø 1, crown 0.5, pavilion 1), flat-shaded and turned
//! about its vertical axis, drawn as one Rune canvas of projected facets. Figure 1 adds the
//! drafting annotations (apex, girdle, culet, the heights and the girdle's diameter).

use rune::{Color, El, LineCap, Shape, canvas, col, el, oklch, text};

use crate::tokens::*;

/// Seconds per full turn.
pub const TURN: f64 = 40.0;
const SIDES: usize = 8;
const CROWN: f32 = 0.5;
const PAVILION: f32 = 1.0;
/// The view's tilt toward the crown, in radians.
const TILT: f32 = 0.16;

type V3 = [f32; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn norm(a: V3) -> V3 {
    let l = dot(a, a).sqrt().max(1e-6);
    [a[0] / l, a[1] / l, a[2] / l]
}

/// A point on a ring at height `y`, radius `r` and angle `a` (y up).
fn ring(y: f32, r: f32, a: f32) -> V3 {
    [r * a.cos(), y, r * a.sin()]
}

/// The gem's facets in model space, each a triangle and whether it is on the crown. Every facet
/// is wound so its normal points outward.
pub fn facets() -> Vec<([V3; 3], bool)> {
    let step = std::f32::consts::TAU / SIDES as f32;
    let apex = [0.0, CROWN, 0.0];
    let culet = [0.0, -PAVILION, 0.0];
    let mut out = Vec::with_capacity(SIDES * 6);
    for i in 0..SIDES {
        let (a0, a1, mid) = (i as f32 * step, (i + 1) as f32 * step, (i as f32 + 0.5) * step);
        let (g0, g1) = (ring(0.0, 0.5, a0), ring(0.0, 0.5, a1));
        // Star and pavilion-main points sit a little outside each side's plane, so a side reads as
        // three facets that catch the light differently.
        let star = ring(0.24, 0.31, mid);
        let main = ring(-0.42, 0.33, mid);
        for (tri, crown) in [
            ([apex, g0, star], true),
            ([apex, star, g1], true),
            ([g0, g1, star], true),
            ([g1, g0, main], false),
            ([g0, culet, main], false),
            ([main, culet, g1], false),
        ] {
            out.push((outward(tri), crown));
        }
    }
    out
}

/// `tri` wound so its normal points away from the gem's middle.
fn outward(tri: [V3; 3]) -> [V3; 3] {
    let n = cross(sub(tri[1], tri[0]), sub(tri[2], tri[0]));
    let c = [
        (tri[0][0] + tri[1][0] + tri[2][0]) / 3.0,
        (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0 + 0.25,
        (tri[0][2] + tri[1][2] + tri[2][2]) / 3.0,
    ];
    if dot(n, c) < 0.0 { [tri[0], tri[2], tri[1]] } else { tri }
}

/// `p` turned by `angle` about the vertical axis, then tilted toward the viewer.
fn view(p: V3, angle: f32) -> V3 {
    let (s, c) = angle.sin_cos();
    let (x, z) = (p[0] * c + p[2] * s, -p[0] * s + p[2] * c);
    let (ts, tc) = TILT.sin_cos();
    [x, p[1] * tc - z * ts, p[1] * ts + z * tc]
}

/// The gem at `angle` as facets on a canvas `w` × `h` points, girdle `scale` points wide, its
/// girdle centred at (`cx`, `cy`). Back facets are culled: the solid is convex, so the rest never
/// overlap.
pub fn draw(mut shape: Shape, angle: f32, scale: f32, cx: f32, cy: f32) -> Shape {
    let light = norm([-0.45, 0.75, 0.6]);
    for (tri, crown) in facets() {
        let v = tri.map(|p| view(p, angle));
        let n = norm(cross(sub(v[1], v[0]), sub(v[2], v[0])));
        if n[2] <= 0.0 {
            continue;
        }
        let lit = dot(n, light).max(0.0);
        let color = if crown { oklch(0.62 + 0.24 * lit, 0.12, 160.0) } else { oklch(0.36 + 0.2 * lit, 0.075, 165.0) };
        let pts: Vec<[f32; 2]> = v.iter().map(|p| [cx + p[0] * scale, cy - p[1] * scale]).collect();
        shape = shape.fill(pts, color);
    }
    shape
}

/// The gem alone, `size` points tall: the mark beside the site's name.
pub fn mark(size: f32, angle: f32) -> El {
    let h = CROWN + PAVILION;
    let scale = size / h;
    let w = scale;
    canvas(draw(Shape::new().view_box(w, size), angle, scale, w / 2.0, CROWN * scale)).size(w, size).a11y_hidden()
}

const FIG_W: f32 = 440.0;
const FIG_H: f32 = 300.0;
/// "girdle Ø 1.00" in 12 pt mono, with a little room.
const DIA_W: f32 = 98.0;

fn dashed(mut shape: Shape, y: f32, x0: f32, x1: f32, color: Color) -> Shape {
    let mut x = x0;
    while x < x1 {
        shape = shape.line([x, y], [(x + 4.0).min(x1), y], 1.0, LineCap::Butt, color);
        x += 8.0;
    }
    shape
}

/// Figure 1: the gem turning at `angle`, with its drafting annotations, at most `width` wide.
pub fn figure(angle: f32, width: f32) -> El {
    let k = (width / FIG_W).min(1.0);
    let scale = 160.0;
    let (cx, top) = (FIG_W / 2.0 - 10.0, 24.0);
    let (apex, girdle, culet) = (top, top + CROWN * scale, top + (CROWN + PAVILION) * scale);
    let ink = fg2();
    let faint = rule_faint();
    let half = 0.5 * scale;
    let bar = cx + half + 52.0;
    let mut shape = Shape::new().view_box(FIG_W, FIG_H);
    for y in [apex, girdle, culet] {
        shape = dashed(shape, y, 78.0, cx - half - 6.0, faint);
        shape = dashed(shape, y, cx + half + 6.0, bar, faint);
    }
    shape = draw(shape, angle, scale, cx, girdle);
    // The heights: one bar with ticks at the apex, girdle and culet.
    shape = shape.line([bar, apex], [bar, culet], 1.0, LineCap::Butt, ink);
    for y in [apex, girdle, culet] {
        shape = shape.line([bar - 5.0, y], [bar + 5.0, y], 1.0, LineCap::Butt, ink);
    }
    // The girdle's diameter, under the culet.
    let dy = culet + 18.0;
    shape = shape
        .line([cx - half, dy], [cx + half, dy], 1.0, LineCap::Butt, ink)
        .line([cx - half, dy - 5.0], [cx - half, dy + 5.0], 1.0, LineCap::Butt, ink)
        .line([cx + half, dy - 5.0], [cx + half, dy + 5.0], 1.0, LineCap::Butt, ink);
    let label = |key: &str, s: &str, x: f32, y: f32, color: Color| {
        text(s.to_owned()).key(key.to_owned()).font(MONO_12).color(color).absolute(x * k, y * k - 8.0)
    };
    el().key("gem-fig")
        .size(FIG_W * k, FIG_H * k)
        .role("image")
        .label("The simee mark: an eight-sided gem, girdle diameter 1, crown height 0.5, pavilion 1, turning")
        .child(canvas(shape).key("gem-canvas").size(FIG_W * k, FIG_H * k))
        .child(label("l-apex", "apex", 34.0, apex, fg()))
        .child(label("l-girdle", "girdle", 20.0, girdle, fg()))
        .child(label("l-culet", "culet", 27.0, culet, fg()))
        .child(
            col()
                .key("l-crown")
                .absolute((bar + 12.0) * k, (apex + girdle) / 2.0 * k - 16.0)
                .child(text("crown").font(MONO_12).color(fg()))
                .child(text("0.50").font(MONO_12).color(ink)),
        )
        .child(
            col()
                .key("l-pavilion")
                .absolute((bar + 12.0) * k, (girdle + culet) / 2.0 * k - 16.0)
                .child(text("pavilion").font(MONO_12).color(fg()))
                .child(text("1.00").font(MONO_12).color(ink)),
        )
        .child(label("l-h", "h 1.50", bar + 12.0, culet, ink))
        // Scaled down (a phone), the figure is the column's width and this label, which doesn't
        // scale, would stick out past it: it moves left, under the bar, instead.
        .child(label("l-dia", "girdle Ø 1.00", if k < 1.0 { (bar + 12.0).min(FIG_W - DIA_W / k) } else { bar + 12.0 }, dy, ink))
}
