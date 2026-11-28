//! VideoToolbox: [`VtEncoder`] (a real-time `VTCompressionSession` per input size) and
//! [`Decoder`] (a `VTDecompressionSession` that checks access units decode).

use std::time::{Duration, Instant};

use sim_coresim::videotoolbox::{self as vt, CompressionSession, H264Config, H264Decoder, H264Profile, PixelBuffer};

use crate::annexb::{self, NAL_PPS, NAL_SPS, ParameterSets};
use crate::config::{EncoderConfig, Hardware, Profile, output_size};
use crate::stream::{AccessUnit, Encoder};
use crate::{Error, Result};

fn coresim(e: sim_coresim::Error) -> Error {
    match e {
        sim_coresim::Error::Unavailable(m) => Error::Unsupported(m),
        sim_coresim::Error::Invalid(m) => Error::InvalidInput(m),
        other => Error::Encoder(other.to_string()),
    }
}

struct Session {
    session: CompressionSession,
    input: (u32, u32),
    output: (u32, u32),
}

/// H.264 on VideoToolbox, real time, no frame reordering (so every picture comes out before the
/// next goes in), at most [`EncoderConfig::keyframe_frames`] frames between IDRs. The session is
/// made for the first picture's size and made again when the size changes (a rotation); a picture
/// larger than [`EncoderConfig::max_size`] is scaled down by VideoToolbox on the way in.
pub struct VtEncoder {
    config: EncoderConfig,
    bitrate: u32,
    session: Option<Session>,
    sets: ParameterSets,
    sessions: u64,
}

impl VtEncoder {
    pub fn new(config: EncoderConfig) -> Result<VtEncoder> {
        config.validate()?;
        let bitrate = config.clamp_bitrate(u64::from(config.bitrate_bps));
        Ok(VtEncoder { config, bitrate, session: None, sets: ParameterSets::default(), sessions: 0 })
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Whether the current session runs on the hardware encoder; `None` before the first picture.
    /// With [`Hardware::Prefer`], `Some(false)` means VideoToolbox fell back to software.
    pub fn hardware(&self) -> Option<bool> {
        self.session.as_ref().map(|s| s.session.hardware())
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

    fn h264_config(&self, (width, height): (u32, u32)) -> H264Config {
        let max_bytes = self.config.max_bitrate_bps.map(|bps| u64::from(bps) / 8);
        H264Config {
            width,
            height,
            profile: match self.config.profile {
                Profile::ConstrainedBaseline => H264Profile::ConstrainedBaseline,
                Profile::Main => H264Profile::Main,
            },
            hardware: match self.config.hardware {
                Hardware::Require => vt::Hardware::Require,
                Hardware::Prefer => vt::Hardware::Prefer,
                Hardware::Software => vt::Hardware::Avoid,
            },
            real_time: true,
            max_keyframe_interval: self.config.keyframe_frames(),
            max_keyframe_interval_duration: Some(self.config.keyframe_interval),
            average_bitrate: self.bitrate,
            data_rate_limits: max_bytes.map(|b| vec![(b, 1.0)]).unwrap_or_default(),
            expected_fps: f64::from(self.config.fps),
        }
    }

    fn session_for(&mut self, input: (u32, u32)) -> Result<&mut Session> {
        if self.session.as_ref().is_none_or(|s| s.input != input) {
            self.session = None;
            let output = output_size(input, self.config.max_size);
            let session = CompressionSession::new(&self.h264_config(output)).map_err(coresim)?;
            self.sessions += 1;
            self.session = Some(Session { session, input, output });
        }
        self.session.as_mut().ok_or_else(|| Error::Encoder("no session".into()))
    }
}

impl Encoder<PixelBuffer> for VtEncoder {
    fn encode(&mut self, picture: &PixelBuffer, pts: Duration, keyframe: bool) -> Result<Vec<AccessUnit>> {
        let session = self.session_for(picture.size())?;
        let start = Instant::now();
        session.session.encode(picture, pts, keyframe).map_err(coresim)?;
        let samples = session.session.complete(Some(pts)).map_err(coresim)?;
        let encode_time = start.elapsed();
        let mut out = Vec::with_capacity(samples.len());
        for sample in samples.into_iter().filter(|s| !s.dropped) {
            if sample.keyframe {
                self.sets.update(&sample.parameter_sets);
            }
            let data = self.sets.access_unit(&sample.data, sample.nal_length_size, sample.keyframe)?;
            out.push(AccessUnit { data, pts: sample.pts, keyframe: sample.keyframe, heartbeat: false, encode_time });
        }
        Ok(out)
    }

    fn set_bitrate(&mut self, bps: u64) -> Result<()> {
        self.bitrate = self.config.clamp_bitrate(bps);
        if let Some(s) = &self.session {
            s.session.set_bitrate(self.bitrate, &[]).map_err(coresim)?;
        }
        Ok(())
    }
}

/// Decodes Annex B access units with VideoToolbox, to check an encoder's output: each IDR's SPS
/// and PPS (re)make the decoder, and every access unit must come out as a picture.
#[derive(Default)]
pub struct Decoder {
    decoder: Option<(H264Decoder, Vec<u8>, Vec<u8>)>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// The decoded picture's size.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<(u32, u32)> {
        let units = annexb::annex_b_units(access_unit);
        let sps = units.iter().find(|n| annexb::nal_type(n) == NAL_SPS);
        let pps = units.iter().find(|n| annexb::nal_type(n) == NAL_PPS);
        if let (Some(sps), Some(pps)) = (sps, pps)
            && self.decoder.as_ref().is_none_or(|(_, s, p)| s != sps || p != pps)
        {
            let decoder = H264Decoder::new(&[sps, pps], 4).map_err(coresim)?;
            self.decoder = Some((decoder, sps.to_vec(), pps.to_vec()));
        }
        let (decoder, _, _) = self.decoder.as_mut().ok_or_else(|| Error::InvalidInput("no SPS and PPS before this access unit".into()))?;
        decoder.decode(&annexb::annex_b_to_length_prefixed(access_unit)).map_err(coresim)
    }
}
