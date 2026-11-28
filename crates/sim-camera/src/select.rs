//! Picking a camera mode for a request: the size nearest the one asked for, then the frame rate
//! nearest it at that size, then the cheapest pixel format. Shared by V4L2 (`ENUM_FMT`,
//! `ENUM_FRAMESIZES`, `ENUM_FRAMEINTERVALS`) and AVFoundation (`AVCaptureDevice.formats` and their
//! frame rate ranges); pure, so it is tested with made-up format lists.

use std::cmp::Ordering;

use crate::frame::FrameSpec;

/// Seconds per frame as a fraction, the way both hosts state rates (`v4l2_fract`, `CMTime`):
/// 1/30 is 30 fps, 1001/30000 is 29.97.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Interval {
    pub num: u64,
    pub den: u64,
}

impl Interval {
    pub(crate) fn fps(self) -> f64 {
        if self.num == 0 { 0.0 } else { self.den as f64 / self.num as f64 }
    }

    /// Whether `fps` frames a second is at least as fast as this interval (exact, no floats).
    fn not_slower_than(self, fps: u32) -> bool {
        u128::from(self.num) * u128::from(fps) <= u128::from(self.den)
    }

    /// Whether `fps` frames a second is at least as slow as this interval.
    fn not_faster_than(self, fps: u32) -> bool {
        u128::from(self.num) * u128::from(fps) >= u128::from(self.den)
    }
}

/// A run of frame rates a mode offers: one rate (`fastest == slowest`) or a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rates {
    /// The shortest interval (the highest rate).
    pub fastest: Interval,
    /// The longest interval (the lowest rate).
    pub slowest: Interval,
}

impl Rates {
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn one(interval: Interval) -> Rates {
        Rates { fastest: interval, slowest: interval }
    }

    /// The interval in this run nearest `fps`: `1/fps` itself when it is inside, else the nearer
    /// end exactly as the host stated it (a host refuses a duration outside its ranges).
    fn nearest(self, fps: u32) -> (f64, Interval) {
        if self.fastest.not_faster_than(fps) {
            return ((self.fastest.fps() - f64::from(fps)).abs(), self.fastest);
        }
        if self.slowest.not_slower_than(fps) {
            return ((self.slowest.fps() - f64::from(fps)).abs(), self.slowest);
        }
        (0.0, Interval { num: 1, den: u64::from(fps) })
    }
}

/// One size in one pixel format, with the rates it runs at (empty when the host doesn't say).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Mode {
    pub width: u32,
    pub height: u32,
    /// Lower is preferred among modes of equal size and rate (cheaper to convert).
    pub rank: u32,
    pub rates: Vec<Rates>,
}

/// The mode to use and the frame interval to ask for (`None` when the mode states no rates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub index: usize,
    pub interval: Option<Interval>,
}

/// How far a size is from the request: the ratio of the areas and of the aspect ratios, both on
/// a log scale, so half the pixels and twice the pixels are equally far.
fn size_cost(width: u32, height: u32, want: FrameSpec) -> f64 {
    let (w, h, ww, wh) = (f64::from(width), f64::from(height), f64::from(want.width), f64::from(want.height));
    ((w * h) / (ww * wh)).ln().abs() + ((w / h) / (ww / wh)).ln().abs()
}

/// The mode nearest `want`: nearest size first (a tie goes to the larger, which scales down rather
/// than up), then the nearest rate at that size (a tie goes to the faster), then the lowest rank,
/// then the host's order. `None` for an empty list or one with no usable size.
pub(crate) fn nearest(modes: &[Mode], want: FrameSpec) -> Option<Choice> {
    struct Scored {
        index: usize,
        size: f64,
        area: u64,
        fps: f64,
        faster: f64,
        rank: u32,
        interval: Option<Interval>,
    }
    let scored = modes.iter().enumerate().filter(|(_, m)| m.width > 0 && m.height > 0).map(|(index, m)| {
        let best = m.rates.iter().map(|r| r.nearest(want.fps)).min_by(|a, b| a.0.total_cmp(&b.0).then(b.1.fps().total_cmp(&a.1.fps())));
        Scored {
            index,
            size: size_cost(m.width, m.height, want),
            area: u64::from(m.width) * u64::from(m.height),
            // A mode that states no rates loses to any that does, at the same size.
            fps: best.map_or(f64::INFINITY, |b| b.0),
            faster: best.map_or(0.0, |b| b.1.fps()),
            rank: m.rank,
            interval: best.map(|b| b.1),
        }
    });
    let order = |a: &Scored, b: &Scored| -> Ordering {
        a.size
            .total_cmp(&b.size)
            .then(b.area.cmp(&a.area))
            .then(a.fps.total_cmp(&b.fps))
            .then(b.faster.total_cmp(&a.faster))
            .then(a.rank.cmp(&b.rank))
            .then(a.index.cmp(&b.index))
    };
    scored.min_by(order).map(|s| Choice { index: s.index, interval: s.interval })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn per(fps: u64) -> Interval {
        Interval { num: 1, den: fps }
    }

    pub(crate) fn mode(width: u32, height: u32, rank: u32, rates: &[u64]) -> Mode {
        Mode { width, height, rank, rates: rates.iter().map(|&f| Rates::one(per(f))).collect() }
    }

    fn want(w: u32, h: u32, fps: u32) -> FrameSpec {
        FrameSpec { width: w, height: h, fps }
    }

    /// A FaceTime HD-like list, as AVFoundation reports it: each format with one 1..30 fps range.
    fn facetime() -> Vec<Mode> {
        let range = Rates { fastest: per(30), slowest: per(1) };
        [(1920, 1080), (1280, 720), (1080, 1080), (1760, 1328), (640, 480)]
            .into_iter()
            .map(|(width, height)| Mode { width, height, rank: 0, rates: vec![range] })
            .collect()
    }

    #[test]
    fn an_exact_size_and_rate_is_taken_as_is() {
        let choice = nearest(&facetime(), want(1280, 720, 30));
        assert_eq!(choice, Some(Choice { index: 1, interval: Some(per(30)) }));
    }

    #[test]
    fn a_rate_inside_a_range_is_asked_for_exactly_and_one_outside_snaps_to_its_end() {
        assert_eq!(nearest(&facetime(), want(640, 480, 15)), Some(Choice { index: 4, interval: Some(per(15)) }));
        // 60 fps is past the range: its fastest end, as the host stated it.
        assert_eq!(nearest(&facetime(), want(640, 480, 60)), Some(Choice { index: 4, interval: Some(per(30)) }));
        let ntsc = Interval { num: 1001, den: 30000 };
        let modes = [Mode { width: 1280, height: 720, rank: 0, rates: vec![Rates::one(ntsc)] }];
        assert_eq!(nearest(&modes, want(1280, 720, 30)), Some(Choice { index: 0, interval: Some(ntsc) }));
    }

    #[test]
    fn the_nearest_size_wins_by_area_and_aspect_ratio() {
        // 1600x900: 1920x1080 has 1.44x the pixels, 1280x720 0.64x; the larger is nearer.
        assert_eq!(nearest(&facetime(), want(1600, 900, 30)).map(|c| c.index), Some(0));
        // 800x600 (4:3) is nearer 640x480 than 1280x720 (16:9, 1.9x the pixels).
        assert_eq!(nearest(&facetime(), want(800, 600, 30)).map(|c| c.index), Some(4));
        // A square request takes the square format.
        assert_eq!(nearest(&facetime(), want(1000, 1000, 30)).map(|c| c.index), Some(2));
        // Past every size: the largest.
        assert_eq!(nearest(&facetime(), want(4096, 2160, 30)).map(|c| c.index), Some(0));
    }

    #[test]
    fn an_equally_near_size_goes_to_the_larger() {
        let modes = [mode(500, 500, 0, &[30]), mode(2000, 2000, 0, &[30])];
        assert_eq!(nearest(&modes, want(1000, 1000, 30)).map(|c| c.index), Some(1));
    }

    #[test]
    fn at_one_size_the_rate_decides_before_the_pixel_format() {
        // A UVC webcam: YUYV (rank 0) is bandwidth-limited at 720p; MJPEG (rank 1) isn't.
        let modes = [mode(640, 480, 0, &[30, 15]), mode(1280, 720, 0, &[10, 5]), mode(1280, 720, 1, &[30, 15])];
        assert_eq!(nearest(&modes, want(1280, 720, 30)), Some(Choice { index: 2, interval: Some(per(30)) }));
        assert_eq!(nearest(&modes, want(1280, 720, 10)), Some(Choice { index: 1, interval: Some(per(10)) }));
        // Equal rates: the lower rank.
        assert_eq!(nearest(&modes, want(1280, 720, 15)), Some(Choice { index: 2, interval: Some(per(15)) }));
        let same = [mode(640, 480, 1, &[30]), mode(640, 480, 0, &[30])];
        assert_eq!(nearest(&same, want(640, 480, 30)).map(|c| c.index), Some(1));
    }

    #[test]
    fn between_two_discrete_rates_the_nearer_then_the_faster() {
        let modes = [mode(640, 480, 0, &[30, 15, 5])];
        assert_eq!(nearest(&modes, want(640, 480, 24)).and_then(|c| c.interval), Some(per(30)));
        assert_eq!(nearest(&modes, want(640, 480, 10)).and_then(|c| c.interval), Some(per(15)), "a tie goes to the faster");
        assert_eq!(nearest(&modes, want(640, 480, 1)).and_then(|c| c.interval), Some(per(5)));
    }

    #[test]
    fn a_mode_without_rates_is_used_with_no_interval() {
        let modes = [mode(640, 480, 0, &[])];
        assert_eq!(nearest(&modes, want(640, 480, 30)), Some(Choice { index: 0, interval: None }));
        // At one size, one that states a rate is preferred.
        let both = [mode(640, 480, 0, &[]), mode(640, 480, 1, &[15])];
        assert_eq!(nearest(&both, want(640, 480, 30)).map(|c| c.index), Some(1));
    }

    #[test]
    fn nothing_usable_chooses_nothing() {
        assert_eq!(nearest(&[], FrameSpec::default()), None);
        assert_eq!(nearest(&[mode(0, 0, 0, &[30])], FrameSpec::default()), None);
    }
}
