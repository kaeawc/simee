//! Camera pixel formats to BGRA: packed YUYV (4:2:2, BT.601 limited range, what UVC webcams send
//! uncompressed) and Motion JPEG (each frame a JPEG, often without its Huffman tables).

use crate::error::{Error, Result};

/// `V4L2_PIX_FMT_YUYV`.
pub(crate) const YUYV: u32 = u32::from_le_bytes(*b"YUYV");
/// `V4L2_PIX_FMT_MJPEG`.
pub(crate) const MJPG: u32 = u32::from_le_bytes(*b"MJPG");
/// `V4L2_PIX_FMT_JPEG`: the same frames under another name (some older drivers).
pub(crate) const JPEG: u32 = u32::from_le_bytes(*b"JPEG");

/// The largest frame side a camera frame may decode to (the hub's own limit).
const MAX_SIDE: u32 = crate::frame::FrameSpec::MAX_SIDE;

/// A V4L2 fourcc (little-endian) as its four characters.
pub(crate) fn fourcc_text(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect()
}

/// A pixel format this crate converts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pixel {
    Yuyv,
    Mjpeg,
}

impl Pixel {
    pub(crate) fn from_fourcc(code: u32) -> Option<Pixel> {
        match code {
            YUYV => Some(Pixel::Yuyv),
            MJPG | JPEG => Some(Pixel::Mjpeg),
            _ => None,
        }
    }

    /// The preference among formats of equal size and rate: YUYV converts with arithmetic, MJPEG
    /// needs a decode.
    pub(crate) fn rank(self) -> u32 {
        match self {
            Pixel::Yuyv => 0,
            Pixel::Mjpeg => 1,
        }
    }
}

/// One frame from the camera as it arrived: its pixel format, the negotiated size and row stride
/// (for YUYV), and the bytes the driver filled.
pub(crate) fn to_bgra(pixel: Pixel, width: u32, height: u32, stride: usize, data: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    match pixel {
        Pixel::Yuyv => yuyv_to_bgra(data, width, height, stride).map(|px| (width, height, px)).ok_or_else(|| {
            Error::Capture(format!(
                "a {width}x{height} YUYV frame needs {} bytes, the camera filled {}",
                stride * height as usize,
                data.len()
            ))
        }),
        Pixel::Mjpeg => mjpeg_to_bgra(data),
    }
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Packed YUYV (Y0 U Y1 V for each pair of pixels) to opaque BGRA, BT.601 limited range in
/// integer arithmetic. `None` when the buffer is shorter than `height` rows of `stride` bytes, the
/// stride is shorter than a row, or the width is odd.
pub(crate) fn yuyv_to_bgra(src: &[u8], width: u32, height: u32, stride: usize) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || !w.is_multiple_of(2) || stride < w * 2 || src.len() < stride * (h - 1) + w * 2 {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
        let line = &src[y * stride..][..w * 2];
        for (quad, px) in line.as_chunks::<4>().0.iter().zip(row.as_chunks_mut::<8>().0) {
            let (d, e) = (i32::from(quad[1]) - 128, i32::from(quad[3]) - 128);
            for (i, luma) in [quad[0], quad[2]].into_iter().enumerate() {
                let c = 298 * (i32::from(luma) - 16);
                px[i * 4] = clamp((c + 516 * d + 128) >> 8);
                px[i * 4 + 1] = clamp((c - 100 * d - 208 * e + 128) >> 8);
                px[i * 4 + 2] = clamp((c + 409 * e + 128) >> 8);
                px[i * 4 + 3] = 0xff;
            }
        }
    }
    Some(out)
}

/// The segments of a JPEG before its scan: whether it carries Huffman tables (`DHT`). UVC cameras
/// usually leave them out and rely on the JPEG standard's (Annex K).
fn has_huffman_tables(bytes: &[u8]) -> bool {
    let mut i = 2;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xff {
            return false;
        }
        match bytes[i + 1] {
            0xc4 => return true,
            0xda | 0xd9 => return false,
            0xff => i += 1,
            0x01 | 0xd0..=0xd7 => i += 2,
            _ => i += 2 + usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]])),
        }
    }
    false
}

/// The `APP0` segment that marks a frame as AVI-style Motion JPEG, which makes the decoder supply
/// the standard Huffman tables a frame leaves out.
const AVI1: [u8; 9] = [0xff, 0xe0, 0x00, 0x07, b'A', b'V', b'I', b'1', 0x00];

/// One Motion JPEG frame to opaque BGRA, at the size the frame says.
pub(crate) fn mjpeg_to_bgra(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let bad = |why: String| Error::Capture(format!("a Motion JPEG frame doesn't decode: {why}"));
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err(bad("no JPEG start marker".into()));
    }
    let patched;
    let input = if has_huffman_tables(bytes) {
        bytes
    } else {
        patched = [&bytes[..2], &AVI1, &bytes[2..]].concat();
        &patched
    };
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(MAX_SIDE as usize)
        .set_max_height(MAX_SIDE as usize);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(input), options);
    let rgb = decoder.decode().map_err(|e| bad(e.to_string()))?;
    let info = decoder.info().ok_or_else(|| bad("no frame header".into()))?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if w == 0 || h == 0 || rgb.len() != w as usize * h as usize * 3 {
        return Err(bad(format!("{} bytes for {w}x{h} RGB", rgb.len())));
    }
    Ok((w, h, rgb.as_chunks::<3>().0.iter().flat_map(|p| [p[2], p[1], p[0], 0xff]).collect()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture_jpeg() -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/red-16x16.jpg");
        std::fs::read(path).unwrap_or_default()
    }

    /// The fixture with its `DHT` segments cut out, as a UVC camera sends a frame.
    pub(crate) fn without_huffman_tables(jpeg: &[u8]) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        let mut i = 2;
        while i + 4 <= jpeg.len() {
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if jpeg[i + 1] == 0xda {
                out.extend_from_slice(&jpeg[i..]);
                break;
            }
            if jpeg[i + 1] != 0xc4 {
                out.extend_from_slice(&jpeg[i..i + 2 + len]);
            }
            i += 2 + len;
        }
        out
    }

    /// One YUYV pair: both pixels the same colour.
    fn pair(y: u8, u: u8, v: u8) -> [u8; 4] {
        [y, u, y, v]
    }

    #[test]
    fn yuyv_converts_with_bt601_limited_range() {
        let cases = [
            (pair(16, 128, 128), [0, 0, 0, 255]),        // black
            (pair(235, 128, 128), [255, 255, 255, 255]), // white
            (pair(126, 128, 128), [128, 128, 128, 255]), // mid grey
            (pair(81, 90, 240), [0, 0, 255, 255]),       // red
            (pair(145, 54, 34), [0, 255, 0, 255]),       // green
            (pair(41, 240, 110), [255, 0, 0, 255]),      // blue
        ];
        for (yuyv, bgra) in cases {
            let px = yuyv_to_bgra(&yuyv, 2, 1, 4).unwrap_or_default();
            for (i, got) in px.as_chunks::<4>().0.iter().enumerate() {
                let close = got.iter().zip(bgra).all(|(&g, w)| g.abs_diff(w) <= 2);
                assert!(close, "{yuyv:?} pixel {i}: {got:?}, want {bgra:?}");
            }
        }
        // Out-of-range luma clamps instead of wrapping.
        assert_eq!(yuyv_to_bgra(&pair(255, 128, 128), 2, 1, 4).unwrap_or_default()[..4], [255, 255, 255, 255]);
        assert_eq!(yuyv_to_bgra(&pair(0, 128, 128), 2, 1, 4).unwrap_or_default()[..4], [0, 0, 0, 255]);
    }

    #[test]
    fn yuyv_pairs_keep_their_own_luma_and_rows_honour_the_stride() {
        // 2x2 with 4 bytes of padding per row: black|white on the first row, white|black below.
        let mut src = vec![16, 128, 235, 128, 0xee, 0xee, 0xee, 0xee];
        src.extend_from_slice(&[235, 128, 16, 128]); // the last row may omit its padding
        let px = yuyv_to_bgra(&src, 2, 2, 8).unwrap_or_default();
        assert_eq!(px.len(), 16);
        assert_eq!((px[0], px[4], px[8], px[12]), (0, 255, 255, 0));
    }

    #[test]
    fn short_or_odd_yuyv_buffers_are_refused() {
        assert_eq!(yuyv_to_bgra(&[0; 7], 2, 2, 4), None, "short");
        assert_eq!(yuyv_to_bgra(&[0; 12], 3, 1, 6), None, "odd width");
        assert_eq!(yuyv_to_bgra(&[0; 8], 2, 2, 2), None, "stride under a row");
        assert_eq!(yuyv_to_bgra(&[], 0, 0, 0), None);
        let err = to_bgra(Pixel::Yuyv, 640, 480, 1280, &[0; 100]).err();
        assert!(matches!(&err, Some(Error::Capture(m)) if m.contains("614400")), "{err:?}");
    }

    #[test]
    fn mjpeg_frames_decode_with_or_without_huffman_tables() {
        let jpeg = fixture_jpeg();
        let stripped = without_huffman_tables(&jpeg);
        assert!(has_huffman_tables(&jpeg));
        assert!(!has_huffman_tables(&stripped) && stripped.len() < jpeg.len());
        let full = mjpeg_to_bgra(&jpeg).unwrap_or_else(|e| panic!("{e}"));
        let bare = mjpeg_to_bgra(&stripped).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((full.0, full.1), (16, 16));
        assert_eq!(full, bare, "the standard tables are the ones the fixture carried");
        let [b, g, r, a] = [full.2[0], full.2[1], full.2[2], full.2[3]];
        assert!(r > 230 && g < 30 && b < 30 && a == 255, "red: {r},{g},{b},{a}");
        assert_eq!(to_bgra(Pixel::Mjpeg, 1, 1, 0, &jpeg).map(|f| (f.0, f.1)), Ok((16, 16)), "the frame's own size");
    }

    #[test]
    fn broken_mjpeg_frames_are_errors() {
        assert!(matches!(mjpeg_to_bgra(b"not a jpeg"), Err(Error::Capture(_))));
        let jpeg = fixture_jpeg();
        assert!(mjpeg_to_bgra(&jpeg[..jpeg.len() / 3]).is_err(), "truncated");
        assert!(mjpeg_to_bgra(&[]).is_err());
    }

    #[test]
    fn fourccs_map_to_the_formats_converted() {
        assert_eq!(Pixel::from_fourcc(YUYV), Some(Pixel::Yuyv));
        assert_eq!(Pixel::from_fourcc(MJPG), Some(Pixel::Mjpeg));
        assert_eq!(Pixel::from_fourcc(JPEG), Some(Pixel::Mjpeg));
        assert_eq!(Pixel::from_fourcc(u32::from_le_bytes(*b"NV12")), None);
        assert!(Pixel::Yuyv.rank() < Pixel::Mjpeg.rank());
    }
}
