//! Frames as sources produce them: tightly packed BGRA behind an `Arc`, so a frame fanned out to
//! many subscribers (and a still source that repeats one image) is never copied.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};

/// The size and rate a consumer asks a source for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl FrameSpec {
    pub const MIN_SIDE: u32 = 32;
    pub const MAX_SIDE: u32 = 8192;
    pub const MAX_FPS: u32 = 240;

    /// A checked spec: each side 32..=8192 pixels, 1..=240 fps.
    pub fn new(width: u32, height: u32, fps: u32) -> Result<FrameSpec> {
        let side = Self::MIN_SIDE..=Self::MAX_SIDE;
        if !side.contains(&width) || !side.contains(&height) {
            return Err(Error::Invalid(format!(
                "frame size {width}x{height} is out of range ({}..={} per side)",
                Self::MIN_SIDE,
                Self::MAX_SIDE
            )));
        }
        if !(1..=Self::MAX_FPS).contains(&fps) {
            return Err(Error::Invalid(format!("frame rate {fps} is out of range (1..={})", Self::MAX_FPS)));
        }
        Ok(FrameSpec { width, height, fps })
    }

    /// The time between frames.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(1) / self.fps.max(1)
    }

    /// Pixels per frame, for picking the largest of several requests.
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

impl Default for FrameSpec {
    /// VGA at 30 fps: what the goldfish camera HAL and most webcams offer first.
    fn default() -> FrameSpec {
        FrameSpec { width: 640, height: 480, fps: 30 }
    }
}

/// When a frame is produced: its number in the capture, its time since the capture started, and
/// the wall clock (drawn into the test card, so a consumer can measure end-to-end latency).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    pub seq: u64,
    pub pts: Duration,
    pub wall: SystemTime,
}

impl Tick {
    /// The first tick of a capture started now.
    pub fn start() -> Tick {
        Tick { seq: 0, pts: Duration::ZERO, wall: SystemTime::now() }
    }
}

/// One frame: opaque BGRA, top row first, `width * 4` bytes per row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Arc<[u8]>,
    pub tick: Tick,
}

impl CameraFrame {
    /// The BGRA bytes of pixel (x, y).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [self.bgra[i], self.bgra[i + 1], self.bgra[i + 2], self.bgra[i + 3]]
    }

    /// This frame as RGBA, for a PNG preview (`sim_core::frame::encode_png`).
    pub fn to_rgba(&self) -> sim_core::frame::Frame {
        let (w, h) = (self.width as usize, self.height as usize);
        let rgba = sim_core::pixels::bgra_to_rgba(&self.bgra, w, h, w * 4).unwrap_or_default();
        sim_core::frame::Frame { width: self.width, height: self.height, rgba }
    }

    /// This frame at another size, or itself (sharing its pixels) when the size already matches.
    pub fn fit(&self, width: u32, height: u32) -> CameraFrame {
        if (width, height) == (self.width, self.height) {
            return self.clone();
        }
        let bgra = fill(self.width, self.height, &self.bgra, width, height);
        CameraFrame { width, height, bgra: bgra.into(), tick: self.tick }
    }
}

/// One picture's pixels fitted to other sizes, by size (`(width, height)`), made once each.
pub(crate) type Fitted = Vec<((u32, u32), Arc<[u8]>)>;

/// Scales 4-byte pixels to `dst_w`x`dst_h` the way a camera fills its sensor: keep the aspect
/// ratio, cover the whole frame, and crop the overflow equally from both sides (nearest
/// neighbour; sources here are test patterns and stills, where speed matters more than filtering).
pub fn fill(src_w: u32, src_h: u32, src: &[u8], dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];
    if src_w == 0 || src_h == 0 || src.len() < src_w as usize * src_h as usize * 4 {
        return out;
    }
    // The visible window of the source: as wide as the destination's aspect ratio allows.
    let (sw, sh, dw, dh) = (u64::from(src_w), u64::from(src_h), u64::from(dst_w), u64::from(dst_h));
    let (win_w, win_h) = if sw * dh > dw * sh { ((sh * dw / dh).max(1), sh) } else { (sw, (sw * dh / dw).max(1)) };
    let (x0, y0) = ((sw - win_w) / 2, (sh - win_h) / 2);
    let columns: Vec<usize> = (0..dw).map(|x| ((x0 + x * win_w / dw) * 4) as usize).collect();
    for (y, row) in out.chunks_exact_mut(dst_w as usize * 4).enumerate() {
        let sy = (y0 + y as u64 * win_h / dh) as usize;
        let src_row = &src[sy * src_w as usize * 4..][..src_w as usize * 4];
        for (px, &sx) in row.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            px.copy_from_slice(&src_row[sx..sx + 4]);
        }
    }
    out
}

/// Copies `height` rows of `width` 4-byte pixels, `stride` bytes apart, into tight rows turned
/// `quarter_turns` times clockwise (a recording's rotation, applied so it plays upright). Returns
/// the turned width and height with the pixels; `None` when `src` is shorter than it claims.
pub fn turn(src: &[u8], stride: usize, width: u32, height: u32, quarter_turns: u8) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = (width as usize, height as usize);
    if stride < w * 4 || src.len() < stride * h.saturating_sub(1) + w * 4 {
        return None;
    }
    let rows = || src.chunks(stride).take(h).map(|row| &row[..w * 4]);
    let turns = quarter_turns % 4;
    if turns == 0 {
        let mut out = Vec::with_capacity(w * h * 4);
        rows().for_each(|row| out.extend_from_slice(row));
        return Some((width, height, out));
    }
    let (out_w, out_h) = if turns == 2 { (w, h) } else { (h, w) };
    let mut out = vec![0u8; w * h * 4];
    for (y, row) in rows().enumerate() {
        for (x, px) in row.as_chunks::<4>().0.iter().enumerate() {
            // Where source pixel (x, y) lands, turned clockwise.
            let (ox, oy) = match turns {
                1 => (h - 1 - y, x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let at = (oy * out_w + ox) * 4;
            out[at..at + 4].copy_from_slice(px);
        }
    }
    Some((out_w as u32, out_h as u32, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3x2 image of pixel numbers (`[n, 0, 0, 255]`), rows padded to 16 bytes.
    fn numbered() -> Vec<u8> {
        let mut src = Vec::new();
        for y in 0..2u8 {
            for x in 0..3u8 {
                src.extend_from_slice(&[y * 3 + x, 0, 0, 255]);
            }
            src.extend_from_slice(&[0xee; 4]);
        }
        src
    }

    fn numbers(px: &[u8]) -> Vec<u8> {
        px.as_chunks::<4>().0.iter().map(|p| p[0]).collect()
    }

    #[test]
    fn turns_rotate_clockwise_and_drop_the_row_padding() {
        // 0 1 2
        // 3 4 5
        let src = numbered();
        let at = |turns| turn(&src, 16, 3, 2, turns).map(|(w, h, px)| (w, h, numbers(&px)));
        assert_eq!(at(0), Some((3, 2, vec![0, 1, 2, 3, 4, 5])));
        assert_eq!(at(1), Some((2, 3, vec![3, 0, 4, 1, 5, 2])), "a quarter turn: the left column becomes the top row");
        assert_eq!(at(2), Some((3, 2, vec![5, 4, 3, 2, 1, 0])));
        assert_eq!(at(3), Some((2, 3, vec![2, 5, 1, 4, 0, 3])));
        assert_eq!(at(4), at(0), "four turns are none");
        assert_eq!(turn(&src[..20], 16, 3, 2, 0), None, "shorter than it claims");
        assert_eq!(turn(&src, 8, 3, 2, 0), None, "rows shorter than the width");
    }

    #[test]
    fn specs_are_range_checked() {
        assert!(FrameSpec::new(640, 480, 30).is_ok());
        assert!(FrameSpec::new(16, 480, 30).is_err());
        assert!(FrameSpec::new(640, 9000, 30).is_err());
        assert!(FrameSpec::new(640, 480, 0).is_err());
        assert!(FrameSpec::new(640, 480, 241).is_err());
        assert_eq!(FrameSpec::default().interval(), Duration::from_nanos(33_333_333));
    }

    #[test]
    fn fill_crops_the_overflow_from_both_sides() {
        // 4x2, both rows red, green, blue, white (BGRA). Into a square, the middle two columns
        // survive.
        let row = [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255];
        let src = row.repeat(2);
        let out = fill(4, 2, &src, 2, 2);
        assert_eq!(&out[0..4], &[0, 255, 0, 255]);
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
        assert_eq!(&out[8..16], &out[0..8]);
        // Scaling up repeats pixels; a 1-row source fills every row.
        let up = fill(4, 1, &row, 8, 2);
        assert_eq!(&up[0..8], &[0, 0, 255, 255, 0, 0, 255, 255]);
        assert_eq!(&up[32..64], &up[0..32]);
    }

    #[test]
    fn fit_shares_pixels_when_the_size_matches() {
        let frame = CameraFrame { width: 2, height: 1, bgra: vec![1; 8].into(), tick: Tick::start() };
        assert!(Arc::ptr_eq(&frame.fit(2, 1).bgra, &frame.bgra));
        let big = frame.fit(4, 2);
        assert_eq!((big.width, big.height, big.bgra.len()), (4, 2, 32));
        assert_eq!(frame.to_rgba().rgba, [1, 1, 1, 255].repeat(2), "opaque RGBA");
    }
}
