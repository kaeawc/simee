//! A pixel-based WCAG contrast check: find text-like regions in a screenshot, estimate each one's
//! foreground and background colours and report the ones below the contrast WCAG 2.x asks for.
//!
//! No view hierarchy is read, so the check sees what a person sees and nothing else. The pipeline:
//!
//! 1. **Edges.** A pixel is an edge when it differs from its right or lower neighbour by at least
//!    [`EDGE_STEP`] in some channel. The step is small on purpose: faint text is what this finds,
//!    and its anti-aliased edges step by a fraction of its contrast.
//! 2. **Blobs.** Each row's edge pixels are smeared into runs (gaps up to [`gap`] closed: letters
//!    and words join), runs in neighbouring rows that overlap are united (union-find over runs, not
//!    pixels), and each united set is a candidate with a bounding box. Text joins into lines; a
//!    photo or a noise field joins into one huge blob that the size rules drop.
//! 3. **Two-colour fit.** Around a candidate the flat pixels of a ring just outside its box give
//!    the background: a plane per channel when they fit one (a gradient), else their commonest
//!    colour. The box's pixels that differ from it are ink; the ink farthest from the background
//!    is the foreground (the stroke cores, which anti-aliasing doesn't reach). Text is two colours
//!    and the blends between them, so the fraction of ink that lies on the line from background to
//!    foreground ([`PURITY`]) is what separates text and icons from photos and texture.
//! 4. **Ratio.** WCAG's contrast ratio between the foreground and the background, taking the worst
//!    of the background's colours across the box (a gradient). Normal text needs 4.5:1; large text
//!    (about 18 pt) and graphics such as icons 3:1.
//!
//! Limits, stated plainly: text over a photo or a busy background has no single background colour
//! and is skipped, not judged; bold text isn't told from regular, so 14 pt bold text is held to
//! the normal-text ratio; size comes from the ink height of a line, so a line with no ascenders or
//! descenders reads a little smaller than its font size (the stricter threshold).

use crate::frame::Frame;

/// Smallest neighbour difference, in any channel, that makes a pixel an edge.
pub const EDGE_STEP: u8 = 6;
/// A pixel this close (largest channel difference) to its background isn't ink.
const INK_STEP: i32 = 8;
/// The least fraction of a box's ink that must lie on the line from its background to its
/// foreground for the box to count as text or an icon rather than a picture.
pub const PURITY: f64 = 0.85;
/// WCAG 1.4.3: contrast of normal text.
pub const NORMAL_TEXT: f64 = 4.5;
/// WCAG 1.4.3 for large text and 1.4.11 for user interface components and graphics.
pub const LARGE_TEXT: f64 = 3.0;
/// The ink height (points) from which text is large: 18 pt type spans about 17 pt from ascender to
/// descender.
pub const LARGE_INK_PT: f64 = 17.0;
/// The smallest ink height (points) of something worth judging: text's x-height or an icon.
const MIN_INK_PT: f64 = 4.5;
/// The tallest candidate (points). Taller is a picture or a panel, not text.
const MAX_INK_PT: f64 = 80.0;
/// Squarish candidates at least this tall (points) are icons and controls, not text.
const COMPONENT_PT: f64 = 16.0;

/// A colour, 8-bit sRGB.
pub type Rgb = [u8; 3];

/// A rectangle of pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What a finding is judged as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Text under about 18 pt: 4.5:1.
    Text,
    /// Text of about 18 pt and over: 3:1.
    LargeText,
    /// A squarish graphic, an icon or a glyph-like control: 3:1.
    Component,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::LargeText => "large_text",
            Kind::Component => "component",
        }
    }

    /// The least contrast WCAG asks of this kind.
    pub fn required(self) -> f64 {
        if self == Kind::Text { NORMAL_TEXT } else { LARGE_TEXT }
    }
}

/// One text-like region and how it measures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub rect: Rect,
    pub kind: Kind,
    pub foreground: Rgb,
    /// The background colour the ratio was worst against.
    pub background: Rgb,
    pub ratio: f64,
}

impl Region {
    /// Whether the ratio is under what WCAG asks of this kind.
    pub fn fails(&self) -> bool {
        self.ratio < self.kind.required()
    }
}

/// What to check.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Only this part of the frame, in pixels.
    pub region: Option<Rect>,
    /// Pixels per point (an iPhone 3, an Android 2.625): sizes are judged in points. 1 when unknown.
    pub scale: f64,
}

impl Default for Options {
    fn default() -> Options {
        Options { region: None, scale: 1.0 }
    }
}

/// The outcome of a check.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Every text-like region found, passing or not, in reading order (top to bottom).
    pub regions: Vec<Region>,
}

impl Report {
    /// The regions below the contrast WCAG asks of them.
    pub fn findings(&self) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(|r| r.fails())
    }
}

/// The relative luminance of an sRGB colour (WCAG 2.x), 0 (black) to 1 (white).
pub fn luminance([r, g, b]: Rgb) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// The WCAG contrast ratio of two colours, 1 to 21, whichever is lighter.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `#rrggbb`.
pub fn hex([r, g, b]: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// A read-only window onto RGBA rows.
#[derive(Clone, Copy)]
struct Img<'a> {
    data: &'a [u8],
    stride: usize,
    w: usize,
    h: usize,
}

impl<'a> Img<'a> {
    fn row(&self, y: usize) -> &'a [u8] {
        &self.data[y * self.stride * 4..][..self.w * 4]
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.stride + x) * 4;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    /// Whether the pixel differs from its right or lower neighbour by [`EDGE_STEP`].
    fn is_edge(&self, x: usize, y: usize) -> bool {
        let p = self.px(x, y);
        (x + 1 < self.w && step(p, self.px(x + 1, y))) || (y + 1 < self.h && step(p, self.px(x, y + 1)))
    }
}

fn step(a: [u8; 3], b: [u8; 3]) -> bool {
    a[0].abs_diff(b[0]).max(a[1].abs_diff(b[1])).max(a[2].abs_diff(b[2])) >= EDGE_STEP
}

/// The widest gap between edge pixels (pixels) a row smear closes: about 3 pt, which joins the letters
/// of a word (words are joined into lines afterwards).
fn gap(scale: f64) -> usize {
    ((3.0 * scale).round() as usize).max(4)
}

/// How many empty rows between two runs still join them: a dot over an "i" is a point or two away.
fn bridge(scale: f64) -> usize {
    ((1.5 * scale).round() as usize).max(2)
}

#[derive(Clone, Copy)]
struct Run {
    x0: u32,
    x1: u32,
    /// Index of the run's row in the scan.
    y: u32,
    edges: u32,
}

/// A candidate: the united runs' box and edge pixel count.
#[derive(Clone, Copy)]
struct Blob {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    edges: u32,
}

fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        parent[i as usize] = parent[parent[i as usize] as usize];
        i = parent[i as usize];
    }
    i
}

/// Candidate blobs: smeared edge runs united across neighbouring rows.
fn blobs(img: &Img, scale: f64) -> Vec<Blob> {
    let (gap, bridge) = (gap(scale), bridge(scale));
    let mut runs: Vec<Run> = Vec::new();
    let mut row_start: Vec<usize> = Vec::with_capacity(img.h + 1);
    let mut parent: Vec<u32> = Vec::new();
    let mut edge = vec![false; img.w];
    for y in 0..img.h {
        row_start.push(runs.len());
        let (row, below) = (img.row(y), (y + 1 < img.h).then(|| img.row(y + 1)));
        for x in 0..img.w {
            let p = &row[x * 4..x * 4 + 3];
            let right = x + 1 < img.w && step([p[0], p[1], p[2]], [row[x * 4 + 4], row[x * 4 + 5], row[x * 4 + 6]]);
            let down = below.is_some_and(|b| step([p[0], p[1], p[2]], [b[x * 4], b[x * 4 + 1], b[x * 4 + 2]]));
            edge[x] = right || down;
        }
        let first = runs.len();
        let mut x = 0;
        while x < img.w {
            if !edge[x] {
                x += 1;
                continue;
            }
            let (x0, mut last, mut edges) = (x, x, 1u32);
            x += 1;
            while x < img.w && x <= last + gap + 1 {
                if edge[x] {
                    last = x;
                    edges += 1;
                }
                x += 1;
            }
            runs.push(Run { x0: x0 as u32, x1: last as u32, y: y as u32, edges });
            parent.push(parent.len() as u32);
        }
        // Unite with the runs of the rows above (this one's neighbours and the bridged rows).
        for py in y.saturating_sub(bridge + 1)..y {
            let (from, to) = (row_start[py], row_start[py + 1]);
            let mut j = from;
            for i in first..runs.len() {
                while j < to && runs[j].x1 < runs[i].x0 {
                    j += 1;
                }
                let mut k = j;
                while k < to && runs[k].x0 <= runs[i].x1 {
                    let (a, b) = (find(&mut parent, i as u32), find(&mut parent, k as u32));
                    if a != b {
                        parent[a as usize] = b;
                    }
                    k += 1;
                }
            }
        }
    }
    let mut by_root: std::collections::HashMap<u32, Blob> = std::collections::HashMap::new();
    for (i, &r) in runs.iter().enumerate() {
        let root = find(&mut parent, i as u32);
        let b = by_root.entry(root).or_insert(Blob { x0: r.x0, y0: r.y, x1: r.x1, y1: r.y, edges: 0 });
        b.x0 = b.x0.min(r.x0);
        b.x1 = b.x1.max(r.x1);
        b.y0 = b.y0.min(r.y);
        b.y1 = b.y1.max(r.y);
        b.edges += r.edges;
    }
    let mut out: Vec<Blob> = by_root.into_values().collect();
    out.sort_by_key(|b| (b.y0, b.x0));
    out
}

/// A background model: a plane per channel, `a + bx*u + by*v` with `u`, `v` in -0.5..0.5 across the box.
#[derive(Clone, Copy)]
struct Plane {
    a: [f32; 3],
    bx: [f32; 3],
    by: [f32; 3],
}

impl Plane {
    fn flat(c: [f32; 3]) -> Plane {
        Plane { a: c, bx: [0.0; 3], by: [0.0; 3] }
    }

    fn at(&self, u: f32, v: f32) -> [f32; 3] {
        std::array::from_fn(|c| self.a[c] + self.bx[c] * u + self.by[c] * v)
    }
}

/// The least squares plane through `samples` (u, v, colour) and its worst channel's rms residual;
/// `None` when the samples don't pin a plane down.
fn fit_plane(samples: &[(f32, f32, [u8; 3])]) -> Option<(Plane, f32)> {
    let n = samples.len() as f64;
    let (mut su, mut sv, mut suu, mut svv, mut suv) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(u, v, _) in samples {
        let (u, v) = (f64::from(u), f64::from(v));
        (su, sv, suu, svv, suv) = (su + u, sv + v, suu + u * u, svv + v * v, suv + u * v);
    }
    // Normal equations [n su sv; su suu suv; sv suv svv] * [a bx by] = [sc scu scv], by Cramer.
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let m = [[n, su, sv], [su, suu, suv], [sv, suv, svv]];
    let d = det(m);
    if d.abs() < 1e-9 {
        return None;
    }
    let mut plane = Plane::flat([0.0; 3]);
    let mut worst = 0.0f32;
    for c in 0..3 {
        let (mut sc, mut scu, mut scv) = (0.0, 0.0, 0.0);
        for &(u, v, col) in samples {
            let value = f64::from(col[c]);
            (sc, scu, scv) = (sc + value, scu + value * f64::from(u), scv + value * f64::from(v));
        }
        let rhs = [sc, scu, scv];
        let solve = |col: usize| {
            let mut mm = m;
            for r in 0..3 {
                mm[r][col] = rhs[r];
            }
            (det(mm) / d) as f32
        };
        (plane.a[c], plane.bx[c], plane.by[c]) = (solve(0), solve(1), solve(2));
        let sq: f32 = samples.iter().map(|&(u, v, col)| (plane.at(u, v)[c] - f32::from(col[c])).powi(2)).sum();
        worst = worst.max((sq / samples.len() as f32).sqrt());
    }
    Some((plane, worst))
}

/// The commonest colour among `samples` (4 bits a channel), averaged over its bin.
fn mode_colour(samples: impl Iterator<Item = [u8; 3]> + Clone) -> Option<[f32; 3]> {
    let key = |c: [u8; 3]| usize::from(c[0] >> 4) << 8 | usize::from(c[1] >> 4) << 4 | usize::from(c[2] >> 4);
    let mut counts = vec![0u32; 4096];
    for c in samples.clone() {
        counts[key(c)] += 1;
    }
    let (best, &n) = counts.iter().enumerate().max_by_key(|&(i, &n)| (n, std::cmp::Reverse(i)))?;
    if n == 0 {
        return None;
    }
    let (mut sum, mut count) = ([0f32; 3], 0f32);
    for c in samples.filter(|&c| key(c) == best) {
        (0..3).for_each(|i| sum[i] += f32::from(c[i]));
        count += 1.0;
    }
    Some(sum.map(|s| s / count))
}

/// The background around `rect`: the flat pixels of a ring outside it (a plane when they fit
/// one, else their commonest colour), or the commonest colour inside when the ring is no use.
fn background(img: &Img, rect: Rect) -> Option<Plane> {
    let (x0, y0, x1, y1) = (rect.x as i64, rect.y as i64, (rect.x + rect.width) as i64, (rect.y + rect.height) as i64);
    let (gapw, thick) = (2i64, 3i64);
    let (wf, hf) = (rect.width as f32, rect.height as f32);
    let mut samples: Vec<(f32, f32, [u8; 3])> = Vec::new();
    let (ox0, oy0, ox1, oy1) = (x0 - gapw - thick, y0 - gapw - thick, x1 + gapw + thick, y1 + gapw + thick);
    // About 4000 samples at most: step along long sides.
    let step_by = (((ox1 - ox0 + oy1 - oy0) * 2 * thick / 4000).max(1)) as usize;
    let mut take = |x: i64, y: i64| {
        if x >= 0 && y >= 0 && (x as usize) < img.w && (y as usize) < img.h && !img.is_edge(x as usize, y as usize) {
            samples.push((
                (x as f32 + 0.5 - x0 as f32) / wf - 0.5,
                (y as f32 + 0.5 - y0 as f32) / hf - 0.5,
                img.px(x as usize, y as usize),
            ));
        }
    };
    for y in (oy0..oy1).step_by(step_by) {
        let inner = y >= y0 - gapw && y < y1 + gapw;
        for x in ox0..ox1 {
            if !inner || x < x0 - gapw || x >= x1 + gapw {
                take(x, y);
            }
        }
    }
    if samples.len() >= 8 {
        let colours = || samples.iter().map(|s| s.2);
        let (lo, hi) = colours().fold(([255u8; 3], [0u8; 3]), |(lo, hi), c| {
            (std::array::from_fn(|i| lo[i].min(c[i])), std::array::from_fn(|i| hi[i].max(c[i])))
        });
        let range = (0..3).map(|i| hi[i] - lo[i]).max().unwrap_or(0);
        if range <= 4 {
            return mode_colour(colours()).map(Plane::flat);
        }
        if let Some((plane, rms)) = fit_plane(&samples)
            && rms <= 3.0
        {
            return Some(plane);
        }
        let mode = mode_colour(colours())?;
        let near = |c: [u8; 3]| (0..3).all(|i| (f32::from(c[i]) - mode[i]).abs() <= 24.0);
        return mode_colour(colours().filter(|&c| near(c))).map(Plane::flat);
    }
    let inside = (rect.y..rect.y + rect.height).flat_map(|y| (rect.x..rect.x + rect.width).map(move |x| img.px(x as usize, y as usize)));
    mode_colour(inside).map(Plane::flat)
}

/// What the two-colour fit of a candidate found.
struct Fit {
    fg: Rgb,
    background: Plane,
    ink: usize,
    area: usize,
    purity: f64,
}

fn round(c: [f32; 3]) -> Rgb {
    c.map(|v| v.round().clamp(0.0, 255.0) as u8)
}

fn fit(img: &Img, rect: Rect) -> Option<Fit> {
    let bg = background(img, rect)?;
    let (w, h) = (rect.width as usize, rect.height as usize);
    let at = |i: usize| (i % w, i / w);
    let local = |x: usize, y: usize| bg.at((x as f32 + 0.5) / w as f32 - 0.5, (y as f32 + 0.5) / h as f32 - 0.5);
    let distance = |x: usize, y: usize| {
        let (p, b) = (img.px(rect.x as usize + x, rect.y as usize + y), local(x, y));
        (0..3).map(|c| (f32::from(p[c]) - b[c]).abs()).fold(0.0, f32::max)
    };
    let mut hist = [0u32; 256];
    let mut ink = 0usize;
    for i in 0..w * h {
        let (x, y) = at(i);
        let d = distance(x, y);
        if d > INK_STEP as f32 {
            hist[d.min(255.0) as usize] += 1;
            ink += 1;
        }
    }
    if ink == 0 {
        return None;
    }
    // The strongest ink, less the top 3% (specks): the stroke cores are within 20% of it.
    let (mut seen, mut top) = (0usize, 0usize);
    for d in (0..256).rev() {
        seen += hist[d] as usize;
        if seen * 100 >= ink * 3 {
            top = d;
            break;
        }
    }
    let threshold = top as f32 * 0.8;
    let (mut sum, mut count) = ([0f64; 3], 0f64);
    for i in 0..w * h {
        let (x, y) = at(i);
        if distance(x, y) >= threshold.max(INK_STEP as f32 + 1.0) {
            let p = img.px(rect.x as usize + x, rect.y as usize + y);
            (0..3).for_each(|c| sum[c] += f64::from(p[c]));
            count += 1.0;
        }
    }
    if count == 0.0 {
        return None;
    }
    let fgf = sum.map(|s| (s / count) as f32);
    let mut on_line = 0usize;
    for i in 0..w * h {
        let (x, y) = at(i);
        if distance(x, y) <= INK_STEP as f32 {
            continue;
        }
        let (p, b) = (img.px(rect.x as usize + x, rect.y as usize + y), local(x, y));
        let (u, v): ([f32; 3], [f32; 3]) = (std::array::from_fn(|c| fgf[c] - b[c]), std::array::from_fn(|c| f32::from(p[c]) - b[c]));
        let uu: f32 = u.iter().map(|a| a * a).sum();
        let t = if uu > 0.0 { (u.iter().zip(&v).map(|(a, b)| a * b).sum::<f32>() / uu).clamp(0.0, 1.2) } else { 0.0 };
        let off = (0..3).map(|c| (v[c] - t * u[c]).abs()).fold(0.0, f32::max);
        let reach = u.iter().map(|a| a.abs()).fold(0.0, f32::max);
        if off <= 0.18 * reach + 8.0 {
            on_line += 1;
        }
    }
    Some(Fit { fg: round(fgf), background: bg, ink, area: w * h, purity: on_line as f64 / ink as f64 })
}

/// Checks a frame: every text-like region and its contrast.
pub fn check(frame: &Frame, opts: &Options) -> Report {
    let scale = if opts.scale.is_finite() && opts.scale > 0.0 { opts.scale } else { 1.0 };
    let full = Rect { x: 0, y: 0, width: frame.width, height: frame.height };
    let area = opts.region.map_or(full, |r| {
        let (x, y) = (r.x.min(frame.width), r.y.min(frame.height));
        Rect { x, y, width: r.width.min(frame.width - x), height: r.height.min(frame.height - y) }
    });
    let have = frame.width as usize * frame.height as usize * 4;
    if area.width == 0 || area.height == 0 || frame.rgba.len() < have {
        return Report { regions: Vec::new() };
    }
    let stride = frame.width as usize;
    let img = Img {
        data: &frame.rgba[(area.y as usize * stride + area.x as usize) * 4..],
        stride,
        w: area.width as usize,
        h: area.height as usize,
    };
    let min_px = (MIN_INK_PT * scale).max(5.0);
    let mut regions = Vec::new();
    let mut frames = Vec::new();
    for b in blobs(&img, scale) {
        let (w, h) = (f64::from(b.x1 - b.x0 + 1), f64::from(b.y1 - b.y0 + 1));
        let pt = h / scale;
        // Dividers and scroll bars (thin), panels and pictures (huge), edge-dense noise.
        if h < min_px || w < min_px * 0.4 || pt > MAX_INK_PT || w / h > 60.0 || h / w > 6.0 || b.edges < 10 {
            continue;
        }
        // Judged against the frame, not the region: a region drawn tightly round a label is not a panel.
        if w * h > 0.3 * f64::from(frame.width) * f64::from(frame.height) {
            continue;
        }
        let rect = Rect { x: b.x0, y: b.y0, width: b.x1 - b.x0 + 1, height: b.y1 - b.y0 + 1 };
        // A hollow box's border is its own finding, a non-text element (3:1) against what is outside
        // it; the text inside is judged on its own against the box's fill.
        if let Some((edge, outside)) = thin_frame(&img, rect) {
            let rect = Rect { x: rect.x + area.x, y: rect.y + area.y, ..rect };
            frames.push(rect);
            regions.push(Region {
                rect,
                kind: Kind::Component,
                foreground: edge,
                background: outside,
                ratio: contrast_ratio(edge, outside),
            });
            continue;
        }
        let Some(f) = fit(&img, rect) else { continue };
        let fraction = f.ink as f64 / f.area as f64;
        if f.ink < 8 || !(0.03..=0.75).contains(&fraction) || f.purity < PURITY {
            continue;
        }
        let kind = classify(rect, scale);
        // The worst background across the box: its centre and near-corners (a gradient moves).
        let worst = [(0.0, 0.0), (-0.4, -0.4), (0.4, -0.4), (-0.4, 0.4), (0.4, 0.4)]
            .iter()
            .map(|&(u, v)| round(f.background.at(u, v)))
            .map(|bg| (contrast_ratio(f.fg, bg), bg))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .expect("five samples");
        regions.push(Region {
            rect: Rect { x: rect.x + area.x, y: rect.y + area.y, ..rect },
            kind,
            foreground: f.fg,
            background: worst.1,
            ratio: worst.0,
        });
    }
    Report { regions: lines(regions, scale, &frames) }
}

fn near(a: Rgb, b: Rgb, tol: u8) -> bool {
    (0..3).all(|i| a[i].abs_diff(b[i]) <= tol)
}

/// A hollow box (a field, a card, an outlined button, a pill): a uniform line of one colour along
/// all four sides, different from what is outside, and not the same colour a little way in (that is
/// a filled shape). The line follows the box's outline, so rounded and capsule corners count; it is
/// thin next to the box, so a big hollow letter or a ring icon does not. Returns the border colour
/// and the colour outside it.
fn thin_frame(img: &Img, r: Rect) -> Option<(Rgb, Rgb)> {
    let (w, h) = (i64::from(r.width), i64::from(r.height));
    if w < 12 || h < 12 {
        return None;
    }
    let (x0, y0) = (i64::from(r.x), i64::from(r.y));
    let (x1, y1) = (x0 + w - 1, y0 + h - 1);
    let px = |x: i64, y: i64| -> Option<Rgb> {
        (x >= 0 && y >= 0 && (x as usize) < img.w && (y as usize) < img.h).then(|| img.px(x as usize, y as usize))
    };
    let (xs, ys) = (w / 5..w * 4 / 5, h / 5..h * 4 / 5);
    let outside: Vec<Rgb> = xs
        .clone()
        .flat_map(|i| [px(x0 + i, y0 - 3), px(x0 + i, y1 + 3)])
        .chain(ys.clone().flat_map(|j| [px(x0 - 3, y0 + j), px(x1 + 3, y0 + j)]))
        .flatten()
        .collect();
    if outside.len() < 8 {
        return None;
    }
    let out = round(mode_colour(outside.iter().copied())?);
    if outside.iter().filter(|&&c| near(c, out, 12)).count() * 10 < outside.len() * 8 {
        return None;
    }
    // A border is thin next to the box (a stroke of a big letter or a filled shape is not), on at least
    // three sides (a drop shadow thickens the fourth), and starts near its side unless the outline
    // curves away there.
    let min_side = w.min(h);
    let thickest = min_side / 10 + 1;
    let reach = min_side / 6 + 3;
    // Where the scan `i` along a side crosses the border: how far in it starts, its colour (the more
    // inked of its first two pixels: an antialiased edge is a blend) and its thickness.
    let probe = |side: usize, i: i64| -> Option<(i64, Rgb, i64)> {
        let at = |d: i64| match side {
            0 => px(x0 + i, y0 + d),
            1 => px(x0 + i, y1 - d),
            2 => px(x0 + d, y0 + i),
            _ => px(x1 - d, y0 + i),
        };
        let d = (0..=reach).find(|&d| at(d).is_some_and(|c| !near(c, out, INK_STEP as u8)))?;
        let ink = |c: Rgb| (0..3).map(|k| c[k].abs_diff(out[k])).max().unwrap_or(0);
        // The side of the outside colour a pixel is on: a shadow is darker than the page, a card lighter.
        let above = |c: Rgb| (0..3).map(|k| i32::from(c[k]) - i32::from(out[k])).sum::<i32>() > 0;
        let first = at(d)?;
        let (start, c) = match at(d + 1) {
            Some(next) if above(next) == above(first) && ink(next) > ink(first) => (d + 1, next),
            _ => (d, first),
        };
        // A faint border is told from the inside by less than a bright one: half its own step from outside.
        let tol = (ink(c) / 2).clamp(2, 12);
        let run = (start..=start + 2 * thickest).take_while(|&d| at(d).is_some_and(|p| near(p, c, tol))).count() as i64;
        Some((d, c, run))
    };
    // Each side is judged on its own: one colour along its length, but opposite sides may differ (a
    // card's drop shadow darkens its bottom edge).
    let mut sides = [[0u8; 3]; 4];
    let mut thin_sides = 0;
    let mut curve = [0; 4];
    let mut squared = true;
    let mut straight = [0; 2];
    for side in 0..4 {
        let span = if side < 2 { xs.clone() } else { ys.clone() };
        let probes: Vec<Option<(i64, Rgb, i64)>> = span.map(|i| probe(side, i)).collect();
        let c = round(mode_colour(probes.iter().flatten().map(|p| p.1))?);
        let mine = |p: &Option<(i64, Rgb, i64)>| p.is_some_and(|(_, pc, run)| near(pc, c, 12) && run <= 2 * thickest);
        if probes.iter().filter(|p| mine(p)).count() * 10 < probes.len() * 9 {
            return None;
        }
        sides[side] = c;
        let mut runs: Vec<i64> = probes.iter().flatten().map(|p| p.2).collect();
        runs.sort_unstable();
        thin_sides += usize::from(runs[runs.len() / 2] <= thickest);
        let depths: Vec<Option<i64>> = probes.iter().map(|p| p.map(|p| p.0)).collect();
        let nearest = depths.iter().flatten().copied().min()?;
        if nearest > 3 {
            return None;
        }
        curve[side] = depths.iter().flatten().copied().max()? - nearest;
        // The top and bottom: the longest run where the border is as near as it gets, a straight edge.
        // A pill has one between its ends; a circle has only a short one.
        if side < 2 {
            let (mut longest, mut now) = (0, 0);
            for d in &depths {
                now = if *d == Some(nearest) { now + 1 } else { 0 };
                longest = longest.max(now);
            }
            straight[side] = longest;
            // Whether the border turns away before the corners: the end of the side is deeper in.
            squared &= [2, w - 3].iter().all(|&i| probe(side, i).is_some_and(|p| p.0 <= nearest + 1));
        }
    }
    if thin_sides < 3 {
        return None;
    }
    // The sides share a hue of line (a shadow's darker edge is still the box's), the way a pill's two
    // ends curve alike and a D's do not: a D is no frame.
    if sides.iter().any(|&c| !near(c, sides[0], 48)) || curve[2].abs_diff(curve[3]) > 1 {
        return None;
    }
    // A box whose sides are straight right out to square corners is a box. One with a curved side or
    // rounded corners (a pill, a ring, an O) must leave a straight run along the top and bottom that is a
    // good part of the width and not a few pixels: a circle's flat is about `2 * sqrt(r)` wide, and the
    // smaller it is the flatter its curve looks.
    let wanted = if squared && curve.iter().all(|&c| c <= 1) { w * 2 / 5 } else { (w * 2 / 5).max(12) };
    if straight.iter().any(|&run| run < wanted) {
        return None;
    }
    let edge = sides.into_iter().max_by(|a, b| contrast_ratio(*a, out).total_cmp(&contrast_ratio(*b, out)))?;
    Some((edge, out))
}

/// What a box of ink is judged as: squarish and tall enough is an icon or control, else text,
/// large from about 18 pt.
fn classify(rect: Rect, scale: f64) -> Kind {
    let (w, h) = (f64::from(rect.width), f64::from(rect.height));
    let pt = h / scale;
    if w / h < 2.2 && pt >= COMPONENT_PT {
        Kind::Component
    } else if pt >= LARGE_INK_PT {
        Kind::LargeText
    } else {
        Kind::Text
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// Whether two regions are neighbouring glyphs or words of one line: overlapping mostly in height,
/// within a line's height of each other, and the same two colours (a letter's
/// counter reads them swapped: its ring is the ink around it).
fn same_line(a: &Region, b: &Region) -> bool {
    let near = |p: Rgb, q: Rgb| (0..3).all(|i| p[i].abs_diff(q[i]) <= 16);
    let (top, bottom) = (a.rect.y.max(b.rect.y), (a.rect.y + a.rect.height).min(b.rect.y + b.rect.height));
    let (shorter, taller) = (a.rect.height.min(b.rect.height), a.rect.height.max(b.rect.height));
    let gap = (i64::from(a.rect.x.max(b.rect.x)) - i64::from((a.rect.x + a.rect.width).min(b.rect.x + b.rect.width))).max(0);
    bottom > top
        && (bottom - top) * 2 >= shorter
        && gap <= i64::from(taller)
        && ((near(a.foreground, b.foreground) && near(a.background, b.background))
            || (near(a.foreground, b.background) && near(a.background, b.foreground)))
}

/// Joins the glyphs and words of a line into one region (the worst ratio, the kind of what the
/// line spans), and drops fragments inside another region (the dot of an "i" that didn't join its
/// letter, the inside of a checkbox).
fn lines(regions: Vec<Region>, scale: f64, frames: &[Rect]) -> Vec<Region> {
    let mut regions = regions;
    loop {
        let before = regions.len();
        let mut out: Vec<Region> = Vec::with_capacity(regions.len());
        for r in regions {
            // A hollow box's border neither swallows what is inside it nor joins its line.
            if out.iter().any(|o| contains(o.rect, r.rect) && !frames.contains(&o.rect)) {
                continue;
            }
            let is_frame = frames.contains(&r.rect);
            match out.iter_mut().rev().take(96).find(|o| !is_frame && !frames.contains(&o.rect) && same_line(o, &r)) {
                Some(o) => {
                    let (x0, y0) = (o.rect.x.min(r.rect.x), o.rect.y.min(r.rect.y));
                    let x1 = (o.rect.x + o.rect.width).max(r.rect.x + r.rect.width);
                    let y1 = (o.rect.y + o.rect.height).max(r.rect.y + r.rect.height);
                    if r.ratio < o.ratio {
                        (o.ratio, o.foreground, o.background) = (r.ratio, r.foreground, r.background);
                    }
                    o.rect = Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 };
                    o.kind = classify(o.rect, scale);
                }
                None => out.push(r),
            }
        }
        regions = out;
        if regions.len() == before {
            return regions;
        }
    }
}

/// A copy of `frame` with a red box around each of `regions` (outside the region, so the pixels
/// that were measured stay visible), 2 pixels thick, more on a dense screen.
pub fn annotate<'a>(frame: &Frame, regions: impl IntoIterator<Item = &'a Region>) -> Frame {
    let mut out = frame.clone();
    let thick = (frame.width.max(frame.height) / 600).max(2) as i64;
    let (w, h) = (i64::from(frame.width), i64::from(frame.height));
    let mut put = |x: i64, y: i64| {
        if (0..w).contains(&x) && (0..h).contains(&y) {
            let i = (y * w + x) as usize * 4;
            out.rgba[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
        }
    };
    for r in regions {
        let (x0, y0) = (i64::from(r.rect.x) - thick, i64::from(r.rect.y) - thick);
        let (x1, y1) = (i64::from(r.rect.x + r.rect.width) + thick, i64::from(r.rect.y + r.rect.height) + thick);
        for t in 0..thick {
            for x in x0 + t..x1 - t {
                put(x, y0 + t);
                put(x, y1 - 1 - t);
            }
            for y in y0 + t..y1 - t {
                put(x0 + t, y);
                put(x1 - 1 - t, y);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
