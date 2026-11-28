//! Tap-to-frame latency through `simee serve`: a touch down sent on a kept API connection at a
//! known instant, and the first presented frame whose pixels in a region change, as a frame
//! client of the device sees it. Android: a client of emu's display socket taking frames from its
//! shared ring (what the desktop window and the encoder read). iOS: the framebuffer's
//! damage-rectangle callback (`Display::watch_frames`, what the desktop and the encoder follow),
//! each frame copied to compare the region.
//!
//! Each tap waits until the region has been still for `--idle-ms`, takes it as the baseline, times
//! a no-op `identity` call (the API's own round trip), then sends `touch` down and records when the
//! reply came back and when the first changed frame arrived; then `touch` up after `--hold-ms`.
//! With `--adb` (Android), the guest's side is recorded too: the kernel's timestamp of the touch
//! (`getevent -lt` on the touch screen's tracking id, streamed over a pty so lines arrive as they happen) and the
//! present time SurfaceFlinger gives the frame of `--layer` (`dumpsys SurfaceFlinger --latency`),
//! both on the guest's monotonic clock.
//!
//!     cargo run --release -p simee-api --example input_latency -- --socket S --device D --x X --y Y \
//!         [--region X,Y,W,H] [--taps 60] [--warmup 3] [--idle-ms 300] [--hold-ms 60] [--timeout-ms 3000]
//!         [--csv FILE] [--avd-dir DIR [--adb ADB --serial emulator-N [--input /dev/input/event0]
//!         [--layer "PointerLocation"]]]
//!
//! `--avd-dir` makes it Android (the AVD's directory, for its display socket); without it `--device`
//! is an iOS simulator's UDID. The region (pixels, default 64x64 around the tap) should change on
//! touch down and nowhere else: Android's pointer location overlay (`settings put system
//! pointer_location 1`) draws crosshairs through the touch at once; on iOS a keyboard key's
//! preview balloon. scripts/bench/input-latency.sh runs both; docs/research/input-latency.md has
//! the method and the numbers.
use sim_core::net::UnixStream;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A rectangle in device pixels.
#[derive(Clone, Copy, Debug)]
struct Region {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Region {
    fn parse(s: &str) -> Region {
        let v: Vec<u32> = s.split(',').map(|n| n.trim().parse().expect("--region X,Y,W,H")).collect();
        assert_eq!(v.len(), 4, "--region X,Y,W,H");
        Region { x: v[0], y: v[1], w: v[2], h: v[3] }
    }

    /// The region's RGBA bytes of a `width` x `height` frame whose pixels are `scale` device pixels.
    fn crop(&self, rgba: &[u8], width: u32, height: u32, scale: u32) -> Vec<u8> {
        let s = scale.max(1);
        let (x0, y0) = ((self.x / s).min(width), (self.y / s).min(height));
        let (x1, y1) = (((self.x + self.w) / s).min(width), ((self.y + self.h) / s).min(height));
        let mut out = Vec::with_capacity(((x1 - x0) * (y1 - y0) * 4) as usize);
        for y in y0..y1 {
            let row = (y * width * 4) as usize;
            out.extend_from_slice(&rgba[row + (x0 * 4) as usize..row + (x1 * 4) as usize]);
        }
        out
    }
}

/// A frame as the watcher saw it: when it arrived, its number, and the region's pixels.
struct Seen {
    at: Instant,
    frame: u64,
    region: Vec<u8>,
}

/// One kept connection to `simee serve`.
struct Api {
    stream: BufReader<UnixStream>,
    id: u64,
    line: Vec<u8>,
}

impl Api {
    fn connect(socket: &str) -> Api {
        let stream = UnixStream::connect(socket).unwrap_or_else(|e| panic!("{socket}: {e}"));
        Api { stream: BufReader::new(stream), id: 0, line: Vec::new() }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let mut request = serde_json::to_vec(&json!({"id": self.id, "method": method, "params": params})).expect("json");
        request.push(b'\n');
        self.stream.get_mut().write_all(&request).expect("write to simee serve");
        self.line.clear();
        self.stream.read_until(b'\n', &mut self.line).expect("read from simee serve");
        let reply: Value = serde_json::from_slice(&self.line).expect("a JSON reply");
        assert!(reply["ok"] == json!(true), "{method}: {reply}");
        reply["result"].clone()
    }
}

/// Frames from emu's display socket, through the shared ring, at full size.
fn android_frames(avd_dir: PathBuf, region: Region, tx: Sender<Seen>) {
    use sim_android::display::{Display, Frames};
    use sim_core::stream::Input;
    let mut display = Display::connect(&avd_dir).expect("the display socket");
    display.request_shared().expect("shm");
    display.send(&Input::Hello { max_width: 8192 }).expect("hello");
    let mut frames = Frames::new(display.try_clone().expect("clone")).with_fallback(display.try_clone().expect("clone"));
    loop {
        let Ok((header, rgba)) = frames.next_frame() else { return };
        let at = Instant::now();
        let scale = header.device_width / header.width.max(1);
        let seen = Seen { at, frame: header.frame, region: region.crop(&rgba, header.width, header.height, scale) };
        if tx.send(seen).is_err() {
            return;
        }
    }
}

/// Frames of an iOS simulator's main display: the damage callback's time, then a copy of the
/// framebuffer for the region (dropped and left to the next callback when the surface changed
/// while it was being copied).
#[cfg(target_os = "macos")]
fn ios_frames(udid: String, region: Region, tx: Sender<Seen>) {
    use std::sync::Condvar;
    let device = sim_coresim::Context::new().and_then(|c| c.device(&udid)).expect("the simulator");
    let display = device.display().expect("its display");
    let signal = Arc::new((Mutex::new((0u64, Instant::now())), Condvar::new()));
    let s = signal.clone();
    let _watch = display
        .watch_frames(move || {
            let mut g = s.0.lock().unwrap_or_else(|e| e.into_inner());
            *g = (g.0 + 1, Instant::now());
            s.1.notify_all();
        })
        .expect("the frame callback");
    let mut handled = u64::MAX;
    loop {
        let (count, at) = {
            let g = signal.0.lock().unwrap_or_else(|e| e.into_inner());
            let (g, _) = signal.1.wait_timeout_while(g, Duration::from_secs(1), |g| g.0 == handled).unwrap_or_else(|e| e.into_inner());
            *g
        };
        if count == handled {
            continue;
        }
        handled = count;
        let before = display.seed();
        let Ok(frame) = display.capture() else { continue };
        if display.seed() != before {
            continue;
        }
        let seen = Seen { at, frame: count, region: region.crop(&frame.rgba, frame.width, frame.height, 1) };
        if tx.send(seen).is_err() {
            return;
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn ios_frames(_: String, _: Region, _: Sender<Seen>) {
    panic!("iOS needs macOS");
}

/// Waits until no frame changes the region for `idle`; returns the region as it stands.
fn settle(rx: &Receiver<Seen>, last: &mut Vec<u8>, idle: Duration, max: Duration) -> bool {
    let start = Instant::now();
    let mut quiet = Instant::now();
    while start.elapsed() < max {
        let left = idle.saturating_sub(quiet.elapsed());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(left) {
            Ok(seen) if seen.region != *last => {
                *last = seen.region;
                quiet = Instant::now();
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => panic!("the frame watcher stopped"),
        }
    }
    false
}

/// The guest's touch-down timestamps (seconds, its CLOCK_MONOTONIC), each with when its line
/// reached this process.
type Downs = Arc<Mutex<Vec<(f64, Instant)>>>;

/// `adb shell -tt getevent -lt <input>`: a pty, so getevent's lines aren't held in a pipe buffer.
fn getevent(adb: &str, serial: &str, input: &str) -> (Child, Downs) {
    let mut child = Command::new(adb)
        .args(["-s", serial, "shell", "-tt", "getevent", "-lt", input])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("adb getevent");
    let downs: Downs = Arc::default();
    let out = child.stdout.take().expect("stdout");
    let d = downs.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { return };
            let at = Instant::now();
            // "[    1234.567890] EV_ABS       ABS_MT_TRACKING_ID   00000000": a finger down (emu's
            // multi-touch screen sends no BTN_TOUCH; ffffffff is the finger lifting).
            if line.contains("ABS_MT_TRACKING_ID") && !line.contains("ffffffff") {
                let ts = line.split(']').next().and_then(|s| s.trim_start_matches('[').trim().parse::<f64>().ok());
                if let Some(ts) = ts {
                    d.lock().unwrap_or_else(|e| e.into_inner()).push((ts, at));
                }
            }
        }
    });
    (child, downs)
}

/// The full name of the first SurfaceFlinger layer whose name starts with `prefix`.
fn layer_name(adb: &str, serial: &str, prefix: &str) -> Option<String> {
    let out = Command::new(adb).args(["-s", serial, "shell", "dumpsys", "SurfaceFlinger", "--list"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let name = l.trim().trim_start_matches("RequestedLayerState{");
        let name = name.split(" parentId=").next()?.trim_end_matches('}');
        name.starts_with(prefix).then(|| name.to_owned())
    })
}

/// The layer's actual present times (guest seconds) of its last 127 frames.
fn presents(adb: &str, serial: &str, layer: &str, into: &mut Vec<f64>) {
    let Ok(out) = Command::new(adb).args(["-s", serial, "shell", "dumpsys", "SurfaceFlinger", "--latency", &format!("'{layer}'")]).output()
    else {
        return;
    };
    for l in String::from_utf8_lossy(&out.stdout).lines().skip(1) {
        let cols: Vec<i64> = l.split_whitespace().filter_map(|c| c.parse().ok()).collect();
        if let [_, actual, _] = cols[..]
            && actual > 0
            && actual < i64::MAX
        {
            let t = actual as f64 / 1e9;
            if !into.iter().any(|p| (p - t).abs() < 1e-7) {
                into.push(t);
            }
        }
    }
}

struct Tap {
    t0: Instant,
    noop_ms: f64,
    reply_ms: f64,
    frame_ms: Option<f64>,
    unchanged: u32,
    frame: u64,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn quantile(v: &[f64], q: f64) -> f64 {
    let mut v: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn main() {
    let (mut socket, mut device, mut avd_dir) = (String::new(), String::new(), None);
    let (mut x, mut y, mut region) = (None::<u32>, None::<u32>, None);
    let (mut taps, mut warmup, mut idle_ms, mut hold_ms, mut timeout_ms) = (60usize, 3usize, 300u64, 60u64, 3000u64);
    let (mut csv, mut adb, mut serial) = (None, None::<String>, String::new());
    let (mut input, mut layer_prefix) = ("/dev/input/event0".to_owned(), "PointerLocation".to_owned());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut v = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--socket" => socket = v(),
            "--device" => device = v(),
            "--avd-dir" => avd_dir = Some(PathBuf::from(v())),
            "--x" => x = v().parse().ok(),
            "--y" => y = v().parse().ok(),
            "--region" => region = Some(Region::parse(&v())),
            "--taps" => taps = v().parse().expect("--taps N"),
            "--warmup" => warmup = v().parse().expect("--warmup N"),
            "--idle-ms" => idle_ms = v().parse().expect("--idle-ms N"),
            "--hold-ms" => hold_ms = v().parse().expect("--hold-ms N"),
            "--timeout-ms" => timeout_ms = v().parse().expect("--timeout-ms N"),
            "--csv" => csv = Some(PathBuf::from(v())),
            "--adb" => adb = Some(v()),
            "--serial" => serial = v(),
            "--input" => input = v(),
            "--layer" => layer_prefix = v(),
            other => panic!("unknown argument {other} (the header comment lists them)"),
        }
    }
    let (x, y) = (x.expect("--x"), y.expect("--y"));
    assert!(!socket.is_empty() && !device.is_empty(), "--socket and --device are required");
    let region = region.unwrap_or(Region { x: x.saturating_sub(32), y: y.saturating_sub(32), w: 64, h: 64 });
    let (idle, hold, timeout) = (Duration::from_millis(idle_ms), Duration::from_millis(hold_ms), Duration::from_millis(timeout_ms));

    let (tx, rx) = channel();
    let watcher_device = device.clone();
    let android = avd_dir.clone();
    std::thread::spawn(move || match android {
        Some(dir) => android_frames(dir, region, tx),
        None => ios_frames(watcher_device, region, tx),
    });
    let mut api = Api::connect(&socket);
    let guest = adb.as_deref().filter(|_| avd_dir.is_some()).map(|adb| {
        let layer = layer_name(adb, &serial, &layer_prefix).unwrap_or_else(|| panic!("no SurfaceFlinger layer named {layer_prefix}*"));
        eprintln!("input_latency: guest timestamps from {input} and layer {layer}");
        let (child, downs) = getevent(adb, &serial, &input);
        (adb.to_owned(), layer, child, downs)
    });
    // The watcher's first frame (Android sends the newest on hello; iOS needs a damage callback, so
    // a touch on the region starts it).
    let mut last = rx.recv_timeout(Duration::from_secs(2)).map(|s| s.region).unwrap_or_default();
    let start = Instant::now();
    let mut results = Vec::new();
    let mut present_times = Vec::new();
    for i in 0..warmup + taps {
        if !settle(&rx, &mut last, idle, Duration::from_secs(10)) {
            eprintln!("input_latency: tap {i}: the region never settled; tapping anyway");
        }
        let baseline = last.clone();
        let t = Instant::now();
        api.call("identity", json!({}));
        let noop_ms = ms(t.elapsed());
        let t0 = Instant::now();
        api.call("touch", json!({"device": device, "x": x, "y": y, "phase": "down"}));
        let reply_ms = ms(t0.elapsed());
        let (mut frame_ms, mut unchanged, mut frame) = (None, 0, 0);
        while t0.elapsed() < timeout {
            match rx.recv_timeout(timeout.saturating_sub(t0.elapsed())) {
                Ok(seen) if seen.region != baseline => {
                    frame_ms = Some(ms(seen.at.saturating_duration_since(t0)));
                    frame = seen.frame;
                    last = seen.region;
                    break;
                }
                Ok(_) => unchanged += 1,
                Err(_) => break,
            }
        }
        std::thread::sleep(hold);
        api.call("touch", json!({"device": device, "x": x, "y": y, "phase": "up"}));
        if i >= warmup {
            results.push(Tap { t0, noop_ms, reply_ms, frame_ms, unchanged, frame });
        }
        if let Some((adb, layer, ..)) = &guest
            && (i + 1) % 20 == 0
        {
            presents(adb, &serial, layer, &mut present_times);
        }
    }
    settle(&rx, &mut last, idle, Duration::from_secs(5));

    // The guest's side: each tap's touch-down kernel time (the first DOWN whose line arrived after
    // the tap was sent) and the first present of the layer after it.
    let mut guest_cols: Vec<(Option<f64>, Option<f64>, Option<f64>)> = vec![(None, None, None); results.len()];
    if let Some((adb, layer, mut child, downs)) = guest {
        presents(&adb, &serial, &layer, &mut present_times);
        present_times.sort_by(f64::total_cmp);
        let _ = child.kill();
        let _ = child.wait();
        let downs = downs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (tap, cols) in results.iter().zip(guest_cols.iter_mut()) {
            if let Some((ts, at)) = downs.iter().find(|(_, at)| *at >= tap.t0) {
                let present = present_times.iter().copied().find(|p| *p > *ts);
                *cols = (Some(*ts), present, Some(ms(at.saturating_duration_since(tap.t0))));
            }
        }
    }

    let mut out = String::from(
        "tap,t0_s,noop_ms,touch_reply_ms,frame_ms,unchanged_frames,frame,guest_down_s,guest_present_s,guest_ms,getevent_line_ms\n",
    );
    let opt = |v: Option<f64>, digits: usize| v.map_or(String::new(), |v| format!("{v:.digits$}"));
    for (n, (tap, (down, present, line))) in results.iter().zip(&guest_cols).enumerate() {
        let guest_ms = down.zip(*present).map(|(d, p)| (p - d) * 1000.0);
        out.push_str(&format!(
            "{n},{:.6},{:.3},{:.3},{},{},{},{},{},{},{}\n",
            tap.t0.duration_since(start).as_secs_f64(),
            tap.noop_ms,
            tap.reply_ms,
            opt(tap.frame_ms, 3),
            tap.unchanged,
            tap.frame,
            opt(*down, 6),
            opt(*present, 6),
            opt(guest_ms, 3),
            opt(*line, 3)
        ));
    }
    if let Some(path) = &csv {
        std::fs::write(path, &out).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }

    // The summary: median, p90 and p99 of each stage.
    let frames: Vec<f64> = results.iter().filter_map(|t| t.frame_ms).collect();
    let missed = results.len() - frames.len();
    let stage = |name: &str, v: Vec<f64>| json!({"stage": name, "n": v.iter().filter(|x| x.is_finite()).count(), "median_ms": quantile(&v, 0.5), "p90_ms": quantile(&v, 0.9), "p99_ms": quantile(&v, 0.99)});
    let mut stages = vec![
        stage("api round trip (identity)", results.iter().map(|t| t.noop_ms).collect()),
        stage("touch down reply (API receive + input sent)", results.iter().map(|t| t.reply_ms).collect()),
        stage("input sent -> first changed frame seen", results.iter().filter_map(|t| t.frame_ms.map(|f| f - t.reply_ms)).collect()),
        stage("tap -> first changed frame seen", frames.clone()),
    ];
    let guest: Vec<f64> = guest_cols.iter().filter_map(|(d, p, _)| d.zip(*p).map(|(d, p)| (p - d) * 1000.0)).collect();
    if !guest.is_empty() {
        let rest: Vec<f64> =
            results.iter().zip(&guest_cols).filter_map(|(t, (d, p, _))| Some(t.frame_ms? - (p.as_ref()? - d.as_ref()?) * 1000.0)).collect();
        stages.push(stage("tap -> guest kernel event (getevent line, upper bound)", guest_cols.iter().filter_map(|c| c.2).collect()));
        stages.push(stage("guest kernel event -> SurfaceFlinger present", guest));
        stages.push(stage("host: tap -> kernel + present -> seen", rest));
    }
    let summary = json!({"taps": results.len(), "missed": missed, "stages": stages});
    println!("{}", serde_json::to_string_pretty(&summary).expect("json"));
    for s in &stages {
        eprintln!(
            "{:<58} n={:<3} median {:>7.2} ms  p90 {:>7.2} ms  p99 {:>7.2} ms",
            s["stage"].as_str().unwrap_or_default(),
            s["n"],
            s["median_ms"].as_f64().unwrap_or(f64::NAN),
            s["p90_ms"].as_f64().unwrap_or(f64::NAN),
            s["p99_ms"].as_f64().unwrap_or(f64::NAN)
        );
    }
    if missed > 0 {
        eprintln!("input_latency: {missed} taps saw no changed frame within {timeout_ms} ms");
    }
}
