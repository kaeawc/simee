//! The camera benchmark's measuring side (`scripts/bench/camera.sh` runs it):
//!
//! ```text
//! camera-bench fanout [--readers 1,2,4,8] [--secs 5] [--size 640x480@30] [--mixed] [--source fake-webcam|<source id>]
//! camera-bench latency --socket <path> --shot-cmd '<command writing a PNG to {}>' [--shots 30] [--size 640x480@30]
//!                      [--point 50,50] [--go-file <path>]
//! ```
//!
//! `fanout` is host-only: one source fanned out to N camera feeds, one per linked device, as the
//! simee server serves them (each with one reader, the device's camera, at its size), for each N.
//! The readers run in a child process (`camera-bench read`, this same binary), as a device's camera
//! reads in the device's own process, so the two sides' CPU is measured apart. Per N it prints the
//! server side's CPU (capture, fan-out, socket writes: this process) as a share of one core, in all
//! and per device; the readers' CPU (the child); the slowest reader's frame rate; and
//! source-to-reader latency through the feed (p50, p95, by the wall clock both processes share).
//! `fake-webcam` is a camera-like source (1280x720 frames, fitted to each reader's size, as a
//! webcam's are); `webcam:<id>` is a real one. `--mixed` gives the readers different sizes
//! (640x480, 1280x720, 352x288, 320x240).
//!
//! `latency` serves a feed on `--socket` whose frames encode the frame number in flat colours
//! (two bits per channel, levels 0, 85, 170, 255, which survive scaling, YUV round trips and a
//! camera app's processing; never black or white, which a screen around the preview may be): the
//! centre box (three fifths of the frame's width and height) shows the number mod 62, and the rest
//! of the frame a colour that differs from the centre's and gives the number divided by 62, mod 61.
//! Centre and surround survive rotation and mirroring too, and together they number [`PERIOD`]
//! (3782) frames, 126 s at 30 fps: the run takes no shot once the counter has passed that, so no
//! two frames it can match share a number. Once a reader connects (a device's
//! camera linked to this feed) it prints `connected`, waits for `--go-file` to exist (when given),
//! then takes `--shots` screenshots with `--shot-cmd` (`{}` is the PNG path to write), reads the
//! centre colour at `--point` (percent of the width and height; it must be inside the preview's
//! centre box) and the surround by walking out from it, and matches the number to the frame sent
//! with it. Latency is from sending a frame to the screenshot that shows it, as an interval: the
//! shot was taken between the command's start and its return. Prints p50 and p95 of the interval's
//! midpoint and of its upper end, and the feed side's CPU.
//!
//! Results are lines `RESULT<TAB>arm<TAB>metric<TAB>value`, which the script records.

#[cfg(unix)]
fn main() {
    if let Err(e) = bench::run(std::env::args().skip(1).collect()) {
        eprintln!("camera-bench: {e}");
        std::process::exit(2);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("camera-bench needs Unix sockets");
}

#[cfg(unix)]
mod bench {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use simee_camera::feed::{Feed, FeedReader, Hello};
    use simee_camera::{CameraFrame, FrameSpec, HostOpener, Hub, Opener, Result, Source, SourceSpec, Tick};

    type Res<T> = std::result::Result<T, String>;

    pub fn run(args: Vec<String>) -> Res<()> {
        let (mode, rest) = args.split_first().ok_or("usage: camera-bench fanout|latency [options] (see the source's header)")?;
        let opts = Opts::parse(rest)?;
        match mode.as_str() {
            "fanout" => fanout(&opts),
            "latency" => latency(&opts),
            "read" => read(&opts),
            other => Err(format!("unknown mode {other}: fanout or latency")),
        }
    }

    struct Opts {
        readers: Vec<usize>,
        secs: f64,
        spec: FrameSpec,
        mixed: bool,
        source: String,
        socket: Option<PathBuf>,
        shot_cmd: Option<String>,
        shots: usize,
        point: (f64, f64),
        go_file: Option<PathBuf>,
        /// `read`: the feeds to read, one reader each, and each reader's size.
        sockets: Vec<PathBuf>,
        sizes: Vec<(u32, u32)>,
    }

    impl Opts {
        fn parse(args: &[String]) -> Res<Opts> {
            let mut o = Opts {
                readers: vec![1, 2, 4, 8],
                secs: 5.0,
                spec: FrameSpec::default(),
                mixed: false,
                source: "fake-webcam".into(),
                socket: None,
                shot_cmd: None,
                shots: 30,
                point: (50.0, 50.0),
                go_file: None,
                sockets: Vec::new(),
                sizes: Vec::new(),
            };
            let mut it = args.iter();
            while let Some(a) = it.next() {
                let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
                match a.as_str() {
                    "--readers" => {
                        o.readers =
                            val()?.split(',').map(|n| n.trim().parse().map_err(|_| format!("--readers: {n}"))).collect::<Res<_>>()?
                    }
                    "--secs" => o.secs = val()?.parse().map_err(|_| "--secs: a number")?,
                    "--size" => o.spec = parse_spec(&val()?)?,
                    "--mixed" => o.mixed = true,
                    "--source" => o.source = val()?,
                    "--socket" => o.socket = Some(val()?.into()),
                    "--shot-cmd" => o.shot_cmd = Some(val()?),
                    "--shots" => o.shots = val()?.parse().map_err(|_| "--shots: a number")?,
                    "--point" => {
                        let v = val()?;
                        let (x, y) = v.split_once(',').ok_or("--point: X,Y in percent")?;
                        o.point = (x.parse().map_err(|_| "--point: X")?, y.parse().map_err(|_| "--point: Y")?);
                    }
                    "--go-file" => o.go_file = Some(val()?.into()),
                    "--sockets" => o.sockets = val()?.split(',').map(PathBuf::from).collect(),
                    "--sizes" => {
                        o.sizes = val()?
                            .split(',')
                            .map(|s| {
                                let (w, h) = s.split_once('x').ok_or_else(|| format!("--sizes: {s}"))?;
                                Ok((w.parse().map_err(|_| format!("--sizes: {s}"))?, h.parse().map_err(|_| format!("--sizes: {s}"))?))
                            })
                            .collect::<Res<_>>()?
                    }
                    other => return Err(format!("unknown option {other}")),
                }
            }
            Ok(o)
        }
    }

    fn parse_spec(s: &str) -> Res<FrameSpec> {
        let bad = || format!("{s}: expected WIDTHxHEIGHT@FPS");
        let (size, fps) = s.split_once('@').ok_or_else(bad)?;
        let (w, h) = size.split_once('x').ok_or_else(bad)?;
        let num = |v: &str| v.parse::<u32>().map_err(|_| bad());
        FrameSpec::new(num(w)?, num(h)?, num(fps)?).map_err(|e| e.to_string())
    }

    /// Wall-clock time, in nanoseconds since the Unix epoch: the clock the server and the reader
    /// process share.
    fn wall_ns(t: SystemTime) -> u64 {
        t.duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }

    /// When each frame number was produced (monotonic, and by the wall clock), for the last few
    /// minutes of frames.
    #[derive(Default)]
    struct Sent(Mutex<VecDeque<(u64, Instant, u64)>>);

    impl Sent {
        fn record(&self, seq: u64) {
            let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
            q.push_back((seq, Instant::now(), wall_ns(SystemTime::now())));
            while q.len() > 2 * PERIOD as usize {
                q.pop_front();
            }
        }

        fn clear(&self) {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }

        /// When frame `seq` was produced, by the wall clock.
        fn wall(&self, seq: u64) -> Option<u64> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).iter().rev().find(|(s, ..)| *s == seq).map(|&(.., w)| w)
        }

        /// The newest frame number produced.
        fn last(&self) -> Option<u64> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).back().map(|&(s, ..)| s)
        }

        /// The newest frame whose number is `code` mod [`PERIOD`], produced by `before`.
        fn newest(&self, code: u64, before: Instant) -> Option<(u64, Instant)> {
            let q = self.0.lock().unwrap_or_else(|p| p.into_inner());
            q.iter().rev().find(|(s, t, _)| s % PERIOD == code && *t <= before).map(|&(s, t, _)| (s, t))
        }
    }

    /// Records when each frame a source produces was produced.
    struct Recorded {
        inner: Box<dyn Source>,
        sent: Arc<Sent>,
    }

    impl Source for Recorded {
        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let f = self.inner.frame(spec, tick)?;
            self.sent.record(tick.seq);
            Ok(f)
        }

        fn synthetic(&self) -> bool {
            self.inner.synthetic()
        }
    }

    /// A camera-like source: 1280x720 frames (one moving bar on grey), fitted to each reader's size
    /// as a webcam's frames are.
    struct FakeWebcam;

    impl Source for FakeWebcam {
        fn frame(&mut self, _spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let (w, h) = (1280u32, 720u32);
            let mut bgra = vec![96u8; w as usize * h as usize * 4];
            let x = (tick.seq % 64) as usize * (w as usize / 64);
            for row in bgra.chunks_exact_mut(w as usize * 4) {
                row[x * 4..(x + 20) * 4].fill(255);
            }
            Ok(CameraFrame { width: w, height: h, bgra: bgra.into(), tick })
        }
    }

    const LEVELS: [u8; 4] = [0, 85, 170, 255];

    /// The colours the counter uses: every 6-bit code but black (0) and white (63).
    const COLOURS: u64 = 62;

    /// Frames the latency counter numbers apart: 62 centre colours times 61 surround colours (the
    /// surround never repeats the centre's colour, so the boundary between them always shows).
    const PERIOD: u64 = COLOURS * (COLOURS - 1);

    /// The RGB colour of a 6-bit code.
    fn colour(code: u64) -> [u8; 3] {
        let code = (code % 64) as usize;
        [LEVELS[code >> 4 & 3], LEVELS[code >> 2 & 3], LEVELS[code & 3]]
    }

    /// Whether a code is one of the counter's colours.
    fn counter_code(code: u64) -> bool {
        (1..=COLOURS).contains(&code)
    }

    /// The centre and surround codes of frame `seq` (codes 1 to 62): the centre's is `seq` mod 62;
    /// the surround's one to 61 colours past it, by `seq / 62` mod 61.
    fn codes(seq: u64) -> (u64, u64) {
        let (centre, high) = (seq % COLOURS, seq / COLOURS % (COLOURS - 1));
        (centre + 1, (centre + 1 + high) % COLOURS + 1)
    }

    /// `seq` mod [`PERIOD`] from a frame's centre and surround codes, when they are a frame's.
    fn number(centre: u64, surround: u64) -> Option<u64> {
        if !counter_code(centre) || !counter_code(surround) {
            return None;
        }
        let (c, s) = (centre - 1, surround - 1);
        let high = (s + COLOURS - c - 1) % COLOURS;
        (high < COLOURS - 1).then_some(high * COLOURS + c)
    }

    /// The latency counter: [`codes`] as flat colours, the centre box three fifths of the frame's
    /// width and height (so a point well inside the preview is in it), the surround a fifth on each
    /// side.
    struct CounterColour;

    impl Source for CounterColour {
        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            Ok(counter_frame(spec.width, spec.height, tick))
        }

        fn synthetic(&self) -> bool {
            true
        }
    }

    fn counter_frame(w: u32, h: u32, tick: Tick) -> CameraFrame {
        let (centre, surround) = codes(tick.seq);
        let bgra = |[r, g, b]: [u8; 3]| [b, g, r, 255];
        let (inner, outer) = (bgra(colour(centre)), bgra(colour(surround)));
        let (w, h) = (w as usize, h as usize);
        let mut px = outer.repeat(w * h);
        for row in px.chunks_exact_mut(w * 4).take(4 * h / 5).skip(h / 5) {
            row[w / 5 * 4..4 * w / 5 * 4].copy_from_slice(&inner.repeat(4 * w / 5 - w / 5));
        }
        CameraFrame { width: w as u32, height: h as u32, bgra: px.into(), tick }
    }

    /// The bench's opener: `text:fake-webcam` and `text:counter` are the sources above; anything
    /// else as the host opens it. Every source is recorded.
    struct Bench {
        host: HostOpener,
        sent: Arc<Sent>,
    }

    impl Opener for Bench {
        fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
            self.host.canonical(spec)
        }

        fn open(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Box<dyn Source>> {
            let inner: Box<dyn Source> = match source {
                SourceSpec::Text(t) if t == "fake-webcam" => Box::new(FakeWebcam),
                SourceSpec::Text(t) if t == "counter" => {
                    // A new capture numbers its frames from 0 again: only its own are matched.
                    self.sent.clear();
                    Box::new(CounterColour)
                }
                other => self.host.open(other, spec)?,
            };
            Ok(Box::new(Recorded { inner, sent: self.sent.clone() }))
        }
    }

    /// This process's CPU time (user and system) so far.
    fn cpu_time() -> Res<Duration> {
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            let after = stat.rsplit_once(") ").ok_or("/proc/self/stat: no command field")?.1;
            let f: Vec<&str> = after.split_whitespace().collect();
            let ticks: u64 = f.get(11).and_then(|u| u.parse().ok()).unwrap_or(0) + f.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
            let hz = Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
                .unwrap_or(100u64);
            return Ok(Duration::from_secs_f64(ticks as f64 / hz as f64));
        }
        let out =
            Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output().map_err(|e| format!("ps: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let secs = text.trim().replace('-', ":").split(':').try_fold(0.0f64, |acc, part| part.parse::<f64>().map(|v| acc * 60.0 + v));
        secs.map(Duration::from_secs_f64).map_err(|_| format!("ps -o time: {text:?}"))
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return f64::NAN;
        }
        let i = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[i.min(sorted.len() - 1)]
    }

    fn result(arm: &str, metric: &str, value: f64) {
        println!("RESULT\t{arm}\t{metric}\t{value:.3}");
    }

    fn sock(tag: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!("simee-cambench-{}-{tag}.sock", std::process::id()))
    }

    const MIXED: [(u32, u32); 4] = [(640, 480), (1280, 720), (352, 288), (320, 240)];

    /// What the reader process reports for one measurement.
    #[derive(Default)]
    struct Readers {
        /// Frames each reader got while measuring.
        frames: Vec<u64>,
        /// Each frame got while measuring: its number and when it arrived (wall clock, ns).
        got: Vec<(u64, u64)>,
        /// The reader process's CPU over the measurement, and its length.
        cpu: Duration,
        wall: Duration,
    }

    fn fanout(o: &Opts) -> Res<()> {
        let source = if o.source == "fake-webcam" {
            SourceSpec::Text("fake-webcam".into())
        } else {
            SourceSpec::parse(&o.source).map_err(|e| e.to_string())?
        };
        let sizes = if o.mixed { "mixed sizes".to_string() } else { format!("{}x{}", o.spec.width, o.spec.height) };
        let arm = format!("fan-out {} {sizes}@{}", o.source, o.spec.fps);
        let exe = std::env::current_exe().map_err(|e| format!("this binary's path: {e}"))?;
        for &n in &o.readers {
            let sent = Arc::new(Sent::default());
            let hub = Arc::new(Hub::new(Bench { host: HostOpener::new(), sent: sent.clone() }));
            let feeds: Vec<Feed> = (0..n)
                .map(|i| {
                    let path = sock(&format!("fan{n}-{i}"));
                    let _ = std::fs::remove_file(&path);
                    Feed::serve(hub.clone(), path, source.clone(), o.spec).map_err(|e| e.to_string())
                })
                .collect::<Res<_>>()?;
            let paths: Vec<String> = feeds.iter().map(|f| f.path().display().to_string()).collect();
            let sizes: Vec<String> = (0..n)
                .map(|i| if o.mixed { MIXED[i % MIXED.len()] } else { (o.spec.width, o.spec.height) })
                .map(|(w, h)| format!("{w}x{h}"))
                .collect();
            // The devices' side, in its own process: N readers, one per feed.
            let mut child = Command::new(&exe)
                .args(["read", "--sockets", &paths.join(","), "--sizes", &sizes.join(",")])
                .args(["--size", &format!("{}x{}@{}", o.spec.width, o.spec.height, o.spec.fps), "--secs", &o.secs.to_string()])
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("{}: {e}", exe.display()))?;
            let out = child.stdout.take().ok_or("the reader process's output")?;
            let mut server = (None, None);
            let mut readers = Readers::default();
            for line in BufReader::new(out).lines() {
                let line = line.map_err(|e| format!("the reader process: {e}"))?;
                let f: Vec<&str> = line.split('\t').collect();
                let num =
                    |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).ok_or_else(|| format!("the reader process said {line:?}"));
                match f[0] {
                    // The reader process measures between these two lines; so does this one.
                    "measuring" => server.0 = Some((cpu_time()?, Instant::now())),
                    "stopped" => server.1 = Some((cpu_time()?, Instant::now())),
                    "FRAMES" => readers.frames.push(num(1)?),
                    "GOT" => readers.got.push((num(1)?, num(2)?)),
                    "CPU" => (readers.cpu, readers.wall) = (Duration::from_nanos(num(1)?), Duration::from_nanos(num(2)?)),
                    _ => {}
                }
            }
            let status = child.wait().map_err(|e| format!("the reader process: {e}"))?;
            drop(feeds);
            let (Some((cpu0, t0)), Some((cpu1, t1))) = server else {
                return Err(format!("the reader process ended ({status}) before measuring"));
            };
            if !status.success() || readers.frames.len() != n {
                return Err(format!("the reader process: {status}, {} of {n} readers reported", readers.frames.len()));
            }
            let wall = t1 - t0;
            let mut lat: Vec<f64> =
                readers.got.iter().filter_map(|&(seq, at)| Some(at.saturating_sub(sent.wall(seq)?) as f64 / 1e6)).collect();
            lat.sort_by(f64::total_cmp);
            let slowest = readers.frames.iter().map(|&f| f as f64 / readers.wall.as_secs_f64()).fold(f64::INFINITY, f64::min);
            let server_cpu = (cpu1 - cpu0).as_secs_f64() / wall.as_secs_f64() * 100.0;
            let reader_cpu = readers.cpu.as_secs_f64() / readers.wall.as_secs_f64() * 100.0;
            let arm = format!("{arm}, N={n}");
            result(&arm, "server CPU, % of one core", server_cpu);
            result(&arm, "server CPU per device, % of one core", server_cpu / n as f64);
            result(&arm, "readers' CPU (their own process), % of one core", reader_cpu);
            result(&arm, "slowest reader, fps", slowest);
            result(&arm, "source to reader p50, ms", percentile(&lat, 0.5));
            result(&arm, "source to reader p95, ms", percentile(&lat, 0.95));
        }
        Ok(())
    }

    /// `read`: the devices' side of `fanout`, in its own process. One reader per `--sockets` feed
    /// at its `--sizes` size; after a second's warm-up, prints `measuring`, counts frames for
    /// `--secs`, prints `stopped`, then each reader's frame count (`FRAMES`), each frame's number
    /// and arrival time (`GOT`) and this process's CPU over the measurement (`CPU`, ns of CPU and
    /// of wall time).
    fn read(o: &Opts) -> Res<()> {
        if o.sockets.is_empty() || o.sockets.len() != o.sizes.len() {
            return Err("read needs --sockets and as many --sizes".into());
        }
        let measuring = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let readers: Vec<_> = o
            .sockets
            .iter()
            .zip(&o.sizes)
            .map(|(path, &(width, height))| {
                let mut r = FeedReader::connect(path, Hello { width, height, fps: o.spec.fps }).map_err(|e| e.to_string())?;
                let (measuring, stop) = (measuring.clone(), stop.clone());
                Ok(thread::spawn(move || {
                    let (mut frames, mut got) = (0u64, Vec::new());
                    while !stop.load(Ordering::SeqCst) {
                        let Ok((header, _px)) = r.next(Duration::from_secs(5)) else { break };
                        if measuring.load(Ordering::SeqCst) {
                            frames += 1;
                            got.push((header.seq, wall_ns(SystemTime::now())));
                        }
                    }
                    (frames, got)
                }))
            })
            .collect::<Res<_>>()?;
        thread::sleep(Duration::from_secs(1));
        let mut out = std::io::stdout().lock();
        let (cpu0, t0) = (cpu_time()?, Instant::now());
        measuring.store(true, Ordering::SeqCst);
        writeln!(out, "measuring").and_then(|()| out.flush()).map_err(|e| e.to_string())?;
        thread::sleep(Duration::from_secs_f64(o.secs));
        measuring.store(false, Ordering::SeqCst);
        let (cpu1, wall) = (cpu_time()?, t0.elapsed());
        writeln!(out, "stopped").and_then(|()| out.flush()).map_err(|e| e.to_string())?;
        stop.store(true, Ordering::SeqCst);
        for r in readers {
            let (frames, got) = r.join().map_err(|_| "a reader panicked")?;
            let mut text = format!("FRAMES\t{frames}\n");
            for (seq, at) in got {
                text += &format!("GOT\t{seq}\t{at}\n");
            }
            out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
        writeln!(out, "CPU\t{}\t{}", (cpu1 - cpu0).as_nanos(), wall.as_nanos()).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The code a colour encodes, or None when a channel is far from every level (not a counter
    /// colour).
    fn decode([r, g, b]: [u8; 3]) -> Option<u64> {
        let level = |v: u8| {
            let (i, d) = LEVELS.iter().enumerate().map(|(i, &l)| (i, l.abs_diff(v))).min_by_key(|&(_, d)| d)?;
            (d <= 30).then_some(i as u64)
        };
        Some(level(r)? << 4 | level(g)? << 2 | level(b)?)
    }

    /// A screenshot, as RGB rows.
    struct Shot {
        width: usize,
        height: usize,
        rgb: Vec<u8>,
    }

    impl Shot {
        fn load(path: &Path) -> Res<Shot> {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
            decoder.set_transformations(png::Transformations::normalize_to_color8());
            let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
            let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
            let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
            let channels = match info.color_type {
                png::ColorType::Rgba => 4,
                png::ColorType::Rgb => 3,
                other => return Err(format!("screenshot colour type {other:?}")),
            };
            let (width, height) = (info.width as usize, info.height as usize);
            let rgb = buf.chunks_exact(channels).take(width * height).flat_map(|p| [p[0], p[1], p[2]]).collect();
            Ok(Shot { width, height, rgb })
        }

        /// The mean colour of the square of `radius` around (x, y), clipped to the image.
        fn mean(&self, x: usize, y: usize, radius: usize) -> [u8; 3] {
            let (mut sum, mut n) = ([0u64; 3], 0u64);
            for yy in y.saturating_sub(radius)..(y + radius + 1).min(self.height) {
                for xx in x.saturating_sub(radius)..(x + radius + 1).min(self.width) {
                    let i = (yy * self.width + xx) * 3;
                    for (s, &v) in sum.iter_mut().zip(&self.rgb[i..i + 3]) {
                        *s += u64::from(v);
                    }
                    n += 1;
                }
            }
            sum.map(|s| (s / n.max(1)) as u8)
        }

        /// The surround's code, walking from (x, y) (inside the centre box, which shows `centre`)
        /// in one direction: the first run of [`RUN`] samples that all decode to one code other
        /// than the centre's. None when the walk leaves the image first.
        fn walk(&self, (x, y): (usize, usize), (dx, dy): (isize, isize), centre: u64) -> Option<u64> {
            let (mut run, mut prev) = (0, None);
            for step in 1.. {
                let (px, py) = (x.checked_add_signed(dx * 2 * step)?, y.checked_add_signed(dy * 2 * step)?);
                if px >= self.width || py >= self.height {
                    return None;
                }
                let code = decode(self.mean(px, py, 1)).filter(|&c| c != centre && counter_code(c));
                run = if code.is_some() && code == prev { run + 1 } else { 1 };
                prev = code;
                if code.is_some() && run >= RUN {
                    return code;
                }
            }
            None
        }

        /// The frame number (mod [`PERIOD`]) a screenshot shows: the centre colour at `point`
        /// (percent), and the surround's, found by walking out from it both ways along one axis
        /// (up and down, or left and right) to the same colour. A point in the centre box meets
        /// the surround on both sides, whatever the rotation; a point outside it, in the surround
        /// or off the preview, meets the box on one side at most, so it reads as no number rather
        /// than a wrong one.
        fn number(&self, point: (f64, f64)) -> Res<u64> {
            let at = ((self.width as f64 * point.0 / 100.0) as usize, (self.height as f64 * point.1 / 100.0) as usize);
            let rgb = self.mean(at.0, at.1, 4);
            let centre = decode(rgb)
                .filter(|&c| counter_code(c))
                .ok_or_else(|| format!("colour {rgb:?} at the point isn't a counter colour (is the preview under --point?)"))?;
            let walk = |d| self.walk(at, d, centre);
            let axes = [(walk((0, -1)), walk((0, 1))), (walk((-1, 0)), walk((1, 0)))];
            let surround = axes.iter().find_map(|&(a, b)| a.filter(|_| a == b)).ok_or_else(|| {
                format!("no surround colour on both sides of the centre's (code {centre}): up/down {:?}, left/right {:?}", axes[0], axes[1])
            })?;
            number(centre, surround).ok_or_else(|| format!("centre {centre} and surround {surround} are no frame's"))
        }
    }

    /// Samples in a row, two pixels apart, that make a walk's surround colour.
    const RUN: usize = 4;

    fn latency(o: &Opts) -> Res<()> {
        let socket = o.socket.clone().ok_or("latency needs --socket")?;
        let cmd = o.shot_cmd.clone().ok_or("latency needs --shot-cmd")?;
        let sent = Arc::new(Sent::default());
        let hub = Arc::new(Hub::new(Bench { host: HostOpener::new(), sent: sent.clone() }));
        let _ = std::fs::remove_file(&socket);
        let feed = Feed::serve(hub, &socket, SourceSpec::Text("counter".into()), o.spec).map_err(|e| e.to_string())?;
        eprintln!("camera-bench: serving the counter source on {}; link a device's camera to feed:{}", socket.display(), socket.display());
        let waited = Instant::now();
        while feed.readers() == 0 {
            if waited.elapsed() > Duration::from_secs(120) {
                return Err("no reader connected in 120 s: is the device's camera linked to the feed and open?".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        println!("connected");
        let (cpu0, t0) = (cpu_time()?, Instant::now());
        if let Some(go) = &o.go_file {
            while !go.exists() {
                thread::sleep(Duration::from_millis(100));
            }
        } else {
            thread::sleep(Duration::from_secs(2));
        }
        let shot = std::env::temp_dir().join(format!("camera-bench-{}.png", std::process::id()));
        let (mut mid, mut hi, mut bad) = (Vec::new(), Vec::new(), 0usize);
        for i in 0..o.shots {
            // Every frame number this run has sent is still unique mod PERIOD: once the next could
            // repeat one, a shot could match the wrong frame, so the run stops taking them.
            if sent.last().is_some_and(|s| s + 1 >= PERIOD) {
                eprintln!("shot {i}: not taken; the counter is about to repeat a number ({PERIOD} frames sent)");
                bad += o.shots - i;
                break;
            }
            let _ = std::fs::remove_file(&shot);
            let line = cmd.replace("{}", &shot.display().to_string());
            let start = Instant::now();
            let status = Command::new("sh").arg("-c").arg(&line).status().map_err(|e| format!("{line}: {e}"))?;
            let end = Instant::now();
            if !status.success() {
                return Err(format!("{line}: {status}"));
            }
            let code = match Shot::load(&shot)?.number(o.point) {
                Ok(code) => code,
                Err(why) => {
                    bad += 1;
                    eprintln!("shot {i}: {why}");
                    continue;
                }
            };
            let Some((seq, at)) = sent.newest(code, end) else {
                bad += 1;
                continue;
            };
            let lo = start.saturating_duration_since(at).as_secs_f64() * 1000.0;
            let up = end.duration_since(at).as_secs_f64() * 1000.0;
            eprintln!("shot {i}: frame {seq}, latency {lo:.1}..{up:.1} ms");
            mid.push((lo + up) / 2.0);
            hi.push(up);
        }
        let (cpu1, wall) = (cpu_time()?, t0.elapsed());
        let _ = std::fs::remove_file(&shot);
        mid.sort_by(f64::total_cmp);
        hi.sort_by(f64::total_cmp);
        let arm = format!("source to guest screen {}x{}@{}", o.spec.width, o.spec.height, o.spec.fps);
        result(&arm, "p50, ms", percentile(&mid, 0.5));
        result(&arm, "p95, ms", percentile(&mid, 0.95));
        result(&arm, "p50 upper bound, ms", percentile(&hi, 0.5));
        result(&arm, "p95 upper bound, ms", percentile(&hi, 0.95));
        result(&arm, "undecoded shots", bad as f64);
        result(&arm, "feed side CPU, % of one core", (cpu1 - cpu0).as_secs_f64() / wall.as_secs_f64() * 100.0);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Every frame number in a period has its own pair of codes, and the surround's colour is
        /// never the centre's.
        #[test]
        fn frame_numbers_are_unique_for_a_whole_period() {
            let mut seen = vec![false; PERIOD as usize];
            for seq in 0..PERIOD {
                let (centre, surround) = codes(seq);
                assert_ne!(centre, surround, "frame {seq}");
                assert!(counter_code(centre) && counter_code(surround), "frame {seq}: {centre}, {surround}: never black or white");
                let n = number(centre, surround).unwrap_or_else(|| panic!("frame {seq}: {centre}, {surround}"));
                assert_eq!(n, seq);
                assert!(!seen[n as usize]);
                seen[n as usize] = true;
            }
            assert_eq!(codes(PERIOD), codes(0), "the counter repeats after PERIOD frames");
        }

        /// A screenshot of a camera app: the counter frame scaled into a preview, rotated a
        /// quarter turn or mirrored as a camera app may, on a black screen with grey buttons.
        fn screenshot(seq: u64, rotate: bool, mirror: bool) -> Shot {
            let frame = counter_frame(640, 480, Tick { seq, ..Tick::start() });
            let (sw, sh) = (720usize, 1280usize);
            let mut rgb = vec![0u8; sw * sh * 3];
            let (pw, ph) = if rotate { (480usize, 640usize) } else { (640, 480) };
            // The preview: nine tenths of the frame's size, centred across, 200 px from the top.
            let (pw2, ph2) = (pw * 9 / 10, ph * 9 / 10);
            let (ox, oy) = ((sw - pw2) / 2, 200usize);
            for y in 0..ph2 {
                for x in 0..pw2 {
                    let (mut fx, mut fy) = (x * pw / pw2, y * ph / ph2);
                    if mirror {
                        fx = pw - 1 - fx;
                    }
                    if rotate {
                        (fx, fy) = (fy, pw - 1 - fx);
                    }
                    let p = frame.pixel(fx as u32, fy as u32);
                    let i = ((oy + y) * sw + ox + x) * 3;
                    rgb[i..i + 3].copy_from_slice(&[p[2], p[1], p[0]]);
                }
            }
            for i in (1100 * sw..1200 * sw).map(|i| i * 3) {
                rgb[i..i + 3].fill(128);
            }
            Shot { width: sw, height: sh, rgb }
        }

        #[test]
        fn a_shot_reads_the_frame_number_rotated_or_mirrored() {
            for seq in [0, 1, 61, 62, 63, 1000, PERIOD - 2, PERIOD - 1] {
                for (rotate, mirror) in [(false, false), (true, false), (false, true), (true, true)] {
                    let shot = screenshot(seq, rotate, mirror);
                    let centre = (50.0, (200.0 + if rotate { 288.0 } else { 216.0 }) / 1280.0 * 100.0);
                    assert_eq!(shot.number(centre), Ok(seq), "frame {seq}, rotated {rotate}, mirrored {mirror}");
                }
            }
        }

        #[test]
        fn a_point_off_the_preview_or_in_its_surround_is_no_number() {
            for seq in [5, 70, 1000] {
                let shot = screenshot(seq, false, false);
                assert!(shot.number((50.0, 5.0)).is_err(), "frame {seq}: the black screen above the preview");
                // The preview's surround, below the centre box: 200 + 432 * 0.9 px down.
                assert!(shot.number((50.0, (200.0 + 432.0 * 0.9) / 1280.0 * 100.0)).is_err(), "frame {seq}: in the surround");
            }
        }
    }
}
