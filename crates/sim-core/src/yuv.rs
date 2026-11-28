//! BGRA camera frames to the layouts a camera HAL asks for, named by V4L2 fourcc as the goldfish
//! camera protocol names them: NV21, NV12, YV12, YU12 (I420) and RGB32.
//!
//! YUV is BT.601 limited range (Y 16..=235, chroma 16..=240), as the stock emulator's converters
//! produce it. Chroma is the average of each 2x2 block (odd edges repeat the last row or column).
//! Planes are tightly packed: chroma rows are `(width + 1) / 2` samples.

/// A frame layout from the guest's `pix=` parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraFormat {
    /// Y plane, then interleaved V,U (`V4L2_PIX_FMT_NV21`, Android's default camera format).
    Nv21,
    /// Y plane, then interleaved U,V (`V4L2_PIX_FMT_NV12`).
    Nv12,
    /// Y plane, V plane, U plane (`V4L2_PIX_FMT_YVU420`).
    Yv12,
    /// Y plane, U plane, V plane (`V4L2_PIX_FMT_YUV420`, I420).
    Yu12,
    /// `V4L2_PIX_FMT_RGB32` as the goldfish HAL reads it: R, G, B, A bytes (its RGBA_8888 buffers).
    Rgb32,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

impl CameraFormat {
    pub const ALL: [CameraFormat; 5] = [Self::Nv21, Self::Nv12, Self::Yv12, Self::Yu12, Self::Rgb32];

    /// The V4L2 fourcc, as the protocol sends it (a decimal `u32`).
    pub const fn fourcc(self) -> u32 {
        match self {
            Self::Nv21 => fourcc(b"NV21"),
            Self::Nv12 => fourcc(b"NV12"),
            Self::Yv12 => fourcc(b"YV12"),
            Self::Yu12 => fourcc(b"YU12"),
            Self::Rgb32 => fourcc(b"RGB4"),
        }
    }

    pub fn from_fourcc(code: u32) -> Option<CameraFormat> {
        Self::ALL.into_iter().find(|f| f.fourcc() == code)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Nv21 => "NV21",
            Self::Nv12 => "NV12",
            Self::Yv12 => "YV12",
            Self::Yu12 => "YU12",
            Self::Rgb32 => "RGB32",
        }
    }

    /// Bytes in one `width` x `height` frame.
    pub const fn frame_size(self, width: usize, height: usize) -> usize {
        match self {
            Self::Rgb32 => width * height * 4,
            _ => width * height + 2 * width.div_ceil(2) * height.div_ceil(2),
        }
    }
}

/// BT.601 limited-range luma of one pixel.
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

/// BT.601 limited-range (U, V) of one pixel (or of a block's mean).
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Converts tightly packed BGRA (`width * 4` bytes per row) into `out`, which must be exactly
/// [`CameraFormat::frame_size`] bytes. False (and `out` untouched) when a size doesn't match.
pub fn from_bgra(bgra: &[u8], width: usize, height: usize, format: CameraFormat, out: &mut [u8]) -> bool {
    if width == 0 || height == 0 || bgra.len() != width * height * 4 || out.len() != format.frame_size(width, height) {
        return false;
    }
    let (pixels, _) = bgra.as_chunks::<4>();
    if format == CameraFormat::Rgb32 {
        for (d, &[b, g, r, _]) in out.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
            *d = [r, g, b, 255];
        }
        return true;
    }
    let (y_plane, uv) = out.split_at_mut(width * height);
    for (d, &[b, g, r, _]) in y_plane.iter_mut().zip(pixels) {
        *d = luma(i32::from(r), i32::from(g), i32::from(b));
    }
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let px = |x: usize, y: usize| {
        let i = (y.min(height - 1) * width + x.min(width - 1)) * 4;
        (i32::from(bgra[i + 2]), i32::from(bgra[i + 1]), i32::from(bgra[i]))
    };
    let plane = cw * ch;
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut r, mut g, mut b) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (pr, pg, pb) = px(2 * cx + dx, 2 * cy + dy);
                (r, g, b) = (r + pr, g + pg, b + pb);
            }
            let (u, v) = chroma((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
            let i = cy * cw + cx;
            match format {
                CameraFormat::Nv21 => (uv[2 * i], uv[2 * i + 1]) = (v, u),
                CameraFormat::Nv12 => (uv[2 * i], uv[2 * i + 1]) = (u, v),
                CameraFormat::Yv12 => (uv[i], uv[plane + i]) = (v, u),
                CameraFormat::Yu12 => (uv[i], uv[plane + i]) = (u, v),
                CameraFormat::Rgb32 => unreachable!("handled above"),
            }
        }
    }
    true
}

/// A converted copy: [`from_bgra`] into a new buffer, or `None` when the sizes don't match.
pub fn convert(bgra: &[u8], width: usize, height: usize, format: CameraFormat) -> Option<Vec<u8>> {
    let mut out = vec![0; format.frame_size(width, height)];
    from_bgra(bgra, width, height, format, &mut out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, [r, g, b]: [u8; 3]) -> Vec<u8> {
        [b, g, r, 255].repeat(width * height)
    }

    #[test]
    fn fourccs_match_v4l2() {
        // The decimal values a guest sends in `pix=`.
        assert_eq!(CameraFormat::Nv21.fourcc(), 825_382_478);
        assert_eq!(CameraFormat::Yv12.fourcc(), 842_094_169);
        assert_eq!(CameraFormat::Yu12.fourcc(), 842_093_913);
        assert_eq!(CameraFormat::Nv12.fourcc(), 842_094_158);
        assert_eq!(CameraFormat::Rgb32.fourcc(), 876_758_866);
        for f in CameraFormat::ALL {
            assert_eq!(CameraFormat::from_fourcc(f.fourcc()), Some(f));
        }
        assert_eq!(CameraFormat::from_fourcc(0x5659_5559), None, "YUYV isn't offered");
    }

    #[test]
    fn frame_sizes() {
        assert_eq!(CameraFormat::Nv21.frame_size(640, 480), 640 * 480 * 3 / 2);
        assert_eq!(CameraFormat::Yv12.frame_size(3, 3), 9 + 2 * 4);
        assert_eq!(CameraFormat::Rgb32.frame_size(640, 480), 640 * 480 * 4);
    }

    #[test]
    fn primaries_in_bt601_limited_range() {
        // (rgb, y, u, v) as BT.601 limited range gives them.
        for (rgb, y, u, v) in [
            ([0, 0, 0], 16, 128, 128),
            ([255, 255, 255], 235, 128, 128),
            ([255, 0, 0], 82, 90, 240),
            ([0, 255, 0], 144, 54, 34),
            ([0, 0, 255], 41, 240, 110),
        ] {
            let nv21 = convert(&solid(2, 2, rgb), 2, 2, CameraFormat::Nv21).expect("nv21");
            assert_eq!(nv21, [y, y, y, y, v, u], "{rgb:?}");
            let nv12 = convert(&solid(2, 2, rgb), 2, 2, CameraFormat::Nv12).expect("nv12");
            assert_eq!(nv12, [y, y, y, y, u, v], "{rgb:?}");
            let yv12 = convert(&solid(2, 2, rgb), 2, 2, CameraFormat::Yv12).expect("yv12");
            assert_eq!(yv12, [y, y, y, y, v, u], "{rgb:?}");
            let i420 = convert(&solid(2, 2, rgb), 2, 2, CameraFormat::Yu12).expect("yu12");
            assert_eq!(i420, [y, y, y, y, u, v], "{rgb:?}");
        }
    }

    #[test]
    fn planes_are_laid_out_by_format() {
        // 4x2: left half red, right half blue. One chroma row of two samples.
        let mut bgra = Vec::new();
        for _ in 0..2 {
            bgra.extend(solid(2, 1, [255, 0, 0]));
            bgra.extend(solid(2, 1, [0, 0, 255]));
        }
        let (red, blue) = ((90, 240), (240, 110));
        let nv21 = convert(&bgra, 4, 2, CameraFormat::Nv21).expect("nv21");
        assert_eq!(nv21[..8], [82, 82, 41, 41, 82, 82, 41, 41]);
        assert_eq!(nv21[8..], [red.1, red.0, blue.1, blue.0], "V,U pairs");
        let yv12 = convert(&bgra, 4, 2, CameraFormat::Yv12).expect("yv12");
        assert_eq!(yv12[8..], [red.1, blue.1, red.0, blue.0], "V plane, then U");
        let i420 = convert(&bgra, 4, 2, CameraFormat::Yu12).expect("yu12");
        assert_eq!(i420[8..], [red.0, blue.0, red.1, blue.1], "U plane, then V");
    }

    #[test]
    fn chroma_averages_each_block_and_odd_edges() {
        // 3x1: black, white, red. Block 0 averages black and white (grey); block 1 is red alone.
        let bgra = [solid(1, 1, [0, 0, 0]), solid(1, 1, [255, 255, 255]), solid(1, 1, [255, 0, 0])].concat();
        let yu12 = convert(&bgra, 3, 1, CameraFormat::Yu12).expect("yu12");
        assert_eq!(yu12.len(), 3 + 2 + 2);
        assert_eq!(yu12[..3], [16, 235, 82]);
        assert_eq!(yu12[3..], [128, 90, 128, 240]);
    }

    #[test]
    fn rgb32_is_rgba_bytes() {
        let out = convert(&[10, 20, 30, 0], 1, 1, CameraFormat::Rgb32).expect("rgb32");
        assert_eq!(out, [30, 20, 10, 255], "opaque, red first");
    }

    #[test]
    fn wrong_sizes_are_refused() {
        assert_eq!(convert(&[0; 15], 2, 2, CameraFormat::Nv21), None);
        assert_eq!(convert(&[], 0, 0, CameraFormat::Nv21), None);
        let mut out = [7u8; 5];
        assert!(!from_bgra(&[0; 16], 2, 2, CameraFormat::Nv21, &mut out));
        assert_eq!(out, [7; 5], "untouched");
    }
}
