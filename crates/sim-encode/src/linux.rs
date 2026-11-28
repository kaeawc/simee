//! OpenH264 on Linux: [`OpenH264Encoder`] (Cisco's encoder, built from source with the crate's
//! `openh264` feature) and [`Decoder`] (its decoder, to check that output decodes).

use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::decoder::Decoder as RawDecoder;
use openh264::encoder::{
    BitRate, Encoder as RawEncoder, EncoderConfig as RawConfig, FrameRate, FrameType, IntraFramePeriod, Profile as RawProfile,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;

use crate::annexb::ParameterSets;
use crate::config::{EncoderConfig, Hardware, Profile, output_size};
use crate::i420::{Converter, I420, Picture};
use crate::stream::{AccessUnit, Encoder};
use crate::{Error, Result};

fn encoder_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Encoder(format!("{what}: {e}"))
}

/// The widest side OpenH264 encodes (level 5.2: 3840x2160, either way up).
const MAX_LONG_SIDE: u32 = 3840;
const MAX_SHORT_SIDE: u32 = 2160;

/// The ceiling OpenH264 is given when [`EncoderConfig::max_bitrate_bps`] is `None`, and the most
/// any target can be: 240 Mbit/s, under level 5.2's maximum. Above that OpenH264 pulls its ceiling
/// back to the stream's level maximum, which can fall under the target and refuse it.
const NO_CEILING: u32 = 240_000_000;

/// An [`I420`] as the encoder's input.
struct Planes<'a>(&'a I420);

impl YUVSource for Planes<'_> {
    fn dimensions(&self) -> (usize, usize) {
        let (w, h) = self.0.size();
        (w as usize, h as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.0.stride(), self.0.stride() / 2, self.0.stride() / 2)
    }

    fn y(&self) -> &[u8] {
        self.0.y()
    }

    fn u(&self) -> &[u8] {
        self.0.u()
    }

    fn v(&self) -> &[u8] {
        self.0.v()
    }
}

struct Session {
    encoder: RawEncoder,
    input: (u32, u32),
    output: (u32, u32),
    /// The bitrate the encoder is running at; `None` until its ceiling has been set, which can
    /// only happen once the first picture has initialised it.
    applied: Option<u32>,
    /// The timestamp the last picture went in with, in milliseconds.
    last_millis: Option<u64>,
}

/// H.264 on OpenH264, in software, real time, no frame reordering (every picture comes out as it
/// goes in), at most [`EncoderConfig::keyframe_frames`] frames between IDRs; Constrained Baseline
/// or Main (with CAVLC entropy coding, which OpenH264 uses for both). The session is made for the
/// first picture's size and made again when the size changes (a rotation); a picture larger than
/// [`EncoderConfig::max_size`] is scaled down first ([`crate::i420`]). The encoder may skip a
/// picture to hold its bitrate: that comes back as no access unit, like a dropped one on
/// VideoToolbox.
pub struct OpenH264Encoder {
    config: EncoderConfig,
    bitrate: u32,
    session: Option<Session>,
    sets: ParameterSets,
    sessions: u64,
    converter: Converter,
    stream: Vec<u8>,
}

impl OpenH264Encoder {
    pub fn new(config: EncoderConfig) -> Result<OpenH264Encoder> {
        config.validate()?;
        if config.hardware == Hardware::Require {
            return Err(Error::Unsupported("OpenH264 is a software encoder; Linux has no hardware H.264 path here yet".into()));
        }
        let bitrate = config.clamp_bitrate(u64::from(config.bitrate_bps)).min(NO_CEILING);
        Ok(OpenH264Encoder {
            config,
            bitrate,
            session: None,
            sets: ParameterSets::default(),
            sessions: 0,
            converter: Converter::new(),
            stream: Vec::new(),
        })
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Always `Some(false)` once there is a session: this encoder is software; `None` before the
    /// first picture, like [`crate::VtEncoder::hardware`].
    pub fn hardware(&self) -> Option<bool> {
        self.session.as_ref().map(|_| false)
    }

    /// The size pictures come out at; `None` before the first.
    pub fn output_size(&self) -> Option<(u32, u32)> {
        self.session.as_ref().map(|s| s.output)
    }

    /// The SPS and PPS of the stream so far.
    pub fn parameter_sets(&self) -> &ParameterSets {
        &self.sets
    }

    /// Sessions made so far (more than one after a size change).
    pub fn sessions(&self) -> u64 {
        self.sessions
    }

    /// The average bitrate the encoder is running at, in bits per second.
    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn raw_config(&self) -> RawConfig {
        RawConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(self.bitrate))
            .max_frame_rate(FrameRate::from_hz(self.config.fps as f32))
            .skip_frames(true)
            .profile(match self.config.profile {
                Profile::ConstrainedBaseline => RawProfile::Baseline,
                Profile::Main => RawProfile::Main,
            })
            .intra_frame_period(IntraFramePeriod::from_num_frames(self.config.keyframe_frames()))
            // Screen content mode turns these on or off itself and logs a warning to stderr when
            // asked for the other; ask for what it will do.
            .scene_change_detect(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .vui(VuiConfig::bt709())
    }

    fn ensure_session(&mut self, input: (u32, u32)) -> Result<()> {
        if self.session.as_ref().is_none_or(|s| s.input != input) {
            self.session = None;
            let output = output_size(input, self.config.max_size);
            let (long, short) = (output.0.max(output.1), output.0.min(output.1));
            if long > MAX_LONG_SIDE || short > MAX_SHORT_SIDE {
                return Err(Error::Unsupported(format!(
                    "OpenH264 encodes up to {MAX_LONG_SIDE}x{MAX_SHORT_SIDE}; a {}x{} picture needs EncoderConfig::max_size",
                    output.0, output.1
                )));
            }
            let encoder = RawEncoder::with_api_config(OpenH264API::from_source(), self.raw_config())
                .map_err(|e| encoder_error("creating the encoder", e))?;
            self.sessions += 1;
            self.session = Some(Session { encoder, input, output, applied: None, last_millis: None });
        }
        Ok(())
    }

    /// What a retarget may reach: [`EncoderConfig::max_bitrate_bps`], or [`NO_CEILING`] without
    /// one. Never under the target itself.
    fn ceiling(&self, bps: u32) -> u32 {
        self.config.max_bitrate_bps.unwrap_or(NO_CEILING).min(NO_CEILING).max(bps)
    }
}

/// Sets the target (and the ceiling it may not pass) of a running encoder, in bits per second.
#[allow(unsafe_code)]
fn retarget(encoder: &mut RawEncoder, bps: u32, ceiling: u32) -> Result<()> {
    use openh264_sys2::{
        ENCODER_OPTION, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, LAYER_NUM, SBitrateInfo, SPATIAL_LAYER_0, SPATIAL_LAYER_ALL,
    };
    let mut set = |option: ENCODER_OPTION, layer: LAYER_NUM, bits: u32| -> Result<()> {
        let mut info = SBitrateInfo { iLayer: layer, iBitrate: i32::try_from(bits).unwrap_or(i32::MAX) };
        // SAFETY: `raw_api` is `unsafe` because a caller can change parameters the safe wrapper
        // relies on; the bitrate and its ceiling are not among them (the wrapper reads neither
        // back). The encoder is initialised (this runs after its first picture). `SetOption`
        // takes a pointer to an `SBitrateInfo` for these two options, reads it during the call
        // and keeps no reference to it.
        let rc = unsafe { encoder.raw_api().set_option(option, (&raw mut info).cast()) };
        if rc == 0 { Ok(()) } else { Err(Error::Encoder(format!("SetOption({option}) returned {rc}"))) }
    };
    // The ceiling first, on the one spatial layer (`SPATIAL_LAYER_ALL` would set another limit): a
    // target above the layer's ceiling is refused.
    set(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, ceiling)?;
    set(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, bps)
}

impl Encoder<Picture> for OpenH264Encoder {
    fn encode(&mut self, picture: &Picture, pts: Duration, keyframe: bool) -> Result<Vec<AccessUnit>> {
        let start = Instant::now();
        let bitrate = self.bitrate;
        let ceiling = self.ceiling(bitrate);
        self.ensure_session(picture.size())?;
        let session = self.session.as_mut().ok_or_else(|| Error::Encoder("no session".into()))?;
        let planes = self.converter.convert(&picture.raster(), session.output)?;
        if keyframe {
            session.encoder.force_intra_frame();
        }
        let first = session.applied.is_none();
        if let Some(applied) = session.applied
            && applied != bitrate
        {
            retarget(&mut session.encoder, bitrate, ceiling)?;
            session.applied = Some(bitrate);
        }
        // OpenH264's rate control reads the timestamps as milliseconds; two pictures less than a
        // millisecond apart (an IDR that re-encodes the last picture) still get distinct ones.
        let millis = (pts.as_millis() as u64).max(session.last_millis.map_or(0, |m| m + 1));
        session.last_millis = Some(millis);
        let bitstream =
            session.encoder.encode_at(&Planes(planes), Timestamp::from_millis(millis)).map_err(|e| encoder_error("encoding", e))?;
        self.stream.clear();
        let skipped = match bitstream.frame_type() {
            FrameType::Skip => true,
            FrameType::Invalid => return Err(Error::Encoder("the encoder isn't ready or refused the parameters".into())),
            _ => {
                bitstream.write_vec(&mut self.stream);
                false
            }
        };
        if first {
            // The ceiling can only be set on an initialised encoder, and the first picture is
            // what initialises it. The encoder started at the target, so this picture is held to
            // it; a later request for more is let through.
            retarget(&mut session.encoder, bitrate, ceiling)?;
            session.applied = Some(bitrate);
        }
        if skipped || self.stream.is_empty() {
            return Ok(Vec::new());
        }
        let (data, keyframe) = self.sets.annex_b_access_unit(&self.stream)?;
        Ok(vec![AccessUnit { data, pts, keyframe, heartbeat: false, encode_time: start.elapsed() }])
    }

    fn set_bitrate(&mut self, bps: u64) -> Result<()> {
        self.bitrate = self.config.clamp_bitrate(bps).min(NO_CEILING);
        Ok(())
    }
}

/// Decodes Annex B access units with OpenH264's decoder, to check an encoder's output: every
/// access unit must come out as a picture.
pub struct Decoder {
    decoder: RawDecoder,
    planes: Decoded,
}

/// A decoded picture's planes, tightly packed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Result<Decoder> {
        let decoder = RawDecoder::new().map_err(|e| encoder_error("creating the decoder", e))?;
        Ok(Decoder { decoder, planes: Decoded::default() })
    }

    /// The decoded picture's size.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<(u32, u32)> {
        let p = self.decode_planes(access_unit)?;
        Ok((p.width, p.height))
    }

    /// The decoded picture, tightly packed (valid until the next call).
    pub fn decode_planes(&mut self, access_unit: &[u8]) -> Result<&Decoded> {
        let yuv = self
            .decoder
            .decode(access_unit)
            .map_err(|e| encoder_error("decoding", e))?
            .ok_or_else(|| Error::Encoder("the access unit decoded to no picture".into()))?;
        let (w, h) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let copy = |plane: &[u8], stride: usize, width: usize, height: usize, out: &mut Vec<u8>| {
            out.clear();
            for row in plane.chunks(stride).take(height) {
                out.extend_from_slice(&row[..width]);
            }
        };
        let p = &mut self.planes;
        (p.width, p.height) = (w as u32, h as u32);
        copy(yuv.y(), ys, w, h, &mut p.y);
        copy(yuv.u(), us, w / 2, h / 2, &mut p.u);
        copy(yuv.v(), vs, w / 2, h / 2, &mut p.v);
        Ok(&self.planes)
    }
}
