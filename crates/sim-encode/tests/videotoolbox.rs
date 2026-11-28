//! VideoToolbox end to end on synthetic frames (no device): profiles, forced and periodic IDRs
//! with their SPS/PPS, timestamps, scaling, RGBA input, and that every access unit decodes. A
//! host with no H.264 encoder at all (some macOS VMs) prints SKIP.
#![cfg(target_os = "macos")]

use std::time::Duration;

use sim_coresim::videotoolbox::{PixelBuffer, PixelBufferPool, PixelFormat};
use sim_rtc::{Profile as RtcProfile, ProfileLevelId};
use simee_encode::annexb::{self, NAL_IDR, NAL_PPS, NAL_SPS};
use simee_encode::{AccessUnit, Decoder, Encoder, EncoderConfig, Error, Profile, VtEncoder};

/// A moving diagonal gradient with a block that slides across, `frame` steps along.
fn picture(pool: &PixelBufferPool, frame: u32) -> PixelBuffer {
    let (w, h) = pool.size();
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let v = ((x + y + frame * 7) % 256) as u8;
            let block = (x / 16 + frame).is_multiple_of(8) && y < h / 2;
            px[i..i + 4].copy_from_slice(&if block { [255, 255, 255, 255] } else { [v, v / 2, 255 - v, 255] });
        }
    }
    pool.fill(&px, (w * 4) as usize).expect("fill")
}

/// Encodes `n` frames at 30 fps, forcing an IDR at the indices in `force`. `None` when the host
/// has no encoder for the configuration.
fn encode(config: EncoderConfig, size: (u32, u32), format: PixelFormat, n: u32, force: &[u32]) -> Option<(Vec<AccessUnit>, VtEncoder)> {
    let pool = PixelBufferPool::new(size.0, size.1, format).expect("pool");
    let mut encoder = VtEncoder::new(config).expect("config");
    let mut units = Vec::new();
    for i in 0..n {
        let pts = Duration::from_secs(1) / 30 * i;
        match encoder.encode(&picture(&pool, i), pts, force.contains(&i)) {
            Ok(out) => units.extend(out),
            Err(Error::Unsupported(m)) => {
                eprintln!("SKIP: {m}");
                return None;
            }
            Err(e) => panic!("frame {i}: {e}"),
        }
    }
    Some((units, encoder))
}

fn types(unit: &AccessUnit) -> Vec<u8> {
    annexb::annex_b_units(&unit.data).iter().map(|n| annexb::nal_type(n)).collect()
}

fn sps(unit: &AccessUnit) -> ProfileLevelId {
    let units = annexb::annex_b_units(&unit.data);
    let sps = units.iter().find(|n| annexb::nal_type(n) == NAL_SPS).expect("an SPS");
    ProfileLevelId::from_sps(sps).expect("profile-level-id")
}

#[test]
fn constrained_baseline_with_forced_keyframes_decodes() {
    let config = EncoderConfig { keyframe_interval: Duration::from_secs(10), ..EncoderConfig::default() };
    let Some((units, encoder)) = encode(config, (320, 240), PixelFormat::Bgra, 12, &[5, 9]) else { return };
    assert_eq!(units.len(), 12, "one access unit per picture");
    let keys: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys, vec![0, 5, 9], "the first picture and the forced ones");
    for (i, u) in units.iter().enumerate() {
        let t = types(u);
        if u.keyframe {
            // SPS and PPS in front of every IDR, not only the first.
            let (s, p, idr) =
                (t.iter().position(|&x| x == NAL_SPS), t.iter().position(|&x| x == NAL_PPS), t.iter().position(|&x| x == NAL_IDR));
            assert!(matches!((s, p, idr), (Some(s), Some(p), Some(idr)) if s < idr && p < idr), "unit {i}: {t:?}");
        } else {
            assert!(!t.contains(&NAL_SPS) && !t.contains(&NAL_IDR), "unit {i}: {t:?}");
        }
    }
    let plid = sps(&units[0]);
    assert_eq!(plid.profile(), RtcProfile::ConstrainedBaseline, "{plid}");
    assert!(ProfileLevelId::CONSTRAINED_BASELINE_3_1.accepts(plid), "{plid} goes out on a 42e01f session");
    let pts: Vec<Duration> = units.iter().map(|u| u.pts).collect();
    assert!(pts.windows(2).all(|w| w[1] > w[0]), "{pts:?}");
    assert_eq!(pts[3], Duration::from_secs(1) / 30 * 3, "the submitted timestamps come back");
    let mut decoder = Decoder::new();
    for (i, u) in units.iter().enumerate() {
        assert_eq!(decoder.decode(&u.data).unwrap_or_else(|e| panic!("unit {i}: {e}")), (320, 240), "unit {i}");
    }
    assert_eq!((encoder.output_size(), encoder.sessions()), (Some((320, 240)), 1));
    eprintln!("hardware: {:?}", encoder.hardware());
}

#[test]
fn main_profile_rgba_input_scaled_with_periodic_keyframes() {
    // 30 fps with an IDR at most every 200 ms: one every 6 frames.
    let config = EncoderConfig {
        profile: Profile::Main,
        keyframe_interval: Duration::from_millis(200),
        max_size: Some(320),
        ..EncoderConfig::default()
    };
    let Some((units, encoder)) = encode(config, (480, 640), PixelFormat::Rgba, 13, &[]) else { return };
    let keys: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys, vec![0, 6, 12]);
    assert!(units.iter().filter(|u| u.keyframe).all(|u| types(u).starts_with(&[NAL_SPS, NAL_PPS])));
    assert_eq!(sps(&units[0]).profile(), RtcProfile::Main);
    assert_eq!(encoder.output_size(), Some((240, 320)), "the long edge scaled to 320");
    let mut decoder = Decoder::new();
    for u in &units {
        assert_eq!(decoder.decode(&u.data).expect("decode"), (240, 320));
    }
}

#[test]
fn a_new_input_size_makes_a_new_session_that_starts_with_an_idr() {
    let Ok(mut encoder) = VtEncoder::new(EncoderConfig::default()) else { return };
    let (a, b) =
        (PixelBufferPool::new(320, 240, PixelFormat::Bgra).expect("a"), PixelBufferPool::new(240, 320, PixelFormat::Bgra).expect("b"));
    let mut units = Vec::new();
    for i in 0..6 {
        let pool = if i < 3 { &a } else { &b };
        match encoder.encode(&picture(pool, i), Duration::from_millis(u64::from(i) * 33), false) {
            Ok(out) => units.extend(out),
            Err(Error::Unsupported(m)) => return eprintln!("SKIP: {m}"),
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(units.iter().map(|u| u.keyframe).collect::<Vec<_>>(), vec![true, false, false, true, false, false]);
    assert_eq!((encoder.sessions(), encoder.output_size(), encoder.parameter_sets().changes()), (2, Some((240, 320)), 2));
    let mut decoder = Decoder::new();
    let sizes: Vec<(u32, u32)> = units.iter().map(|u| decoder.decode(&u.data).expect("decode")).collect();
    assert_eq!(sizes, vec![(320, 240), (320, 240), (320, 240), (240, 320), (240, 320), (240, 320)]);
}

#[test]
fn bitrate_retargets_and_odd_sizes_work() {
    // An iPhone's framebuffer is 1179 pixels wide: odd.
    let Some((units, mut encoder)) = encode(EncoderConfig::default(), (1179, 64), PixelFormat::Bgra, 2, &[]) else { return };
    assert!(encoder.set_bitrate(1_000_000).is_ok() && encoder.set_bitrate(50_000_000).is_ok());
    assert_eq!(encoder.output_size(), Some((1178, 64)), "rounded down to even");
    let mut decoder = Decoder::new();
    assert_eq!(decoder.decode(&units[0].data).expect("decode"), (1178, 64));
    assert!(Decoder::new().decode(&units[1].data).is_err(), "a P frame with no SPS/PPS before it");
}
