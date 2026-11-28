//! OpenH264 on Linux end to end on synthetic screens (no device): profiles and their
//! `profile-level-id`, forced and periodic IDRs with their SPS/PPS, timestamps, scaling, both pixel
//! orders, runtime bitrate changes, a rotation, the encoder skipping a picture, and the whole loop
//! on a fake source and clock; every access unit is decoded back.
#![cfg(all(target_os = "linux", feature = "openh264"))]

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use sim_rtc::{Profile as RtcProfile, ProfileLevelId};
use simee_encode::annexb::{self, NAL_IDR, NAL_PPS, NAL_SPS};
use simee_encode::pacing::Pacing;
use simee_encode::stream::{self, Clock, Controls, Flow, Source};
use simee_encode::{
    AccessUnit, Converter, Decoder, Encoder, EncoderConfig, Error, Hardware, OpenH264Encoder, Picture, PixelOrder, Profile,
};

#[allow(dead_code)]
#[path = "../examples/synthetic/mod.rs"]
mod synthetic;
use synthetic::Content;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn drawer(width: u32, height: u32, step: u32, order: PixelOrder) -> Picture {
    synthetic::picture(Content::Drawer, width, height, step * 2, order)
}

/// Encodes `n` frames at 30 fps, forcing an IDR at the indices in `force`.
fn encode(config: EncoderConfig, size: (u32, u32), n: u32, force: &[u32]) -> (Vec<AccessUnit>, OpenH264Encoder) {
    let mut encoder = OpenH264Encoder::new(config).expect("config");
    let mut units = Vec::new();
    for i in 0..n {
        let picture = drawer(size.0, size.1, i, PixelOrder::Bgra);
        units.extend(encoder.encode(&picture, ms(33) * i, force.contains(&i)).unwrap_or_else(|e| panic!("frame {i}: {e}")));
    }
    (units, encoder)
}

fn types(unit: &AccessUnit) -> Vec<u8> {
    annexb::annex_b_units(&unit.data).iter().map(|n| annexb::nal_type(n)).collect()
}

fn profile_level(unit: &AccessUnit) -> ProfileLevelId {
    let units = annexb::annex_b_units(&unit.data);
    let sps = units.iter().find(|n| annexb::nal_type(n) == NAL_SPS).expect("an SPS");
    ProfileLevelId::from_sps(sps).expect("profile-level-id")
}

/// Every unit decodes, in order, to a picture of `size`.
fn assert_decodes(units: &[AccessUnit], size: (u32, u32)) {
    let mut decoder = Decoder::new().expect("decoder");
    for (i, unit) in units.iter().enumerate() {
        assert_eq!(decoder.decode(&unit.data).unwrap_or_else(|e| panic!("unit {i}: {e}")), size, "unit {i}");
    }
}

fn config() -> EncoderConfig {
    EncoderConfig { keyframe_interval: Duration::from_secs(60), ..EncoderConfig::default() }
}

#[test]
fn constrained_baseline_with_forced_keyframes_decodes() {
    let (units, encoder) = encode(config(), (320, 640), 14, &[7]);
    assert_eq!(units.len(), 14, "every picture comes out as one access unit");
    let keys: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys, vec![0, 7], "the first picture and the requested one, no others");
    for &k in &keys {
        // SPS and PPS in front of the IDR, and nothing else before them.
        assert_eq!(&types(&units[k])[..3], &[NAL_SPS, NAL_PPS, NAL_IDR], "unit {k}");
    }
    assert!(
        units.iter().filter(|u| !u.keyframe).all(|u| !types(u).iter().any(|t| [NAL_SPS, NAL_PPS, NAL_IDR].contains(t))),
        "P frames carry no parameter sets"
    );
    let id = profile_level(&units[0]);
    assert_eq!(id.profile(), RtcProfile::ConstrainedBaseline, "{id:?}");
    assert!(units.windows(2).all(|w| w[1].pts > w[0].pts));
    assert_eq!((encoder.output_size(), encoder.sessions(), encoder.hardware()), (Some((320, 640)), 1, Some(false)));
    assert!(encoder.parameter_sets().sps().is_some() && encoder.parameter_sets().pps().is_some());
    assert_decodes(&units, (320, 640));
}

#[test]
fn main_profile_decodes() {
    let (units, _) = encode(EncoderConfig { profile: Profile::Main, ..config() }, (320, 640), 6, &[]);
    assert_eq!(profile_level(&units[0]).profile_idc, 0x4d);
    assert_eq!(profile_level(&units[0]).profile(), RtcProfile::Main);
    assert_decodes(&units, (320, 640));
}

#[test]
fn the_keyframe_interval_makes_periodic_idrs_with_their_parameter_sets() {
    // 10 fps, an IDR at least every half second: every fifth picture.
    let config = EncoderConfig { fps: 10, keyframe_interval: ms(500), ..EncoderConfig::default() };
    assert_eq!(config.keyframe_frames(), 5);
    let mut encoder = OpenH264Encoder::new(config).expect("config");
    let units: Vec<AccessUnit> =
        (0..16).flat_map(|i| encoder.encode(&drawer(192, 384, i, PixelOrder::Rgba), ms(100) * i, false).expect("encode")).collect();
    let keys: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys, vec![0, 5, 10, 15]);
    assert!(keys.iter().all(|&k| types(&units[k])[..2] == [NAL_SPS, NAL_PPS]));
    assert_decodes(&units, (192, 384));
}

#[test]
fn output_size_follows_max_size_with_even_sides() {
    // 360x800 capped to 400 on the long edge: 180x400; an odd source side rounds down to even.
    let (units, encoder) = encode(EncoderConfig { max_size: Some(400), ..config() }, (360, 800), 3, &[]);
    assert_eq!(encoder.output_size(), Some((180, 400)));
    assert_decodes(&units, (180, 400));
    let (units, encoder) = encode(config(), (321, 641), 2, &[]);
    assert_eq!(encoder.output_size(), Some((320, 640)));
    assert_decodes(&units, (320, 640));
}

#[test]
fn a_new_input_size_makes_a_new_session_that_starts_with_an_idr() {
    let mut encoder = OpenH264Encoder::new(config()).expect("config");
    let mut units = Vec::new();
    for i in 0..4 {
        units.extend(encoder.encode(&drawer(192, 384, i, PixelOrder::Rgba), ms(33) * i, false).expect("encode"));
    }
    // The device turned: the same pixels as a wide frame.
    for i in 4..8 {
        units.extend(encoder.encode(&drawer(384, 192, i, PixelOrder::Rgba), ms(33) * i, false).expect("encode"));
    }
    assert_eq!(encoder.sessions(), 2);
    assert_eq!(units.iter().map(|u| u.keyframe).collect::<Vec<_>>(), [true, false, false, false, true, false, false, false]);
    assert_eq!(&types(&units[4])[..3], &[NAL_SPS, NAL_PPS, NAL_IDR]);
    assert_decodes(&units[..4], (192, 384));
    assert_decodes(&units[4..], (384, 192));
    assert_eq!(encoder.parameter_sets().changes(), 2, "the new size's SPS replaced the first");
}

#[test]
fn bgra_and_rgba_pictures_of_the_same_screen_encode_the_same() {
    let mut a = OpenH264Encoder::new(config()).expect("config");
    let mut b = OpenH264Encoder::new(config()).expect("config");
    for i in 0..4 {
        let from_bgra = a.encode(&drawer(192, 384, i, PixelOrder::Bgra), ms(33) * i, false).expect("encode");
        let from_rgba = b.encode(&drawer(192, 384, i, PixelOrder::Rgba), ms(33) * i, false).expect("encode");
        assert_eq!(
            from_bgra.iter().map(|u| &u.data).collect::<Vec<_>>(),
            from_rgba.iter().map(|u| &u.data).collect::<Vec<_>>(),
            "picture {i}"
        );
    }
}

#[test]
fn colours_come_out_as_bt709_limited_range() {
    // A flat red screen: Y 63, Cb 102, Cr 240 in BT.709 limited range, give or take what lossy
    // coding of a flat area costs.
    let red = Picture::new(64, 64, PixelOrder::Rgba, [255, 0, 0, 255].repeat(64 * 64));
    let mut encoder = OpenH264Encoder::new(config()).expect("config");
    let unit = encoder.encode(&red, Duration::ZERO, true).expect("encode").remove(0);
    let mut decoder = Decoder::new().expect("decoder");
    let planes = decoder.decode_planes(&unit.data).expect("decode");
    let mean = |p: &[u8]| p.iter().map(|&v| u32::from(v)).sum::<u32>() / p.len() as u32;
    let (y, u, v) = (mean(&planes.y), mean(&planes.u), mean(&planes.v));
    assert!(y.abs_diff(63) <= 3 && u.abs_diff(102) <= 3 && v.abs_diff(240) <= 3, "y {y} u {u} v {v}");
}

#[test]
fn the_bitrate_follows_set_bitrate_in_both_directions() {
    // Bytes per picture over `n` pictures, after `skip` for the encoder to settle.
    fn per_picture(encoder: &mut OpenH264Encoder, from: u32, n: u32, skip: u32) -> f64 {
        let mut bytes = 0;
        for i in from..from + n {
            let units =
                encoder.encode(&synthetic::picture(Content::Noise, 160, 320, i, PixelOrder::Rgba), ms(33) * i, false).expect("encode");
            if i >= from + skip {
                bytes += units.iter().map(|u| u.data.len()).sum::<usize>();
            }
        }
        bytes as f64 / f64::from(n - skip)
    }
    let config = EncoderConfig { bitrate_bps: 200_000, max_bitrate_bps: Some(6_000_000), ..config() };
    // Noise, so the encoder always has more to say than the target lets it.
    let mut encoder = OpenH264Encoder::new(config).expect("config");
    let low = per_picture(&mut encoder, 0, 60, 20);
    encoder.set_bitrate(3_000_000).expect("set");
    assert_eq!(encoder.bitrate(), 3_000_000);
    let high = per_picture(&mut encoder, 60, 90, 30);
    encoder.set_bitrate(200_000).expect("set");
    let low_again = per_picture(&mut encoder, 150, 90, 30);
    eprintln!("bytes per picture: {low:.0} at 200 kbit/s, {high:.0} at 3 Mbit/s, {low_again:.0} at 200 kbit/s again");
    assert!(high > low * 3.0, "raising the target raises the output: {low:.0} then {high:.0}");
    assert!(low_again < high / 3.0, "and lowering it lowers it: {high:.0} then {low_again:.0}");
    // A request over the ceiling is held to it; one under 100 kbit/s is raised to that.
    encoder.set_bitrate(50_000_000).expect("set");
    assert_eq!(encoder.bitrate(), 6_000_000);
    encoder.set_bitrate(1).expect("set");
    assert_eq!(encoder.bitrate(), 100_000);
}

#[test]
fn without_a_ceiling_a_retarget_is_not_held_to_the_starting_target() {
    // `max_bitrate_bps: None`: nothing caps a retarget (OpenH264 itself isn't told the target is
    // its maximum), and an absurd request is held to what the encoder accepts rather than
    // failing this and every later picture.
    let config = EncoderConfig { bitrate_bps: 200_000, max_bitrate_bps: None, ..config() };
    let mut encoder = OpenH264Encoder::new(config).expect("config");
    let bytes = |encoder: &mut OpenH264Encoder, from: u32, n: u32| -> usize {
        (from..from + n)
            .map(|i| {
                let picture = synthetic::picture(Content::Noise, 160, 320, i, PixelOrder::Rgba);
                encoder.encode(&picture, ms(33) * i, false).expect("encode").iter().map(|u| u.data.len()).sum::<usize>()
            })
            .sum()
    };
    bytes(&mut encoder, 0, 30);
    let low = bytes(&mut encoder, 30, 60);
    encoder.set_bitrate(3_000_000).expect("set");
    bytes(&mut encoder, 90, 30);
    let high = bytes(&mut encoder, 120, 60);
    assert!(high > low * 3, "{low} bytes at 200 kbit/s, then {high} at 3 Mbit/s");
    for absurd in [u64::from(u32::MAX) + 1, u64::MAX] {
        encoder.set_bitrate(absurd).expect("set");
        assert_eq!(encoder.bitrate(), 240_000_000, "held to what OpenH264 accepts");
        bytes(&mut encoder, 180, 3);
    }
    // Retargets don't change the level the stream was announced at: a publisher negotiated it.
    let before = encoder.parameter_sets().sps().map(<[u8]>::to_vec);
    let idr = (200..220).find_map(|i| {
        let picture = synthetic::picture(Content::Noise, 160, 320, i, PixelOrder::Rgba);
        encoder.encode(&picture, ms(33) * i, true).expect("encode").into_iter().find(|u| u.keyframe)
    });
    assert!(idr.is_some(), "a requested IDR came out");
    assert_eq!(encoder.parameter_sets().sps().map(<[u8]>::to_vec), before, "the SPS is the same after every retarget");
}

#[test]
fn a_picture_the_encoder_skips_comes_back_as_no_access_unit() {
    // Noise can't fit 100 kbit/s: after the first IDR the rate control skips pictures. Every unit
    // that does come out still decodes, and forced IDRs eventually come out (the loop asks again
    // after a skip).
    let config = EncoderConfig { bitrate_bps: 100_000, max_bitrate_bps: Some(100_000), ..config() };
    let mut encoder = OpenH264Encoder::new(config).expect("config");
    let mut units = Vec::new();
    let mut skipped = 0;
    let mut forced_until_out = 0;
    for i in 0..40 {
        let picture = synthetic::picture(Content::Noise, 128, 256, i, PixelOrder::Rgba);
        let force = i == 10 || (i > 10 && forced_until_out > 0);
        let out = encoder.encode(&picture, ms(33) * i, force).expect("encode");
        skipped += usize::from(out.is_empty());
        if force {
            forced_until_out = if out.iter().any(|u| u.keyframe) { 0 } else { forced_until_out + 1 };
            assert!(forced_until_out < 20, "a requested IDR never came out");
        }
        units.extend(out);
    }
    assert!(skipped > 0, "the rate control skipped nothing at 100 kbit/s of noise");
    assert!(units.iter().filter(|u| u.keyframe).count() >= 2, "the first IDR and the requested one");
    assert_decodes(&units, (128, 256));
}

#[test]
fn bad_configurations_and_pictures_are_refused() {
    assert!(matches!(OpenH264Encoder::new(EncoderConfig { fps: 0, ..config() }), Err(Error::InvalidInput(_))));
    match OpenH264Encoder::new(EncoderConfig { hardware: Hardware::Require, ..config() }) {
        Err(Error::Unsupported(m)) => assert!(m.contains("software"), "{m}"),
        other => panic!("{:?}", other.err()),
    }
    let mut encoder = OpenH264Encoder::new(config()).expect("config");
    // OpenH264 stops at level 5.2's 3840x2160: a bigger picture needs max_size.
    let big = Picture::new(4096, 2304, PixelOrder::Rgba, vec![0; 4096 * 2304 * 4]);
    match encoder.encode(&big, Duration::ZERO, true) {
        Err(Error::Unsupported(m)) => assert!(m.contains("max_size"), "{m}"),
        other => panic!("{:?}", other.err()),
    }
    let mut capped = OpenH264Encoder::new(EncoderConfig { max_size: Some(1920), ..config() }).expect("config");
    assert!(capped.encode(&big, Duration::ZERO, true).is_ok(), "max_size brings it under the limit");
    let short = Picture::new(64, 64, PixelOrder::Rgba, vec![0; 100]);
    assert!(matches!(encoder.encode(&short, Duration::ZERO, true), Err(Error::InvalidInput(_))));
}

struct FakeClock(Rc<Cell<Instant>>);

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.0.get()
    }
}

/// Frames at the given offsets, on made-up time that jumps to each frame (or the deadline) as the
/// loop waits; the loop is stopped once time reaches `end`.
struct FakeSource {
    now: Rc<Cell<Instant>>,
    t0: Instant,
    frames: Vec<Duration>,
    next: usize,
    end: Duration,
    newest: Option<usize>,
    pictures: Vec<Picture>,
    controls: Rc<Controls>,
}

impl Source for FakeSource {
    type Picture = Picture;

    fn wait(&mut self, deadline: Instant) -> simee_encode::Result<Option<Instant>> {
        let end = self.t0 + self.end;
        match self.frames.get(self.next).map(|&d| self.t0 + d) {
            Some(at) if at <= deadline => {
                self.now.set(self.now.get().max(at));
                self.newest = Some(self.next);
                self.next += 1;
                Ok(Some(at))
            }
            _ => {
                self.now.set(deadline.min(end).max(self.now.get()));
                if self.now.get() >= end {
                    self.controls.stop();
                }
                Ok(None)
            }
        }
    }

    fn picture(&mut self) -> simee_encode::Result<Option<&Picture>> {
        Ok(self.newest.map(|i| &self.pictures[i % self.pictures.len()]))
    }
}

#[test]
fn the_loop_paces_requests_keyframes_and_retargets_with_the_real_encoder() {
    let t0 = Instant::now();
    let now = Rc::new(Cell::new(t0));
    let controls = Rc::new(Controls::default());
    let frames: Vec<Duration> = (0..30).map(|i| ms(i * 50)).collect();
    let pictures = (0..30).map(|i| drawer(192, 384, i, PixelOrder::Rgba)).collect();
    let mut source =
        FakeSource { now: now.clone(), t0, frames, next: 0, end: ms(1600), newest: None, pictures, controls: controls.clone() };
    let mut encoder = OpenH264Encoder::new(EncoderConfig { max_bitrate_bps: Some(4_000_000), ..config() }).expect("config");
    let mut units = Vec::new();
    let stats = stream::run_with_clock(&mut source, &mut encoder, Pacing::new(30, None), &controls, &FakeClock(now), |u| {
        if u.pts == ms(300) {
            controls.request_keyframe();
            controls.set_bitrate(1_500_000);
        }
        units.push(u);
        Ok(Flow::Continue)
    })
    .expect("run");
    assert_eq!(stats.encoded, units.len() as u64);
    assert!(units[0].keyframe && types(&units[0])[..2] == [NAL_SPS, NAL_PPS]);
    // The request is answered by the very next picture, an IDR with its parameter sets.
    let at = units.iter().position(|u| u.pts == ms(300)).expect("the unit that asked");
    assert!(
        units[at + 1].keyframe && types(&units[at + 1])[..3] == [NAL_SPS, NAL_PPS, NAL_IDR],
        "{:?}",
        units.iter().map(|u| (u.pts, u.keyframe)).collect::<Vec<_>>()
    );
    assert_eq!(stats.keyframes, 2, "only the first picture and the requested one");
    assert_eq!(encoder.bitrate(), 1_500_000);
    let pts: Vec<Duration> = units.iter().map(|u| u.pts).collect();
    assert!(pts.windows(2).all(|w| w[1] > w[0]), "strictly increasing: {pts:?}");
    // Spaced by the 30 fps cap, except for the IDR that re-encodes the last picture at once.
    assert!(pts.windows(2).enumerate().all(|(i, w)| i == at || w[1] - w[0] >= Duration::from_micros(24_000)), "{pts:?}");
    assert_decodes(&units, (192, 384));
}

/// Luma PSNR in dB between two planes of the same size (99 when identical).
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a.iter().zip(b).map(|(&x, &y)| f64::from(x.abs_diff(y)).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

#[test]
fn a_scrolling_screen_round_trips_at_30_and_60_fps_above_the_psnr_floor() {
    // A drawer dragged up 3 pixels a frame, in the order emu's frames come in (RGBA), encoded at
    // the frame rate asked for and decoded again: every picture that came out must be close to
    // what the encoder was given. No timing: the pictures carry the timestamps of their rate.
    let size = (216, 480);
    for fps in [30u32, 60] {
        let config = EncoderConfig { fps, keyframe_interval: Duration::from_secs(60), ..EncoderConfig::default() };
        let mut encoder = OpenH264Encoder::new(config).expect("config");
        let mut decoder = Decoder::new().expect("decoder");
        let mut converter = Converter::new();
        let (mut worst, mut decoded, mut keyframes) = (f64::MAX, 0, 0);
        for i in 0..40u32 {
            let picture = synthetic::picture(Content::Drawer, size.0, size.1, i * 3, PixelOrder::Rgba);
            let pts = Duration::from_micros(u64::from(i) * 1_000_000 / u64::from(fps));
            let units = encoder.encode(&picture, pts, false).unwrap_or_else(|e| panic!("{fps} fps, frame {i}: {e}"));
            // At most one access unit per picture: the encoder may skip one to hold its rate.
            assert!(units.len() <= 1, "{fps} fps, frame {i}: {} units", units.len());
            let Some(unit) = units.first() else { continue };
            keyframes += usize::from(unit.keyframe);
            let planes = decoder.decode_planes(&unit.data).unwrap_or_else(|e| panic!("{fps} fps, frame {i}: {e}"));
            assert_eq!((planes.width, planes.height), size);
            let source = converter.convert(&picture.raster(), size).expect("convert");
            worst = worst.min(psnr(source.y(), &planes.y));
            decoded += 1;
        }
        assert_eq!(keyframes, 1, "{fps} fps: only the first picture is an IDR");
        assert!(decoded >= 30, "{fps} fps: {decoded} of 40 pictures came out");
        assert!(worst > 35.0, "{fps} fps: the worst picture has {worst:.1} dB of luma PSNR");
    }
}
