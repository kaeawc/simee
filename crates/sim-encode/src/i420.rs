//! Frames for a software encoder: packed 32-bit pixels in, planar 4:2:0 out, with the downscale
//! [`crate::EncoderConfig::max_size`] asks for. VideoToolbox does both on the GPU; a software
//! encoder gets them from here, on buffers this module keeps between frames, so a steady stream
//! allocates nothing per frame.
//!
//! The colour is BT.709, limited range (Y 16..=235, chroma 16..=240), what a browser assumes for
//! HD video and what the encoder signals in the SPS. Each chroma sample is the mean of its 2x2
//! block. Pure logic: it builds and tests on every host.

use crate::{Error, Result};

/// Where red and blue sit in each 4-byte pixel (the alpha byte is ignored; frames are opaque).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelOrder {
    /// B, G, R, A: what an iOS framebuffer and a camera frame hold.
    Bgra,
    /// R, G, B, A: what emu's display socket and frame ring carry.
    Rgba,
}

/// A frame to encode: `height` rows of `width` pixels, `stride` bytes apart.
#[derive(Clone, Debug)]
pub struct Raster<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub order: PixelOrder,
}

impl Raster<'_> {
    fn validate(&self) -> Result<()> {
        let row = self.width as usize * 4;
        if self.width == 0 || self.height == 0 || self.stride < row {
            return Err(Error::InvalidInput(format!("a {}x{} frame with {}-byte rows", self.width, self.height, self.stride)));
        }
        let need = self.stride * (self.height as usize - 1) + row;
        if self.data.len() < need {
            return Err(Error::InvalidInput(format!("a {}x{} frame needs {need} bytes, got {}", self.width, self.height, self.data.len())));
        }
        Ok(())
    }

    fn row(&self, y: usize) -> &[u8] {
        &self.data[y * self.stride..][..self.width as usize * 4]
    }
}

/// A frame a software encoder takes: owned pixels with their geometry, what a frame source hands
/// over.
#[derive(Clone, Debug)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next (at least `width * 4`).
    pub stride: usize,
    pub order: PixelOrder,
    pub data: Vec<u8>,
}

impl Picture {
    /// A tightly packed picture.
    pub fn new(width: u32, height: u32, order: PixelOrder, data: Vec<u8>) -> Picture {
        Picture { width, height, stride: width as usize * 4, order, data }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn raster(&self) -> Raster<'_> {
        Raster { data: &self.data, width: self.width, height: self.height, stride: self.stride, order: self.order }
    }
}

/// Planar 4:2:0 at an even size, tightly packed: `width * height` luma, then two
/// `width / 2 * height / 2` chroma planes.
#[derive(Clone, Debug, Default)]
pub struct I420 {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420 {
    pub fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    pub fn y(&self) -> &[u8] {
        &self.y
    }

    pub fn u(&self) -> &[u8] {
        &self.u
    }

    pub fn v(&self) -> &[u8] {
        &self.v
    }

    /// Bytes per row of the luma plane (the chroma planes' is half).
    pub fn stride(&self) -> usize {
        self.width
    }

    fn resize(&mut self, width: usize, height: usize) {
        (self.width, self.height) = (width, height);
        self.y.resize(width * height, 0);
        self.u.resize(width / 2 * (height / 2), 0);
        self.v.resize(width / 2 * (height / 2), 0);
    }
}

/// Converts and scales frames into a reused [`I420`].
#[derive(Debug, Default)]
pub struct Converter {
    planes: I420,
    /// The frame scaled to the output size, in the input's pixel order; unused at 1:1.
    scaled: Vec<u8>,
    /// For each output column and tap, the byte offset of the source pixel it reads.
    columns: Vec<usize>,
    /// The sum of a pixel row's taps, a row at a time.
    line: Vec<u16>,
}

/// The most source samples per output pixel along one axis.
const MAX_TAPS: usize = 4;

impl Converter {
    pub fn new() -> Converter {
        Converter::default()
    }

    /// The frame as 4:2:0 at `out` (even sides, no larger than the frame, and so at most a pixel
    /// short of it unless it is a downscale: see [`crate::output_size`], whose result this takes).
    /// A frame whose size is `out` or one pixel more per side is cropped; anything else is
    /// scaled down with a box filter of up to four taps per axis.
    pub fn convert(&mut self, frame: &Raster<'_>, out: (u32, u32)) -> Result<&I420> {
        frame.validate()?;
        let (ow, oh) = (out.0 as usize, out.1 as usize);
        if ow == 0 || oh == 0 || !ow.is_multiple_of(2) || !oh.is_multiple_of(2) {
            return Err(Error::InvalidInput(format!("a {ow}x{oh} output: sides must be even and nonzero")));
        }
        if ow > frame.width as usize || oh > frame.height as usize {
            return Err(Error::InvalidInput(format!(
                "a {ow}x{oh} output from a {}x{} frame: scaling up isn't supported",
                frame.width, frame.height
            )));
        }
        self.planes.resize(ow, oh);
        let crop = frame.width as usize - ow <= 1 && frame.height as usize - oh <= 1;
        if crop {
            to_planes(frame, &mut self.planes);
        } else {
            self.scaled.resize(ow * oh * 4, 0);
            scale(frame, &mut self.scaled, (ow, oh), &mut self.columns, &mut self.line);
            let scaled = Raster { data: &self.scaled, width: out.0, height: out.1, stride: ow * 4, order: frame.order };
            to_planes(&scaled, &mut self.planes);
        }
        Ok(&self.planes)
    }
}

/// How many taps per axis cover `source` pixels per output pixel.
fn taps(source: usize, output: usize) -> usize {
    source.div_ceil(output).clamp(1, MAX_TAPS)
}

/// A box-ish downscale: each output pixel is the mean of an `nx` x `ny` grid of source pixels
/// spread evenly over the area it covers (exact area averaging at integer ratios, a close one
/// elsewhere), the colour order unchanged. The grid's rows are summed a whole row at a time first
/// (a loop the compiler vectorises), then each output pixel adds its `nx` columns of that sum.
/// `columns` and `line` are scratch.
fn scale(src: &Raster<'_>, dst: &mut [u8], (ow, oh): (usize, usize), columns: &mut Vec<usize>, line: &mut Vec<u16>) {
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (nx, ny) = (taps(sw, ow), taps(sh, oh));
    // Tap `t` of `n` over output pixel `o` of `out`, in source pixels `of` wide: the middle of the
    // t-th of n equal slices of the area the pixel covers.
    let tap = |o: usize, t: usize, n: usize, of: usize, out: usize| (2 * o * n + 2 * t + 1) * of / (2 * out * n);
    columns.clear();
    columns.extend((0..ow).flat_map(|x| (0..nx).map(move |t| tap(x, t, nx, sw, ow) * 4)));
    line.resize(sw * 4, 0);
    // The mean as a multiply and a shift: `(sum + half) * recip >> 16` is exact for the sums here
    // (at most 16 samples of 255) at every tap count, so a flat colour stays the same colour.
    let count = (nx * ny) as u32;
    let (half, recip) = (count / 2, (65_536 + count / 2) / count);
    for (y, out_row) in dst.chunks_exact_mut(ow * 4).enumerate().take(oh) {
        for t in 0..ny {
            let row = &src.row(tap(y, t, ny, sh, oh))[..sw * 4];
            if t == 0 {
                line.iter_mut().zip(row).for_each(|(l, &p)| *l = u16::from(p));
            } else {
                line.iter_mut().zip(row).for_each(|(l, &p)| *l += u16::from(p));
            }
        }
        for (px, cols) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(columns.chunks_exact(nx)) {
            let (mut a, mut b, mut c) = (half, half, half);
            for &i in cols {
                a += u32::from(line[i]);
                b += u32::from(line[i + 1]);
                c += u32::from(line[i + 2]);
            }
            *px = [((a * recip) >> 16) as u8, ((b * recip) >> 16) as u8, ((c * recip) >> 16) as u8, 255];
        }
    }
}

/// BT.709 limited-range luma of one pixel.
#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8
}

/// BT.709 limited-range (U, V) of the sum of four pixels.
#[inline(always)]
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    // The coefficients are per pixel at 1/1024, and the sum is of four: shift by 12.
    let u = ((-103 * r - 347 * g + 450 * b + 2048) >> 12) + 128;
    let v = ((450 * r - 408 * g - 42 * b + 2048) >> 12) + 128;
    (u.clamp(16, 240) as u8, v.clamp(16, 240) as u8)
}

/// Fills `out` (already sized) from the top left of `src`.
fn to_planes(src: &Raster<'_>, out: &mut I420) {
    match src.order {
        PixelOrder::Bgra => rows::<true>(src, out),
        PixelOrder::Rgba => rows::<false>(src, out),
    }
}

/// Two source rows at a time: four lumas and one chroma pair per 2x2 block.
fn rows<const BGRA: bool>(src: &Raster<'_>, out: &mut I420) {
    let (w, h) = (out.width, out.height);
    let pick = |p: &[u8; 4]| {
        if BGRA { (i32::from(p[2]), i32::from(p[1]), i32::from(p[0])) } else { (i32::from(p[0]), i32::from(p[1]), i32::from(p[2])) }
    };
    let (cw, ch) = (w / 2, h / 2);
    let luma_rows = out.y.chunks_exact_mut(2 * w);
    let u_rows = out.u.chunks_exact_mut(cw);
    let v_rows = out.v.chunks_exact_mut(cw);
    for (cy, ((luma_pair, u_row), v_row)) in luma_rows.zip(u_rows).zip(v_rows).enumerate().take(ch) {
        let (top, bottom) = luma_pair.split_at_mut(w);
        let (r0, r1) = (&src.row(2 * cy)[..w * 4], &src.row(2 * cy + 1)[..w * 4]);
        let blocks = r0.as_chunks::<8>().0.iter().zip(r1.as_chunks::<8>().0);
        let lumas = top.as_chunks_mut::<2>().0.iter_mut().zip(bottom.as_chunks_mut::<2>().0.iter_mut());
        for (((p, q), (l0, l1)), (u, v)) in blocks.zip(lumas).zip(u_row.iter_mut().zip(v_row.iter_mut())) {
            let px = [
                pick(&[p[0], p[1], p[2], p[3]]),
                pick(&[p[4], p[5], p[6], p[7]]),
                pick(&[q[0], q[1], q[2], q[3]]),
                pick(&[q[4], q[5], q[6], q[7]]),
            ];
            l0[0] = luma(px[0].0, px[0].1, px[0].2);
            l0[1] = luma(px[1].0, px[1].1, px[1].2);
            l1[0] = luma(px[2].0, px[2].1, px[2].2);
            l1[1] = luma(px[3].0, px[3].1, px[3].2);
            let (r, g, b) = px.iter().fold((0, 0, 0), |a, c| (a.0 + c.0, a.1 + c.1, a.2 + c.2));
            (*u, *v) = chroma(r, g, b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, [r, g, b]: [u8; 3], order: PixelOrder) -> Vec<u8> {
        let px = match order {
            PixelOrder::Bgra => [b, g, r, 255],
            PixelOrder::Rgba => [r, g, b, 255],
        };
        px.repeat(width * height)
    }

    fn raster(data: &[u8], width: u32, height: u32, order: PixelOrder) -> Raster<'_> {
        Raster { data, width, height, stride: width as usize * 4, order }
    }

    #[test]
    fn primaries_in_bt709_limited_range() {
        // (rgb, y, u, v): the standard's values, within one code of rounding.
        for (rgb, y, u, v) in [
            ([0, 0, 0], 16, 128, 128),
            ([255, 255, 255], 235, 128, 128),
            ([255, 0, 0], 63, 102, 240),
            ([0, 255, 0], 173, 42, 26),
            ([0, 0, 255], 32, 240, 118),
        ] {
            for order in [PixelOrder::Bgra, PixelOrder::Rgba] {
                let data = solid(4, 2, rgb, order);
                let mut c = Converter::new();
                let planes = c.convert(&raster(&data, 4, 2, order), (4, 2)).expect("convert");
                let near = |got: u8, want: u8| got.abs_diff(want) <= 1;
                assert!(planes.y().iter().all(|&g| near(g, y)), "{rgb:?} {order:?} y {:?}", planes.y());
                assert!(planes.u().iter().all(|&g| near(g, u)), "{rgb:?} {order:?} u {:?}", planes.u());
                assert!(planes.v().iter().all(|&g| near(g, v)), "{rgb:?} {order:?} v {:?}", planes.v());
                assert_eq!((planes.y().len(), planes.u().len(), planes.v().len()), (8, 2, 2));
            }
        }
    }

    #[test]
    fn chroma_averages_each_block_and_the_plane_layout_is_row_major() {
        // 4x2: the left block red, the right blue.
        let mut data = Vec::new();
        for _ in 0..2 {
            data.extend(solid(2, 1, [255, 0, 0], PixelOrder::Bgra));
            data.extend(solid(2, 1, [0, 0, 255], PixelOrder::Bgra));
        }
        let mut c = Converter::new();
        let p = c.convert(&raster(&data, 4, 2, PixelOrder::Bgra), (4, 2)).expect("convert");
        assert_eq!(p.y(), [63, 63, 32, 32, 63, 63, 32, 32]);
        assert_eq!(p.u(), [102, 240]);
        assert_eq!(p.v(), [240, 118]);
        // A mixed block is the mean: black and white make mid grey chroma.
        let row = [solid(1, 1, [0, 0, 0], PixelOrder::Bgra), solid(1, 1, [255, 255, 255], PixelOrder::Bgra)].concat();
        let mixed = [row.clone(), row].concat();
        let p = c.convert(&raster(&mixed, 2, 2, PixelOrder::Bgra), (2, 2)).expect("convert");
        assert_eq!((p.u(), p.v()), (&[128][..], &[128][..]));
    }

    #[test]
    fn an_odd_frame_is_cropped_to_the_even_output() {
        // 5x3 with the last column and row distinct: the crop to 4x2 never reads them.
        let mut data = solid(5, 3, [255, 255, 255], PixelOrder::Rgba);
        for y in 0..3 {
            data[(y * 5 + 4) * 4..][..4].copy_from_slice(&[255, 0, 0, 255]);
        }
        data[2 * 5 * 4..].copy_from_slice(&[255, 0, 0, 255].repeat(5));
        let mut c = Converter::new();
        let p = c.convert(&raster(&data, 5, 3, PixelOrder::Rgba), (4, 2)).expect("convert");
        assert_eq!(p.size(), (4, 2));
        assert!(p.y().iter().all(|&y| y == 235) && p.u() == [128, 128]);
    }

    #[test]
    fn rows_may_be_padded() {
        // A 2x2 frame with 12-byte rows: the padding holds junk that must not be read as pixels.
        let mut data = Vec::new();
        for _ in 0..2 {
            data.extend(solid(2, 1, [255, 0, 0], PixelOrder::Rgba));
            data.extend([9; 4]);
        }
        let f = Raster { data: &data, width: 2, height: 2, stride: 12, order: PixelOrder::Rgba };
        let mut c = Converter::new();
        let p = c.convert(&f, (2, 2)).expect("convert");
        assert_eq!((p.y(), p.u(), p.v()), (&[63; 4][..], &[102][..], &[240][..]));
    }

    #[test]
    fn a_downscale_averages_what_it_covers() {
        // 8x4, left half white and right half black, to 4x2: a 2x2 box each; the edge between
        // them falls on a box boundary, so every output pixel is pure.
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend(solid(4, 1, [255, 255, 255], PixelOrder::Bgra));
            data.extend(solid(4, 1, [0, 0, 0], PixelOrder::Bgra));
        }
        let mut c = Converter::new();
        let p = c.convert(&raster(&data, 8, 4, PixelOrder::Bgra), (4, 2)).expect("convert");
        assert_eq!(p.y(), [235, 235, 16, 16, 235, 235, 16, 16]);
        // A 6x2 frame to 4x2 straddles source pixels: the mix is between the two extremes.
        let mut mix = Vec::new();
        for _ in 0..2 {
            mix.extend(solid(3, 1, [255, 255, 255], PixelOrder::Bgra));
            mix.extend(solid(3, 1, [0, 0, 0], PixelOrder::Bgra));
        }
        let p = c.convert(&raster(&mix, 6, 2, PixelOrder::Bgra), (4, 2)).expect("convert");
        assert_eq!(p.y()[0], 235);
        assert_eq!(p.y()[3], 16);
        assert!(p.y()[1..3].iter().all(|&y| (16..=235).contains(&y)));
    }

    #[test]
    fn the_output_size_follows_the_encoders_rule_and_buffers_are_reused() {
        let data = solid(1080, 2400, [10, 200, 30], PixelOrder::Rgba);
        let out = crate::output_size((1080, 2400), Some(1280));
        assert_eq!(out, (576, 1280));
        let mut c = Converter::new();
        let f = raster(&data, 1080, 2400, PixelOrder::Rgba);
        let first = c.convert(&f, out).expect("convert").y().as_ptr();
        let (size, again) = {
            let p = c.convert(&f, out).expect("convert");
            (p.size(), p.y().as_ptr())
        };
        assert_eq!(size, out);
        assert_eq!(first, again, "the second frame lands in the same buffers");
        let p = c.convert(&f, out).expect("convert");
        assert!(p.y().iter().all(|&y| y == p.y()[0]), "a flat colour stays flat through the scaler");
    }

    #[test]
    fn flat_colours_survive_every_tap_count() {
        // Source sides over the output's by factors that need 1 to 4 taps per axis (and the
        // odd ones in between): black, white and a mid colour come out unchanged.
        let mut c = Converter::new();
        for (sw, sh, ow, oh) in [(40u32, 40u32, 16u32, 16u32), (48, 30, 16, 16), (64, 64, 16, 16), (100, 20, 16, 16), (90, 90, 30, 18)] {
            for rgb in [[0, 0, 0], [255, 255, 255], [10, 200, 130]] {
                let data = solid(sw as usize, sh as usize, rgb, PixelOrder::Rgba);
                let flat = c.convert(&raster(&data, sw, sh, PixelOrder::Rgba), (ow, oh)).expect("convert").y().to_vec();
                let one = c.convert(&raster(&solid(2, 2, rgb, PixelOrder::Rgba), 2, 2, PixelOrder::Rgba), (2, 2)).expect("convert").y()[0];
                assert!(flat.iter().all(|&y| y == one), "{sw}x{sh} to {ow}x{oh} {rgb:?}: {flat:?} vs {one}");
            }
        }
    }

    #[test]
    fn bad_frames_and_sizes_are_refused() {
        let data = solid(4, 4, [0, 0, 0], PixelOrder::Bgra);
        let mut c = Converter::new();
        assert!(c.convert(&raster(&data[..10], 4, 4, PixelOrder::Bgra), (4, 4)).is_err(), "short data");
        assert!(c.convert(&Raster { stride: 8, ..raster(&data, 4, 4, PixelOrder::Bgra) }, (4, 4)).is_err(), "stride under a row");
        assert!(c.convert(&raster(&data, 4, 4, PixelOrder::Bgra), (3, 4)).is_err(), "odd output");
        assert!(c.convert(&raster(&data, 4, 4, PixelOrder::Bgra), (8, 8)).is_err(), "scaling up");
        assert!(c.convert(&raster(&data, 0, 4, PixelOrder::Bgra), (4, 4)).is_err(), "no pixels");
    }
}
