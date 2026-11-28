//! A small supersampling polygon rasterizer for the mark's PNGs. Each sample is inside exactly
//! one of two facets that share an edge (half-open spans), so neighbouring facets meet with no
//! seam of background between them, which edge-antialiased fills can't promise.

/// An RGBA image drawn at `samples`×`samples` points per pixel, then averaged down.
pub struct Canvas {
    size: u32,
    samples: u32,
    /// Straight-alpha sRGB, one per sample.
    buf: Vec<[u8; 4]>,
}

impl Canvas {
    /// A transparent `size`×`size` canvas; small sizes take more samples per pixel.
    pub fn new(size: u32) -> Canvas {
        let samples = (4096 / size.max(1)).clamp(4, 8);
        let side = (size * samples) as usize;
        Canvas { size, samples, buf: vec![[0; 4]; side * side] }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// Fills the polygon through `points` (in pixels, either winding, even-odd) with `rgba`.
    pub fn fill(&mut self, points: &[[f32; 2]], rgba: [u8; 4]) {
        self.fill_with(points, |_, _| rgba);
    }

    /// Fills the polygon with a colour chosen per sample from its position in pixels.
    pub fn fill_with(&mut self, points: &[[f32; 2]], colour: impl Fn(f32, f32) -> [u8; 4]) {
        if points.len() < 3 {
            return;
        }
        let ss = self.samples as f32;
        let side = (self.size * self.samples) as i64;
        let (lo, hi) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
        let row0 = ((lo * ss - 0.5).ceil() as i64).max(0);
        let row1 = ((hi * ss - 0.5).ceil() as i64).min(side);
        let mut xs = Vec::with_capacity(8);
        for row in row0..row1 {
            let y = (row as f32 + 0.5) / ss;
            xs.clear();
            for k in 0..points.len() {
                let (a, b) = (points[k], points[(k + 1) % points.len()]);
                if (a[1] <= y && y < b[1]) || (b[1] <= y && y < a[1]) {
                    xs.push(a[0] + (y - a[1]) * (b[0] - a[0]) / (b[1] - a[1]));
                }
            }
            xs.sort_by(f32::total_cmp);
            for span in xs.as_chunks::<2>().0 {
                let c0 = ((span[0] * ss - 0.5).ceil() as i64).max(0);
                let c1 = ((span[1] * ss - 0.5).ceil() as i64).min(side);
                for col in c0..c1 {
                    let src = colour((col as f32 + 0.5) / ss, y);
                    let at = (row * side + col) as usize;
                    self.buf[at] = over(src, self.buf[at]);
                }
            }
        }
    }

    /// The image as straight-alpha RGBA rows, each pixel the average of its samples.
    pub fn into_rgba(self) -> Vec<u8> {
        let (n, s) = (self.size as usize, self.samples as usize);
        let side = n * s;
        let mut out = Vec::with_capacity(n * n * 4);
        for py in 0..n {
            for px in 0..n {
                let mut acc = [0.0f32; 4];
                for sy in 0..s {
                    for sx in 0..s {
                        let p = self.buf[(py * s + sy) * side + px * s + sx];
                        let a = f32::from(p[3]) / 255.0;
                        (0..3).for_each(|k| acc[k] += f32::from(p[k]) * a);
                        acc[3] += a;
                    }
                }
                let a = acc[3] / (s * s) as f32;
                let rgb: [u8; 3] =
                    std::array::from_fn(|k| if acc[3] > 0.0 { (acc[k] / acc[3]).round().clamp(0.0, 255.0) as u8 } else { 0 });
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (a * 255.0).round() as u8]);
            }
        }
        out
    }
}

/// `src` over `dst`, straight alpha.
fn over(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    if src[3] == 255 || dst[3] == 0 {
        return src;
    }
    let (sa, da) = (f32::from(src[3]) / 255.0, f32::from(dst[3]) / 255.0);
    let a = sa + da * (1.0 - sa);
    if a == 0.0 {
        return [0; 4];
    }
    let c: [u8; 3] = std::array::from_fn(|k| ((f32::from(src[k]) * sa + f32::from(dst[k]) * da * (1.0 - sa)) / a).round() as u8);
    [c[0], c[1], c[2], (a * 255.0).round() as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(rgba: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn a_square_fills_its_pixels_and_half_covers_its_edge() {
        let mut c = Canvas::new(8);
        c.fill(&[[2.0, 2.0], [6.0, 2.0], [6.0, 6.0], [2.0, 6.0]], [255, 0, 0, 255]);
        let img = c.into_rgba();
        assert_eq!(pixel(&img, 8, 3, 3), [255, 0, 0, 255]);
        assert_eq!(pixel(&img, 8, 1, 3)[3], 0);
        let mut c = Canvas::new(8);
        c.fill(&[[2.5, 0.0], [8.0, 0.0], [8.0, 8.0], [2.5, 8.0]], [0, 0, 255, 255]);
        assert_eq!(pixel(&c.into_rgba(), 8, 2, 4)[3], 128, "half a pixel covered");
    }

    /// Two triangles sharing a diagonal cover every sample once: no seam, no double cover.
    #[test]
    fn neighbours_leave_no_seam() {
        let mut c = Canvas::new(16);
        c.fill(&[[0.0, 0.0], [16.0, 0.0], [0.0, 16.0]], [10, 200, 100, 255]);
        c.fill(&[[16.0, 0.0], [16.0, 16.0], [0.0, 16.0]], [10, 200, 100, 255]);
        let img = c.into_rgba();
        assert!(img.as_chunks::<4>().0.iter().all(|p| *p == [10, 200, 100, 255]));
    }

    #[test]
    fn translucent_fills_blend_over_what_is_there() {
        let mut c = Canvas::new(4);
        let all = [[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        c.fill(&all, [0, 0, 0, 255]);
        c.fill(&all, [255, 255, 255, 128]);
        let p = pixel(&c.into_rgba(), 4, 1, 1);
        assert_eq!(p[3], 255);
        assert!((127..=129).contains(&p[0]), "{p:?}");
    }
}
