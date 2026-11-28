//! Publishes a live device's screen over WHIP: frames from the device, H.264 from VideoToolbox,
//! WebRTC from `simee-rtc`. The publisher's keyframe requests (PLI/FIR) force IDRs and its
//! bandwidth estimates retarget the encoder. With a local MediaMTX (`mediamtx` on PATH, its
//! default config), watch it at `http://127.0.0.1:8889/<path>`.
//!
//! ```text
//! cargo run --release -p simee-encode --example publish_device -- --udid <booted udid> \
//!   [--whip http://127.0.0.1:8889/simee/whip] [--fps 30] [--max-size 1280] [--seconds N]
//! cargo run --release -p simee-encode --example publish_device -- --avd <running avd> [...]
//! ```
//!
//! The session is offered the first keyframe's own `profile-level-id` (its level follows the
//! frame size, which the default `42e01f` would refuse for a phone-sized picture).

#[cfg(target_os = "macos")]
#[path = "common/mod.rs"]
mod common;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = mac::run() {
        eprintln!("publish_device: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("publish_device: VideoToolbox only (macOS)");
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use sim_coresim::videotoolbox::PixelBuffer;
    use sim_rtc::{ProfileLevelId, Publisher, PublisherConfig, PublisherEvent, Sent};
    use simee_encode::annexb::{self, NAL_SPS};
    use simee_encode::pacing::Pacing;
    use simee_encode::stream::{self, Controls, Flow, Source};
    use simee_encode::{AndroidSource, EncoderConfig, Error, IosSource, VtEncoder};

    use crate::common::{Device, number, value};

    pub fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let device = Device::from_args(&args)?;
        let whip = value(&args, "--whip").unwrap_or_else(|| "http://127.0.0.1:8889/simee/whip".into());
        let fps: u32 = number(&args, "--fps", 30)?;
        let seconds: f64 = number(&args, "--seconds", 0.0)?;
        let config = EncoderConfig {
            fps,
            max_size: Some(number(&args, "--max-size", 1280)?),
            bitrate_bps: number(&args, "--bitrate", 2_500_000)?,
            ..EncoderConfig::default()
        };
        match device {
            Device::Ios { udid } => {
                let context = sim_coresim::Context::new().map_err(|e| e.to_string())?;
                let display = context.device(&udid).and_then(|d| d.display()).map_err(|e| e.to_string())?;
                publish(IosSource::new(display).map_err(|e| e.to_string())?, config, &whip, seconds)
            }
            Device::Android { dir, .. } => publish(AndroidSource::connect(&dir, 0).map_err(|e| e.to_string())?, config, &whip, seconds),
        }
    }

    fn publish<S: Source<Picture = PixelBuffer>>(mut source: S, config: EncoderConfig, whip: &str, seconds: f64) -> Result<(), String> {
        let mut encoder = VtEncoder::new(config.clone()).map_err(|e| e.to_string())?;
        let controls = Arc::new(Controls::default());
        let mut publisher: Option<Publisher> = None;
        let started = Instant::now();
        let (mut sent, mut waiting) = (0u64, 0u64);
        let pacing = Pacing::new(config.fps, Some(Duration::from_millis(500)));
        let result = stream::run(&mut source, &mut encoder, pacing, &controls, |unit| {
            if publisher.is_none() {
                // Connect on the first access unit, an IDR, offering its SPS's profile and level.
                let units = annexb::annex_b_units(&unit.data);
                let sps = units.iter().find(|n| annexb::nal_type(n) == NAL_SPS).copied();
                let profile =
                    sps.and_then(ProfileLevelId::from_sps).ok_or_else(|| Error::Encoder("the first access unit has no SPS".into()))?;
                let mut rtc = PublisherConfig::new(whip);
                rtc.profile = profile;
                rtc.start_bitrate_bps = Some(u64::from(config.bitrate_bps));
                rtc.max_bitrate_bps = u64::from(config.max_bitrate_bps.unwrap_or(config.bitrate_bps));
                let c = controls.clone();
                let p = Publisher::connect(rtc, move |event| match event {
                    PublisherEvent::KeyframeRequested(_) => c.request_keyframe(),
                    PublisherEvent::BitrateEstimate { bps, .. } => c.set_bitrate(bps),
                    PublisherEvent::Disconnected => c.stop(),
                    PublisherEvent::Connected => {}
                })
                .map_err(|e| Error::Encoder(format!("WHIP {whip}: {e}")))?;
                eprintln!("publishing {profile} ({}) to {}", profile.profile(), p.resource());
                publisher = Some(p);
            }
            let p = publisher.as_mut().ok_or_else(|| Error::Encoder("no publisher".into()))?;
            match p.send(&unit.data, unit.pts).map_err(|e| Error::Encoder(e.to_string()))? {
                Sent::Queued { .. } => sent += 1,
                Sent::WaitingForKeyframe => waiting += 1,
            }
            Ok(if seconds > 0.0 && started.elapsed().as_secs_f64() >= seconds { Flow::Stop } else { Flow::Continue })
        });
        eprintln!("sent {sent} access units ({waiting} before the first keyframe), hardware encoder: {:?}", encoder.hardware());
        if let Some(p) = publisher {
            p.close().map_err(|e| e.to_string())?;
        }
        result.map(|_| ()).map_err(|e| e.to_string())
    }
}
