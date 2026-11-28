//! Encodes a live device for a while and prints one JSON line: frames, per-frame encode latency
//! (median, p95), the frame copy (Android), CPU, output bitrate and keyframe sizes. The screen is
//! kept moving (home screen pages on iOS, the app drawer on Android) unless `--still`.
//! scripts/bench/encode.sh runs it per device and rate.
//!
//! ```text
//! cargo run --release -p simee-encode --example encode_bench -- --udid <booted udid> --fps 60
//! cargo run --release -p simee-encode --example encode_bench -- --avd <running avd> --fps 30
//!   [--seconds 10] [--max-size N] [--max-width N (Android: emu's downscale)] [--profile main]
//!   [--bitrate BPS] [--heartbeat MS] [--still] [--out stream.h264]
//! ```
//!
//! CPU is this process's (the capture, the copy, the encoder's submission and output) and, apart,
//! the host's `VTEncoderXPCService` processes' (where VideoToolbox runs the hardware encoder; any
//! other client's encoding there counts too).

#[cfg(target_os = "macos")]
#[path = "common/mod.rs"]
mod common;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = run() {
        eprintln!("encode_bench: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("encode_bench: VideoToolbox only (macOS)");
}

#[cfg(target_os = "macos")]
use mac::run;

#[cfg(target_os = "macos")]
mod mac {
    use std::io::Write;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use sim_coresim::procs;
    use sim_coresim::videotoolbox::PixelBuffer;
    use simee_encode::pacing::Pacing;
    use simee_encode::stream::{self, Controls, Flow, Source};
    use simee_encode::{AndroidSource, EncoderConfig, IosSource, Profile, VtEncoder};

    use crate::common::{Device, Motion, flag, median_p95, number, value};

    /// Times each `picture` call: the surface wrap (iOS) or the copy into the pool (Android).
    struct Timed<S> {
        inner: S,
        picture_ms: Vec<f64>,
    }

    impl<S: Source<Picture = PixelBuffer>> Source for Timed<S> {
        type Picture = PixelBuffer;

        fn wait(&mut self, deadline: Instant) -> simee_encode::Result<Option<Instant>> {
            self.inner.wait(deadline)
        }

        fn picture(&mut self) -> simee_encode::Result<Option<&PixelBuffer>> {
            let start = Instant::now();
            let p = self.inner.picture()?;
            self.picture_ms.push(start.elapsed().as_secs_f64() * 1e3);
            Ok(p)
        }
    }

    fn xpc_cpu_ns() -> u64 {
        // `p_comm` holds the first 15 bytes of the name.
        procs::list().iter().filter(|p| p.name().starts_with("VTEncoderXPC")).filter_map(|p| procs::usage(p.pid)).map(|u| u.cpu_ns).sum()
    }

    fn own_cpu_ns() -> u64 {
        procs::usage(std::process::id() as i32).map_or(0, |u| u.cpu_ns)
    }

    pub fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let device = Device::from_args(&args)?;
        let fps: u32 = number(&args, "--fps", 30)?;
        let seconds: f64 = number(&args, "--seconds", 10.0)?;
        let heartbeat: u64 = number(&args, "--heartbeat", 500)?;
        let config = EncoderConfig {
            fps,
            bitrate_bps: number(&args, "--bitrate", EncoderConfig::default().bitrate_bps)?,
            max_size: value(&args, "--max-size").map(|v| v.parse().map_err(|_| format!("--max-size {v}"))).transpose()?,
            profile: if value(&args, "--profile").as_deref() == Some("main") { Profile::Main } else { Profile::ConstrainedBaseline },
            ..EncoderConfig::default()
        };
        let pacing = Pacing::new(fps, (heartbeat > 0).then(|| Duration::from_millis(heartbeat)));
        let still = flag(&args, "--still");
        let out = value(&args, "--out");
        let label = device.label();
        match device {
            Device::Ios { udid } => {
                let context = sim_coresim::Context::new().map_err(|e| e.to_string())?;
                let display = context.device(&udid).and_then(|d| d.display()).map_err(|e| e.to_string())?;
                let source = IosSource::new(display).map_err(|e| e.to_string())?;
                let _motion = (!still).then(|| Motion::ios(&udid, Duration::from_millis(700)));
                bench(&label, source, config, pacing, seconds, out)
            }
            Device::Android { dir, .. } => {
                let mut source = AndroidSource::connect(&dir, number(&args, "--max-width", 0)?).map_err(|e| e.to_string())?;
                source.wait(Instant::now() + Duration::from_secs(10)).map_err(|e| e.to_string())?;
                let size = source.device_size().ok_or("no frame from the device within 10 s")?;
                let link = source.input_link().map_err(|e| e.to_string())?;
                let _motion = (!still).then(|| Motion::android(link, size, Duration::from_millis(700)));
                bench(&label, source, config, pacing, seconds, out)
            }
        }
    }

    fn bench<S: Source<Picture = PixelBuffer>>(
        label: &str,
        source: S,
        config: EncoderConfig,
        pacing: Pacing,
        seconds: f64,
        out: Option<String>,
    ) -> Result<(), String> {
        let mut source = Timed { inner: source, picture_ms: Vec::new() };
        let mut encoder = VtEncoder::new(config.clone()).map_err(|e| e.to_string())?;
        let controls = Arc::new(Controls::default());
        // The window is counted from the first access unit; this ends a run that gets none.
        let watchdog = controls.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(seconds + 10.0));
            watchdog.stop();
        });
        let mut file = out.map(std::fs::File::create).transpose().map_err(|e| e.to_string())?;
        let (mut encode_ms, mut sizes, mut keyframes) = (Vec::new(), 0u64, Vec::new());
        let (mut first, mut heartbeats, mut last_pts) = (None, 0u64, None);
        let mut monotonic = true;
        let window = Duration::from_secs_f64(seconds);
        let stats = stream::run(&mut source, &mut encoder, pacing, &controls, |unit| {
            // The window starts at the first access unit: session setup is not per-frame cost.
            let (t0, _, _) = *first.get_or_insert_with(|| (Instant::now(), own_cpu_ns(), xpc_cpu_ns()));
            monotonic &= last_pts.is_none_or(|p| unit.pts > p);
            last_pts = Some(unit.pts);
            encode_ms.push(unit.encode_time.as_secs_f64() * 1e3);
            sizes += unit.data.len() as u64;
            heartbeats += u64::from(unit.heartbeat);
            if unit.keyframe {
                keyframes.push(unit.data.len() as f64);
            }
            if let Some(f) = file.as_mut() {
                f.write_all(&unit.data).map_err(|e| simee_encode::Error::Encoder(e.to_string()))?;
            }
            Ok(if t0.elapsed() >= window { Flow::Stop } else { Flow::Continue })
        })
        .map_err(|e| e.to_string())?;
        let (t0, cpu0, xpc0) = first.ok_or("no access unit came out")?;
        let wall = t0.elapsed().as_secs_f64();
        let (cpu, xpc) = (own_cpu_ns().saturating_sub(cpu0) as f64 / 1e9, xpc_cpu_ns().saturating_sub(xpc0) as f64 / 1e9);
        let n = encode_ms.len();
        let (enc_p50, enc_p95) = median_p95(&mut encode_ms);
        let (pic_p50, pic_p95) = median_p95(&mut source.picture_ms);
        let first_key = keyframes.first().copied().unwrap_or(0.0);
        let (key_p50, _) = median_p95(&mut keyframes);
        let (w, h) = encoder.output_size().unwrap_or_default();
        println!(
            "{{\"device\":\"{label}\",\"fps_cap\":{fps},\"profile\":\"{profile:?}\",\"hardware\":{hw},\"width\":{w},\"height\":{h},\
             \"seconds\":{wall:.2},\"frames\":{frames},\"encoded\":{n},\"fps\":{rate:.1},\"heartbeats\":{heartbeats},\
             \"superseded\":{sup},\"encode_ms_p50\":{enc_p50:.2},\"encode_ms_p95\":{enc_p95:.2},\"picture_ms_p50\":{pic_p50:.3},\
             \"picture_ms_p95\":{pic_p95:.3},\"cpu_pct\":{cpu_pct:.1},\"xpc_cpu_pct\":{xpc_pct:.1},\"kbps\":{kbps:.0},\
             \"target_kbps\":{target},\"keyframes\":{keys},\"first_keyframe_bytes\":{first_key:.0},\"keyframe_bytes_p50\":{key_p50:.0},\
             \"pts_monotonic\":{monotonic}}}",
            fps = config.fps,
            profile = config.profile,
            hw = encoder.hardware().unwrap_or(false),
            frames = stats.frames,
            rate = n as f64 / wall,
            sup = stats.superseded,
            cpu_pct = cpu / wall * 100.0,
            xpc_pct = xpc / wall * 100.0,
            kbps = sizes as f64 * 8.0 / wall / 1e3,
            target = config.bitrate_bps / 1000,
            keys = stats.keyframes,
        );
        Ok(())
    }
}
