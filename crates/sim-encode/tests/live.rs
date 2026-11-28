//! Smoke tests on a real device's frames: encode a few seconds, check every access unit decodes
//! (VTDecompressionSession) at the encoder's size, the SPS parses with the publisher's parser and
//! is the asked-for profile, a requested keyframe arrives with its SPS/PPS, and timestamps
//! increase. Each prints SKIP without its device:
//!
//! ```text
//! SIMEE_E2E_UDID=<booted udid> cargo test -p simee-encode --test live -- --nocapture
//! SIMEE_E2E_AVD=<running avd> cargo test -p simee-encode --test live -- --nocapture
//! ```
#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use sim_coresim::videotoolbox::PixelBuffer;
use sim_rtc::{Profile as RtcProfile, ProfileLevelId};
use simee_encode::annexb::{self, NAL_SPS};
use simee_encode::pacing::Pacing;
use simee_encode::stream::{self, AccessUnit, Controls, Flow, Source};
use simee_encode::{AndroidSource, Decoder, EncoderConfig, IosSource, VtEncoder};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Two seconds at 30 fps with a heartbeat, so a still screen still yields frames; a keyframe is
/// asked for after the fifth access unit.
fn encode<S: Source<Picture = PixelBuffer>>(source: &mut S) -> (Vec<AccessUnit>, VtEncoder) {
    let config = EncoderConfig { max_size: Some(1280), keyframe_interval: Duration::from_secs(30), ..EncoderConfig::default() };
    let mut encoder = VtEncoder::new(config).expect("config");
    let controls = Arc::new(Controls::default());
    // A device that sends nothing at all ends the run too (and fails the checks).
    let watchdog = controls.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        watchdog.stop();
    });
    let mut units = Vec::new();
    let start = Instant::now();
    stream::run(source, &mut encoder, Pacing::new(30, Some(Duration::from_millis(100))), &controls, |u| {
        units.push(u);
        if units.len() == 5 {
            controls.request_keyframe();
        }
        Ok(if start.elapsed() > Duration::from_secs(2) { Flow::Stop } else { Flow::Continue })
    })
    .expect("run");
    (units, encoder)
}

fn check(units: &[AccessUnit], encoder: &VtEncoder) {
    assert!(units.len() >= 10, "{} access units in 2 s", units.len());
    let (w, h) = encoder.output_size().expect("a session");
    assert!(w.max(h) <= 1280 && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
    let keys: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys.first(), Some(&0), "starts with an IDR");
    assert!(keys.iter().any(|&k| (5..=7).contains(&k)), "the requested IDR came within two frames: {keys:?}");
    assert!(units.windows(2).all(|p| p[1].pts > p[0].pts), "timestamps increase");
    let nal = annexb::annex_b_units(&units[0].data);
    let sps = nal.iter().find(|n| annexb::nal_type(n) == NAL_SPS).expect("SPS");
    let plid = ProfileLevelId::from_sps(sps).expect("profile-level-id");
    assert_eq!(plid.profile(), RtcProfile::ConstrainedBaseline, "{plid}");
    let mut decoder = Decoder::new();
    for (i, u) in units.iter().enumerate() {
        assert_eq!(decoder.decode(&u.data).unwrap_or_else(|e| panic!("access unit {i}: {e}")), (w, h), "access unit {i}");
    }
    let mut ms: Vec<f64> = units.iter().map(|u| u.encode_time.as_secs_f64() * 1e3).collect();
    ms.sort_by(f64::total_cmp);
    eprintln!(
        "{} access units at {w}x{h} ({plid}), hardware {:?}, encode median {:.2} ms, first IDR {} bytes",
        units.len(),
        encoder.hardware(),
        ms[ms.len() / 2],
        units[0].data.len()
    );
}

#[test]
fn an_ios_device_encodes_and_decodes() {
    let Some(udid) = env("SIMEE_E2E_UDID") else { return eprintln!("SKIP: set SIMEE_E2E_UDID to a booted device") };
    let context = sim_coresim::Context::new().expect("context");
    let display = context.device(&udid).and_then(|d| d.display()).expect("display");
    let mut source = IosSource::new(display).expect("source");
    let (units, encoder) = encode(&mut source);
    check(&units, &encoder);
}

#[test]
fn an_android_device_encodes_and_decodes() {
    let Some(name) = env("SIMEE_E2E_AVD") else { return eprintln!("SKIP: set SIMEE_E2E_AVD to a running AVD") };
    let dir = emu_avd::Env::from_env().load(&name).expect("avd").dir;
    let mut source = AndroidSource::connect(&dir, 0).expect("display socket");
    let (units, encoder) = encode(&mut source);
    check(&units, &encoder);
}
