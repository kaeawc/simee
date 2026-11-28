//! Framebuffer pixel conversion, shared by the iOS framebuffer (BGRA `IOSurface`) and the Android
//! scanout (virtio-gpu formats).

/// The byte order of a 32-bit pixel in memory. `X` bytes are padding: the output is opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Bgrx,
    Argb,
    Xrgb,
    Rgba,
    Rgbx,
    Abgr,
    Xbgr,
}

impl PixelFormat {
    /// `VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM`: RGBA bytes in memory.
    pub const RGBA_VIRTIO: u32 = 67;

    /// A virtio-gpu format code (`VIRTIO_GPU_FORMAT_*`, whose names list bytes in memory order).
    pub fn from_virtio_gpu(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Bgra,
            2 => Self::Bgrx,
            3 => Self::Argb,
            4 => Self::Xrgb,
            67 => Self::Rgba,
            68 => Self::Xbgr,
            121 => Self::Abgr,
            134 => Self::Rgbx,
            _ => return None,
        })
    }
}

/// Converts a framebuffer with `stride` bytes per row into tightly packed RGBA. Returns `None`
/// when `src` is too short for the described geometry.
pub fn to_rgba(src: &[u8], width: usize, height: usize, stride: usize, format: PixelFormat) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    to_rgba_into(src, width, height, stride, format, &mut out).then_some(out)
}

/// [`to_rgba`] into `out`, reusing its allocation (a capture kept across frames). Returns false,
/// leaving `out` unchanged, when `src` is too short for the described geometry.
pub fn to_rgba_into(src: &[u8], width: usize, height: usize, stride: usize, format: PixelFormat, out: &mut Vec<u8>) -> bool {
    let Some(row_bytes) = width.checked_mul(4) else { return false };
    let fits = || Some(height == 0 || src.len() >= stride.checked_mul(height - 1)?.checked_add(row_bytes)?);
    if stride < row_bytes || fits() != Some(true) {
        return false;
    }
    // Appended to a cleared Vec, so no byte is written twice (a zero fill first costs a tenth of a
    // 1206x2622 capture).
    out.clear();
    out.reserve(row_bytes * height);
    // One loop per format, so the byte shuffle is a fixed permutation the compiler vectorizes.
    let convert: fn(&[u8], &mut Vec<u8>) = match format {
        PixelFormat::Rgba => |src, dst| dst.extend_from_slice(src),
        PixelFormat::Bgra => |src, dst| shuffle(src, dst, |p| [p[2], p[1], p[0], p[3]]),
        PixelFormat::Bgrx => |src, dst| shuffle(src, dst, |p| [p[2], p[1], p[0], 0xff]),
        PixelFormat::Argb => |src, dst| shuffle(src, dst, |p| [p[1], p[2], p[3], p[0]]),
        PixelFormat::Xrgb => |src, dst| shuffle(src, dst, |p| [p[1], p[2], p[3], 0xff]),
        PixelFormat::Rgbx => |src, dst| shuffle(src, dst, |p| [p[0], p[1], p[2], 0xff]),
        PixelFormat::Abgr => |src, dst| shuffle(src, dst, |p| [p[3], p[2], p[1], p[0]]),
        PixelFormat::Xbgr => |src, dst| shuffle(src, dst, |p| [p[3], p[2], p[1], 0xff]),
    };
    for row in 0..height {
        convert(&src[row * stride..row * stride + row_bytes], out);
    }
    true
}

#[inline(always)]
fn shuffle(src: &[u8], dst: &mut Vec<u8>, f: impl Fn(&[u8; 4]) -> [u8; 4]) {
    dst.extend(src.as_chunks::<4>().0.iter().flat_map(f));
}

/// Converts a BGRA framebuffer into tightly packed opaque RGBA (the iOS framebuffer's alpha byte
/// is not meaningful).
pub fn bgra_to_rgba(src: &[u8], width: usize, height: usize, stride: usize) -> Option<Vec<u8>> {
    to_rgba(src, width, height, stride, PixelFormat::Bgrx)
}

/// Copies `height` rows of `width` pixels from `src` (`stride` bytes per row) into `dst`
/// (`dst_stride` bytes per row), swapping the first and third byte of each: RGBA into BGRA (or
/// back) in the one pass that writes the destination, for an encoder's BGRA pixel buffer (CoreVideo
/// allocates no RGBA ones). Bytes past each row's pixels are left alone. False, with `dst`
/// untouched, when either side is too short.
pub fn swap_red_blue_into(src: &[u8], stride: usize, dst: &mut [u8], dst_stride: usize, width: usize, height: usize) -> bool {
    let Some(row) = width.checked_mul(4) else { return false };
    let fits = |len: usize, stride: usize| {
        height == 0 || stride.checked_mul(height - 1).and_then(|n| n.checked_add(row)).is_some_and(|n| len >= n)
    };
    if stride < row || dst_stride < row || !fits(src.len(), stride) || !fits(dst.len(), dst_stride) {
        return false;
    }
    for y in 0..height {
        let (s, d) = (&src[y * stride..y * stride + row], &mut dst[y * dst_stride..y * dst_stride + row]);
        for (d, s) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.as_chunks::<4>().0) {
            *d = [s[2], s[1], s[0], s[3]];
        }
    }
    true
}

/// Shrinks tightly packed RGBA by a whole `factor`, averaging each `factor` x `factor` block.
/// Factors of 0 and 1 return the image unchanged.
pub fn downscale(width: u32, height: u32, rgba: &[u8], factor: u32) -> (u32, u32, Vec<u8>) {
    let (w, h) = downscaled_size(width, height, factor);
    let mut out = vec![0; w as usize * h as usize * 4];
    downscale_into(width, height, rgba, factor, &mut out);
    (w, h, out)
}

/// The size [`downscale`] produces.
pub fn downscaled_size(width: u32, height: u32, factor: u32) -> (u32, u32) {
    let f = factor.max(1);
    (width / f, height / f)
}

/// [`downscale`] into `out`, which must hold at least the downscaled frame (see
/// [`downscaled_size`]): straight into a shared-memory slot, with no intermediate frame. Large
/// frames are split into bands of rows across a few threads. Returns the downscaled size.
pub fn downscale_into(width: u32, height: u32, rgba: &[u8], factor: u32, out: &mut [u8]) -> (u32, u32) {
    let (_, oh) = downscaled_size(width, height, factor);
    let read = oh as usize * factor.max(1) as usize * width as usize * 4;
    downscale_banded(width, height, rgba, factor, out, bands_for(read))
}

fn downscale_banded(width: u32, height: u32, rgba: &[u8], factor: u32, out: &mut [u8], bands: usize) -> (u32, u32) {
    let (ow, oh) = downscaled_size(width, height, factor);
    let f = factor.max(1) as usize;
    let (in_row, out_row) = (width as usize * 4, ow as usize * 4);
    let out = &mut out[..out_row * oh as usize];
    if out.is_empty() {
        return (ow, oh);
    }
    // Rows per band: whole output rows, so each band reads only its own input rows.
    let rows_per_band = (oh as usize).div_ceil(bands.max(1));
    let bands = (oh as usize).div_ceil(rows_per_band);
    let band = |(i, out): (usize, &mut [u8])| {
        let first = i * rows_per_band;
        let src = &rgba[first * f * in_row..];
        match f {
            1 => out.copy_from_slice(&src[..out.len()]),
            2 => box_rows::<2, 8>(src, in_row, out, out_row),
            3 => box_rows::<3, 12>(src, in_row, out, out_row),
            4 => box_rows::<4, 16>(src, in_row, out, out_row),
            f => box_rows_any(src, in_row, out, out_row, f),
        }
    };
    if bands == 1 {
        band((0, out));
    } else {
        std::thread::scope(|s| {
            let mut parts = out.chunks_mut(rows_per_band * out_row).enumerate();
            let first = parts.next();
            for part in parts {
                s.spawn(move || band(part));
            }
            if let Some(part) = first {
                band(part);
            }
        });
    }
    (ow, oh)
}

/// Input bytes below which one thread does the whole frame: starting a thread costs 20–100 µs,
/// and on a host with SMT two threads can share one core (measured slower than one there). A
/// tablet-sized frame (2480x1740, 17 MB) splits; a phone's (1080x2400, 10 MB) doesn't.
const BAND_MIN_BYTES: usize = 16 << 20;
/// Most threads one frame uses: the emulator's vCPUs and renderer need the rest of the machine.
const MAX_BANDS: usize = 4;

fn bands_for(in_bytes: usize) -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cores = *CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    if in_bytes < BAND_MIN_BYTES { 1 } else { cores.clamp(1, MAX_BANDS) }
}

/// The box filter for a factor known at compile time (the display stream's usual 2–4), one output
/// row at a time, in passes that each run straight along a row (so the compiler vectorizes them):
/// 1. the `F` input rows are summed column by column into 16-bit lanes;
/// 2. each lane gets the next `F - 1` same-channel lanes (one pixel apart) added and is divided,
///    which is the block average wherever a block starts;
/// 3. every `F`th pixel of that row is the output.
fn box_rows<const F: usize, const W: usize>(src: &[u8], in_row: usize, out: &mut [u8], out_row: usize) {
    debug_assert_eq!(W, 4 * F);
    let used = out_row * F;
    // Lanes where a whole block of F pixels starts at or after them.
    let starts = used - 4 * (F - 1);
    let mut acc = vec![0u16; used];
    let mut avg = vec![0u8; starts];
    let div = Reciprocal16::new((F * F) as u32);
    for (oy, out) in out.chunks_exact_mut(out_row).enumerate() {
        let rows: [&[u8]; F] = std::array::from_fn(|dy| &src[(oy * F + dy) * in_row..][..used]);
        let (blocks, tail) = acc.as_chunks_mut::<16>();
        for (i, a) in blocks.iter_mut().enumerate() {
            let mut s = [0u16; 16];
            for row in rows {
                for (k, v) in row[i * 16..][..16].iter().enumerate() {
                    s[k] += u16::from(*v);
                }
            }
            *a = s;
        }
        let done = blocks.len() * 16;
        for (k, a) in tail.iter_mut().enumerate() {
            *a = rows.iter().map(|r| u16::from(r[done + k])).sum();
        }
        let shifted: [&[u16]; F] = std::array::from_fn(|dx| &acc[dx * 4..][..starts]);
        for (i, a) in avg.iter_mut().enumerate() {
            let mut v = 0u16;
            for lanes in shifted {
                v += lanes[i];
            }
            *a = div.of(v);
        }
        for (o, block) in out.as_chunks_mut::<4>().0.iter_mut().zip(avg.chunks(W)) {
            o.copy_from_slice(&block[..4]);
        }
    }
}

fn box_rows_any(src: &[u8], in_row: usize, out: &mut [u8], out_row: usize, f: usize) {
    let div = Reciprocal::new((f * f) as u32);
    for (oy, out) in out.chunks_exact_mut(out_row).enumerate() {
        for (ox, o) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let mut acc = [0u32; 4];
            for dy in 0..f {
                for px in src[(oy * f + dy) * in_row + ox * f * 4..][..f * 4].as_chunks::<4>().0 {
                    for c in 0..4 {
                        acc[c] += u32::from(px[c]);
                    }
                }
            }
            *o = acc.map(|v| div.of(v));
        }
    }
}

/// Division by a fixed `2 <= n <= 16` in 16-bit arithmetic: `v * r >> 16` with
/// `r = ceil(2^16 / n)` (a 16-bit multiply-high, one instruction per vector) is exact (`v / n`,
/// rounded down) for box sums (`v <= 255 * n`) when `n * n < 2^16 / 255`, i.e. `n <= 16`:
/// factors 2 to 4.
#[derive(Clone, Copy)]
struct Reciprocal16(u16);

impl Reciprocal16 {
    fn new(n: u32) -> Reciprocal16 {
        debug_assert!((2..=16).contains(&n));
        Reciprocal16((1u32 << 16).div_ceil(n) as u16)
    }

    fn of(self, v: u16) -> u8 {
        ((u32::from(v) * u32::from(self.0)) >> 16) as u8
    }
}

/// Division by a fixed `n` as a multiply and shift: exact (`v / n`, rounded down) for the sums
/// a box filter produces (`v <= 255 * n`, `n < 2^16`).
#[derive(Clone, Copy)]
struct Reciprocal(u64);

impl Reciprocal {
    fn new(n: u32) -> Reciprocal {
        Reciprocal((1u64 << 32).div_ceil(u64::from(n)))
    }

    fn of(self, v: u32) -> u8 {
        ((u64::from(v) * self.0) >> 32) as u8
    }
}

/// Whether two readbacks of this RGBA frame agreeing proves they share a layout: some pixel's
/// first and third bytes differ (so a red/blue swap would show) and some row differs from its
/// mirror row (so a vertical flip would). A black boot screen proves nothing.
pub fn reveals_layout(rgba: &[u8], width: u32, height: u32) -> bool {
    let row = width as usize * 4;
    let Some(px) = rgba.get(..row * height as usize) else { return false };
    if row == 0 {
        return false;
    }
    let swaps = px.as_chunks::<4>().0.iter().any(|p| p[0] != p[2]);
    let rows: Vec<&[u8]> = px.chunks_exact(row).collect();
    let flips = rows.iter().zip(rows.iter().rev()).any(|(a, b)| a != b);
    swaps && flips
}

/// The smallest whole factor that brings `width` down to at most `max_width` (at least 1).
pub fn downscale_factor(width: u32, max_width: u32) -> u32 {
    width.div_ceil(max_width.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The box filter `downscale` replaced, kept as the reference.
    fn naive(width: u32, height: u32, rgba: &[u8], factor: u32) -> Vec<u8> {
        let (ow, oh) = (width / factor, height / factor);
        let mut out = Vec::new();
        for oy in 0..oh {
            for ox in 0..ow {
                let mut acc = [0u32; 4];
                for dy in 0..factor {
                    let row = ((oy * factor + dy) * width + ox * factor) as usize * 4;
                    for dx in 0..factor as usize {
                        for (c, a) in acc.iter_mut().enumerate() {
                            *a += u32::from(rgba[row + dx * 4 + c]);
                        }
                    }
                }
                out.extend(acc.map(|v| (v / (factor * factor)) as u8));
            }
        }
        out
    }

    #[test]
    fn swap_red_blue_into_copies_rows_between_strides() {
        // Two rows of two pixels, the source padded to 12 bytes a row, the destination to 10.
        let src = [1, 2, 3, 4, 5, 6, 7, 8, 0xee, 0xee, 0xee, 0xee, 9, 10, 11, 12, 13, 14, 15, 16];
        let mut dst = [0u8; 18];
        assert!(swap_red_blue_into(&src, 12, &mut dst, 10, 2, 2));
        assert_eq!(dst, [3, 2, 1, 4, 7, 6, 5, 8, 0, 0, 11, 10, 9, 12, 15, 14, 13, 16]);
        assert!(!swap_red_blue_into(&src, 12, &mut dst[..17], 10, 2, 2), "destination too short");
        assert!(!swap_red_blue_into(&src[..19], 12, &mut dst, 10, 2, 2), "source too short");
        assert!(!swap_red_blue_into(&src, 7, &mut dst, 10, 2, 2), "stride below a row");
        assert!(swap_red_blue_into(&[], 0, &mut [], 0, 0, 0));
    }

    #[test]
    fn downscale_matches_the_reference_box_filter_for_every_factor() {
        let (w, h) = (37u32, 29u32); // not multiples: the remainder rows and columns are dropped
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for factor in 2..=7 {
            let (ow, oh, out) = downscale(w, h, &rgba, factor);
            assert_eq!((ow, oh), (w / factor, h / factor));
            assert_eq!(out, naive(w, h, &rgba, factor), "factor {factor}");
        }
    }

    #[test]
    fn reciprocal_division_is_exact_for_box_sums() {
        for n in 1..=64u32 {
            let r = Reciprocal::new(n);
            for v in 0..=255 * n {
                assert_eq!(u32::from(r.of(v)), v / n, "{v} / {n}");
            }
        }
        for n in 2..=16u32 {
            let r = Reciprocal16::new(n);
            for v in 0..=255 * n {
                assert_eq!(u32::from(r.of(v as u16)), v / n, "16-bit: {v} / {n}");
            }
        }
    }

    #[test]
    fn banded_downscale_matches_the_reference_into_a_larger_buffer() {
        let (w, h) = (53u32, 47u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        for factor in 1..=5 {
            let (ow, oh) = downscaled_size(w, h, factor);
            let want = if factor == 1 { rgba.clone() } else { naive(w, h, &rgba, factor) };
            for bands in [1, 2, 3, 4, 7, 100] {
                // A slot larger than the frame: only the frame's bytes are written.
                let mut out = vec![0xAB; ow as usize * oh as usize * 4 + 9];
                assert_eq!(downscale_banded(w, h, &rgba, factor, &mut out, bands), (ow, oh));
                assert_eq!(&out[..want.len()], want.as_slice(), "factor {factor}, {bands} bands");
                assert!(out[want.len()..].iter().all(|&b| b == 0xAB), "nothing past the frame");
            }
        }
        // The public entry picks its own bands and agrees.
        let mut out = vec![0; (w / 3 * (h / 3) * 4) as usize];
        assert_eq!(downscale_into(w, h, &rgba, 3, &mut out), (w / 3, h / 3));
        assert_eq!(out, naive(w, h, &rgba, 3));
        assert_eq!(downscale_into(2, 2, &rgba, 3, &mut []), (0, 0), "nothing left at this factor");
        assert_eq!(bands_for(0), 1);
        assert!((1..=MAX_BANDS).contains(&bands_for(usize::MAX / 2)));
    }

    #[test]
    fn converts_into_a_reused_buffer() {
        let mut out = vec![9; 64];
        assert!(to_rgba_into(&[1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8], 1, 2, 8, PixelFormat::Bgra, &mut out));
        assert_eq!(out, [3, 2, 1, 4, 7, 6, 5, 8]);
        assert!(!to_rgba_into(&[1, 2, 3], 1, 1, 4, PixelFormat::Rgba, &mut out), "too short");
        assert_eq!(out, [3, 2, 1, 4, 7, 6, 5, 8], "unchanged on failure");
    }

    /// The display stream's downscale at each size and factor, old against new (one thread and
    /// banded), the median of `runs`; one line each.
    fn downscale_bench(sizes: &[(u32, u32)], factors: &[u32], runs: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for &(w, h) in sizes {
            let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
            for &factor in factors {
                let (ow, oh) = downscaled_size(w, h, factor);
                let mut out = vec![0; ow as usize * oh as usize * 4];
                let median = |f: &mut dyn FnMut()| {
                    let mut times: Vec<f64> = (0..runs)
                        .map(|_| {
                            let t = std::time::Instant::now();
                            f();
                            t.elapsed().as_secs_f64() * 1000.0
                        })
                        .collect();
                    times.sort_by(f64::total_cmp);
                    times[times.len() / 2]
                };
                let mut old_out = Vec::new();
                let old = median(&mut || old_out = if factor == 1 { rgba.clone() } else { naive_box(w, h, &rgba, factor) });
                let one = median(&mut || {
                    downscale_banded(w, h, &rgba, factor, &mut out, 1);
                });
                let new = median(&mut || {
                    downscale_into(w, h, &rgba, factor, &mut out);
                });
                assert_eq!(old_out[..out.len()], out[..], "the same pixels");
                let bands = bands_for(oh as usize * factor as usize * w as usize * 4);
                lines.push(format!("{w}x{h} /{factor}: old {old:.2} ms, new 1 thread {one:.2} ms, new {bands} bands {new:.2} ms"));
            }
        }
        lines
    }

    #[test]
    fn the_benchmark_runs_and_agrees_with_the_old_filter() {
        let lines = downscale_bench(&[(24, 18)], &[1, 2, 3, 4], 1);
        assert_eq!(lines.len(), 4, "{lines:?}");
    }

    /// The display stream's downscale on a tablet frame and a phone frame, old against new.
    ///     cargo test --release -p simee-core downscale_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn downscale_cost() {
        for l in downscale_bench(&[(2480, 1740), (1080, 2400)], &[1, 2, 3], 9) {
            println!("{l}");
        }
    }

    /// The previous implementation (const-generic per-pixel blocks into a new Vec), for the
    /// benchmark's before column.
    fn naive_box(width: u32, height: u32, rgba: &[u8], factor: u32) -> Vec<u8> {
        fn imp<const F: usize>(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
            let (ow, oh, in_row) = (width as usize / F, height as usize / F, width as usize * 4);
            let mut out = Vec::with_capacity(ow * oh * 4);
            let div = Reciprocal::new((F * F) as u32);
            for oy in 0..oh {
                let rows: [&[u8]; F] = std::array::from_fn(|dy| &rgba[(oy * F + dy) * in_row..][..ow * F * 4]);
                for ox in 0..ow {
                    let mut acc = [0u32; 4];
                    for row in rows {
                        for px in row[ox * F * 4..][..F * 4].as_chunks::<4>().0 {
                            for c in 0..4 {
                                acc[c] += u32::from(px[c]);
                            }
                        }
                    }
                    out.extend(acc.map(|v| div.of(v)));
                }
            }
            out
        }
        match factor {
            2 => imp::<2>(width, height, rgba),
            3 => imp::<3>(width, height, rgba),
            _ => imp::<4>(width, height, rgba),
        }
    }

    #[test]
    fn swaps_channels_and_drops_padding() {
        // 1x2 image, stride 8 (4 bytes of padding per row).
        let src = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0];
        assert_eq!(bgra_to_rgba(&src, 1, 2, 8), Some(vec![3, 2, 1, 255, 6, 5, 4, 255]));
    }

    #[test]
    fn rejects_short_buffers() {
        assert_eq!(bgra_to_rgba(&[0; 7], 2, 1, 8), None);
        assert_eq!(bgra_to_rgba(&[0; 8], 2, 1, 4), None);
        assert_eq!(bgra_to_rgba(&[], 0, 0, 0), Some(vec![]));
    }

    #[test]
    fn converts_every_virtio_gpu_format() {
        let px = [1, 2, 3, 4];
        let cases = [
            (1, [3, 2, 1, 4]),
            (2, [3, 2, 1, 255]),
            (3, [2, 3, 4, 1]),
            (4, [2, 3, 4, 255]),
            (67, [1, 2, 3, 4]),
            (68, [4, 3, 2, 255]),
            (121, [4, 3, 2, 1]),
            (134, [1, 2, 3, 255]),
        ];
        for (code, rgba) in cases {
            let format = PixelFormat::from_virtio_gpu(code).expect("known format");
            assert_eq!(to_rgba(&px, 1, 1, 4, format), Some(rgba.to_vec()), "format {code}");
        }
        assert_eq!(PixelFormat::from_virtio_gpu(999), None);
    }

    #[test]
    fn only_asymmetric_colorful_frames_reveal_a_layout() {
        let black = [0, 0, 0, 255].repeat(4);
        assert!(!reveals_layout(&black, 2, 2), "uniform");
        let grey_rows = [[9, 9, 9, 255].repeat(2), [1, 1, 1, 255].repeat(2)].concat();
        assert!(!reveals_layout(&grey_rows, 2, 2), "no red/blue difference");
        let symmetric = [[255, 0, 0, 255].repeat(2), [255, 0, 0, 255].repeat(2)].concat();
        assert!(!reveals_layout(&symmetric, 2, 2), "rows mirror each other");
        let telling = [[255, 0, 0, 255].repeat(2), [0, 0, 255, 255].repeat(2)].concat();
        assert!(reveals_layout(&telling, 2, 2));
        assert!(!reveals_layout(&telling[..4], 2, 2), "short");
        assert!(!reveals_layout(&[], 0, 0));
    }

    #[test]
    fn downscale_averages_blocks() {
        // 2x2 image: one white pixel, three black; factor 2 → one grey pixel.
        let px = [255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        assert_eq!(downscale(2, 2, &px, 2), (1, 1, vec![63, 63, 63, 255]));
        assert_eq!(downscale(2, 2, &px, 1).2, px.to_vec());
        assert_eq!((downscale_factor(1080, 720), downscale_factor(720, 720), downscale_factor(720, 0)), (2, 1, 720));
    }
}
