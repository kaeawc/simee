//! What an encoder is asked for.

use std::time::Duration;

/// The H.264 profile. The level follows the output size and rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Baseline with `constraint_set1` (`42e0xx`): what every WebRTC endpoint decodes.
    ConstrainedBaseline,
    /// Main (`4d00xx`): CABAC, better quality per bit; every browser decodes it too.
    Main,
}

/// Whether to use the hardware (media engine) encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hardware {
    /// Fail when there is none.
    Require,
    /// Use it where there is one, else software ([`crate::VtEncoder::hardware`] reports which).
    Prefer,
    /// Software only.
    Software,
}

#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub profile: Profile,
    pub hardware: Hardware,
    /// Frames per second at most; also the rate the encoder expects.
    pub fps: u32,
    /// The average bitrate to aim for, in bits per second (a publisher's estimate retargets it).
    pub bitrate_bps: u32,
    /// A hard ceiling over any one second (`DataRateLimits`), and the most a retarget may ask
    /// for. `None`: no ceiling.
    pub max_bitrate_bps: Option<u32>,
    /// The longest time between keyframes; on-demand ones come in between.
    pub keyframe_interval: Duration,
    /// The longest edge of the output in pixels: larger frames are scaled down (aspect kept).
    pub max_size: Option<u32>,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            profile: Profile::ConstrainedBaseline,
            hardware: Hardware::Prefer,
            fps: 30,
            bitrate_bps: 4_000_000,
            max_bitrate_bps: Some(8_000_000),
            keyframe_interval: Duration::from_secs(2),
            max_size: None,
        }
    }
}

impl EncoderConfig {
    /// The most frames between keyframes at `fps`.
    pub fn keyframe_frames(&self) -> u32 {
        let n = (self.keyframe_interval.as_secs_f64() * f64::from(self.fps.max(1))).round();
        (n as u32).max(1)
    }

    /// The average to set for a requested bitrate: at least 100 kbit/s, at most the ceiling.
    pub fn clamp_bitrate(&self, bps: u64) -> u32 {
        let max = self.max_bitrate_bps.map_or(u64::from(u32::MAX), u64::from);
        bps.clamp(100_000, max.max(100_000)) as u32
    }

    pub fn validate(&self) -> crate::Result<()> {
        if self.fps == 0 || self.fps > 240 {
            return Err(crate::Error::InvalidInput(format!("fps {} is outside 1..=240", self.fps)));
        }
        if self.bitrate_bps < 10_000 {
            return Err(crate::Error::InvalidInput(format!("bitrate {} bit/s is too low", self.bitrate_bps)));
        }
        if self.max_size.is_some_and(|m| m < 16) {
            return Err(crate::Error::InvalidInput("max_size is below 16 pixels".into()));
        }
        Ok(())
    }
}

/// The output size for an input of `width` x `height`: scaled down, aspect kept, when its longest
/// edge is over `max_size`; both sides rounded down to even (4:2:0 chroma: VideoToolbox drops an
/// odd column anyway, as an iPhone's 1179-pixel-wide screen showed), never below 16.
pub fn output_size((width, height): (u32, u32), max_size: Option<u32>) -> (u32, u32) {
    let long = width.max(height);
    let scale = match max_size {
        Some(max) if long > max => f64::from(max) / f64::from(long),
        _ => 1.0,
    };
    let fit = |side: u32| (((f64::from(side) * scale) as u32) & !1).max(16);
    (fit(width), fit(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_keeps_small_frames_and_scales_large_ones_to_even_sides() {
        assert_eq!(output_size((1179, 2556), None), (1178, 2556), "an odd side rounds down to even");
        assert_eq!(output_size((1206, 2622), Some(2622)), (1206, 2622), "fits exactly");
        assert_eq!(output_size((1179, 2556), Some(1280)), (590, 1280));
        assert_eq!(output_size((2560, 1600), Some(1280)), (1280, 800));
        assert_eq!(output_size((1080, 2400), Some(1920)), (864, 1920));
        assert_eq!(output_size((4000, 20), Some(100)), (100, 16), "never below 16");
    }

    #[test]
    fn keyframe_frames_and_bitrate_clamps() {
        let c = EncoderConfig { fps: 60, keyframe_interval: Duration::from_millis(2500), ..EncoderConfig::default() };
        assert_eq!(c.keyframe_frames(), 150);
        assert_eq!(EncoderConfig { keyframe_interval: Duration::ZERO, ..c.clone() }.keyframe_frames(), 1);
        assert_eq!(c.clamp_bitrate(20_000_000), 8_000_000);
        assert_eq!(c.clamp_bitrate(5), 100_000);
        assert_eq!(EncoderConfig { max_bitrate_bps: None, ..c.clone() }.clamp_bitrate(20_000_000), 20_000_000);
        assert!(c.validate().is_ok());
        assert!(EncoderConfig { fps: 0, ..c.clone() }.validate().is_err());
        assert!(EncoderConfig { max_size: Some(8), ..c }.validate().is_err());
    }
}
