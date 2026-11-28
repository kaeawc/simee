//! Encodes synthetic phone screens with OpenH264 for a while through the real loop and prints one
//! JSON line: frames, encode latency (median, p95, max), the colour conversion and scaling on
//! their own, CPU, output bitrate and keyframe sizes. No device: the screens are pre-rendered so
//! the measurement is the encoder's, not the generator's.
//!
//! ```text
//! cargo run --release -p simee-encode --features openh264 --example linux_bench -- \
//!   [--size 1080x2400] [--max-size 1280] [--fps 30] [--source-fps 60] [--seconds 10] \
//!   [--bitrate 4000000] [--profile main] [--content drawer|noise] [--order rgba|bgra] [--out stream.h264]
//! ```
//! scripts/bench/encode-linux.sh runs the table in docs/streaming.md.

#[cfg(all(target_os = "linux", feature = "openh264"))]
#[path = "synthetic/mod.rs"]
mod synthetic;

#[cfg(all(target_os = "linux", feature = "openh264"))]
fn main() {
    if let Err(e) = bench::run() {
        eprintln!("linux_bench: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(all(target_os = "linux", feature = "openh264")))]
fn main() {
    eprintln!("linux_bench: OpenH264 on Linux only (build with --features openh264)");
}

#[cfg(all(target_os = "linux", feature = "openh264"))]
mod bench {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use simee_encode::pacing::Pacing;
    use simee_encode::stream::{self, Controls, Flow, Source};
    use simee_encode::{Converter, Decoder, Encoder, EncoderConfig, OpenH264Encoder, Picture, PixelOrder, Profile, output_size};

    use crate::synthetic::{self, Content};

    /// Frames at a fixed rate from a pre-rendered cycle, like a device that presents them.
    struct Cycle {
        frames: Vec<Picture>,
        period: Duration,
        next_at: Instant,
        shown: usize,
        current: Option<usize>,
    }

    impl Source for Cycle {
        type Picture = Picture;

        fn wait(&mut self, deadline: Instant) -> simee_encode::Result<Option<Instant>> {
            let (at, frame) = (self.next_at, self.shown % self.frames.len());
            let until = at.min(deadline);
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            if at > deadline {
                return Ok(None);
            }
            self.current = Some(frame);
            self.shown += 1;
            self.next_at += self.period;
            Ok(Some(at))
        }

        fn picture(&mut self) -> simee_encode::Result<Option<&Picture>> {
            Ok(self.current.map(|i| &self.frames[i]))
        }
    }

    fn flag(name: &str) -> Option<String> {
        let args: Vec<String> = std::env::args().collect();
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
    }

    fn number(name: &str, default: u64) -> Result<u64, String> {
        flag(name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("{name} wants a number, got {v}")))
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    /// User plus system CPU time of this process, in seconds.
    fn cpu_seconds() -> f64 {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        // Fields after the `(comm)` one; utime and stime are the 12th and 13th of those.
        let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
        let f: Vec<f64> = rest.split_whitespace().skip(11).take(2).filter_map(|v| v.parse().ok()).collect();
        f.iter().sum::<f64>() / 100.0
    }

    /// Mean luma PSNR in dB of the cycle's frames, encoded in order and decoded again, against the
    /// converted source (what the encoder was handed): the picture quality the bitrate buys.
    fn quality(config: &EncoderConfig, frames: &[Picture], out_size: (u32, u32)) -> Result<f64, String> {
        let mut encoder = OpenH264Encoder::new(config.clone()).map_err(|e| e.to_string())?;
        let mut decoder = Decoder::new().map_err(|e| e.to_string())?;
        let mut converter = Converter::new();
        let mut total = 0.0;
        let mut n = 0;
        // Three passes over the cycle, the first to settle the rate control; only the last counts.
        for (i, frame) in frames.iter().cycle().take(frames.len() * 3).enumerate() {
            let pts = Duration::from_micros(33_333 * i as u64);
            let units = encoder.encode(frame, pts, false).map_err(|e| e.to_string())?;
            let Some(unit) = units.first() else { continue };
            let decoded = decoder.decode_planes(&unit.data).map_err(|e| e.to_string())?;
            if i < frames.len() * 2 {
                continue;
            }
            let source = converter.convert(&frame.raster(), out_size).map_err(|e| e.to_string())?;
            let mse =
                source.y().iter().zip(&decoded.y).map(|(&a, &b)| f64::from(a.abs_diff(b)).powi(2)).sum::<f64>() / source.y().len() as f64;
            total += if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() };
            n += 1;
        }
        Ok(total / f64::from(n.max(1)))
    }

    pub fn run() -> Result<(), String> {
        let (w, h) = flag("--size")
            .map(|s| {
                let (w, h) = s.split_once('x').ok_or("--size wants WxH")?;
                Ok::<_, String>((w.parse().map_err(|_| "--size width")?, h.parse().map_err(|_| "--size height")?))
            })
            .transpose()?
            .unwrap_or((1080u32, 2400u32));
        let fps = number("--fps", 30)? as u32;
        let source_fps = number("--source-fps", 60)?;
        let seconds = number("--seconds", 10)?;
        let max_size = flag("--max-size").map(|v| v.parse::<u32>().map_err(|_| "--max-size wants a number")).transpose()?;
        let content = match flag("--content").as_deref() {
            None | Some("drawer") => Content::Drawer,
            Some("noise") => Content::Noise,
            Some(other) => return Err(format!("--content drawer|noise, not {other}")),
        };
        let order = if flag("--order").as_deref() == Some("bgra") { PixelOrder::Bgra } else { PixelOrder::Rgba };
        let config = EncoderConfig {
            profile: if flag("--profile").as_deref() == Some("main") { Profile::Main } else { Profile::ConstrainedBaseline },
            fps,
            bitrate_bps: number("--bitrate", 4_000_000)? as u32,
            max_size,
            ..EncoderConfig::default()
        };
        // Ten steps of a 24-pixel scroll, then back: a drawer being dragged.
        let frames: Vec<Picture> =
            (0..16).map(|i| synthetic::picture(content, w, h, if i < 8 { i * 24 } else { (15 - i) * 24 }, order)).collect();
        let out_size = output_size((w, h), max_size);

        // The conversion (and scaling) on their own.
        let mut converter = Converter::new();
        let mut conv_ms = Vec::new();
        for i in 0..40 {
            let t = Instant::now();
            converter.convert(&frames[i % frames.len()].raster(), out_size).map_err(|e| e.to_string())?;
            conv_ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        conv_ms.drain(..8);
        conv_ms.sort_by(f64::total_cmp);

        let psnr = quality(&config, &frames, out_size)?;
        let mut encoder = OpenH264Encoder::new(config.clone()).map_err(|e| e.to_string())?;
        let mut source = Cycle {
            frames,
            period: Duration::from_micros(1_000_000 / source_fps.max(1)),
            next_at: Instant::now(),
            shown: 0,
            current: None,
        };
        let mut out = flag("--out").map(|p| std::fs::File::create(p).map_err(|e| e.to_string())).transpose()?;
        let (mut enc_ms, mut idr_kb) = (Vec::new(), Vec::new());
        let (mut bytes, mut first_idr) = (0u64, None);
        let (cpu0, t0) = (cpu_seconds(), Instant::now());
        let stats = stream::run(&mut source, &mut encoder, Pacing::new(fps, None), &Controls::default(), |unit| {
            enc_ms.push(unit.encode_time.as_secs_f64() * 1e3);
            bytes += unit.data.len() as u64;
            if unit.keyframe {
                first_idr.get_or_insert(unit.data.len());
                idr_kb.push(unit.data.len() as f64 / 1e3);
            }
            if let Some(f) = out.as_mut() {
                f.write_all(&unit.data).map_err(|e| simee_encode::Error::Source(e.to_string()))?;
            }
            Ok(if t0.elapsed() >= Duration::from_secs(seconds) { Flow::Stop } else { Flow::Continue })
        })
        .map_err(|e| e.to_string())?;
        let (wall, cpu) = (t0.elapsed().as_secs_f64(), cpu_seconds() - cpu0);
        // The first frames warm the encoder up (threads, caches); a median needs steady state.
        let warm = enc_ms.len().min(5);
        let mut steady: Vec<f64> = enc_ms[warm..].to_vec();
        steady.sort_by(f64::total_cmp);
        idr_kb.sort_by(f64::total_cmp);
        println!(
            "{{\"size\":\"{w}x{h}\",\"output\":\"{}x{}\",\"content\":\"{content:?}\",\"profile\":\"{:?}\",\"fps_cap\":{fps},\"source_fps\":{source_fps},\"encoded\":{},\"encoded_fps\":{:.1},\"dropped\":{},\"encode_ms_p50\":{:.2},\"encode_ms_p95\":{:.2},\"encode_ms_max\":{:.2},\"convert_ms_p50\":{:.2},\"convert_ms_p95\":{:.2},\"cpu_pct_of_a_core\":{:.1},\"luma_psnr_db\":{:.1},\"kbit_s\":{:.0},\"first_idr_kb\":{:.1},\"idr_kb_median\":{:.1},\"keyframes\":{}}}",
            out_size.0,
            out_size.1,
            config.profile,
            stats.encoded,
            stats.encoded as f64 / wall,
            stats.dropped,
            percentile(&steady, 0.5),
            percentile(&steady, 0.95),
            steady.last().copied().unwrap_or(0.0),
            percentile(&conv_ms, 0.5),
            percentile(&conv_ms, 0.95),
            cpu / wall * 100.0,
            psnr,
            bytes as f64 * 8.0 / wall / 1e3,
            first_idr.unwrap_or(0) as f64 / 1e3,
            percentile(&idr_kb, 0.5),
            stats.keyframes,
        );
        Ok(())
    }
}
