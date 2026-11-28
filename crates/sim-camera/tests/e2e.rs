//! End to end on the host, no device (#327): every synthetic source kind, and a `device:` source
//! over a fake screen backend, through the path an iOS simulator's camera reads: the source, the
//! hub, the camera feed's Unix socket, and a reader speaking the capture shim's protocol
//! ([`FeedReader`]: the 20-byte hello, then 32-byte frame headers and BGRA pixels, as
//! `shims/ios-camera/hooks.m` reads them). A simulator's back and front cameras share its one feed
//! (one socket per device), so each check runs a back and a front reader on it, at different sizes,
//! at the same time (a thread each): the feed drops a reader that stops reading for its 2 s write
//! timeout, so neither camera may wait while the other is checked.
//!
//! The checks are structural, never timed (a loaded CI runner breaks timing thresholds; every wait
//! below is only a guard against a hang):
//! - the test card's frame counter, decoded from the pixels ([`testcard_seq`]), advances, and is the
//!   frame number the header carries: the source's frame numbers and timestamps reach the reader;
//! - each header's frame number and timestamp increase frame after frame, within one link
//!   generation;
//! - a relink mid-stream (`Feed::set_source`) moves every reader to the new source: frames go
//!   old source, then new source, never back, and the generation never goes down;
//! - a `video:` file (a clip the test writes; macOS only, where `video:` plays through
//!   AVFoundation) shows, in every frame, exactly the clip frame due at the frame's timestamp, and
//!   loops.
//!
//! A host camera (a webcam or a virtual camera) runs only on a host that has one, named by
//! `SIMEE_E2E_CAMERA=<id>`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use simee_camera::device::{DeviceSource, Screen};
use simee_camera::feed::{Feed, FeedReader, FrameHeader, Hello};
use simee_camera::synth::{self, testcard_seq};
use simee_camera::{CameraFrame, FrameSpec, HostOpener, Hub, Opener, Pattern, Result, Source, SourceSpec, Tick};

#[cfg(target_os = "macos")]
#[path = "support/mov.rs"]
mod mov;

/// A guard against a hang, never a threshold: every loop below ends as soon as it has what it
/// checks.
const GUARD: Duration = Duration::from_secs(30);

/// Frames each reader checks per source.
const FRAMES: usize = 8;

/// The fake devices' screen counters start here (device A) and here (device B), far from any
/// capture's own frame numbers, so a frame tells which device it shows.
const DEVICE_A: u64 = 500_000;
const DEVICE_B: u64 = 2_000_000;
/// How far a fake screen's counter is trusted to run in one test.
const SPAN: u64 = 1_000_000;

/// A fake device's screen size: the readers' aspect (4:3), so filling a camera frame crops none of
/// the counter strip.
const SCREEN: (u32, u32) = (640, 480);

/// A short socket path in /tmp: the lane scratchpad or a long TMPDIR is over `sun_path`'s 104 bytes.
fn sock(tag: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!("simee-e2e-{tag}-{}.sock", std::process::id()))
}

/// The server's opener with a fake screen backend: `device:<name>` is a [`DeviceSource`] over a
/// [`Screen`] a thread posts test cards to (as `sim-api`'s readers post a device's frames), counting
/// from [`DEVICE_A`] for `device:A` and [`DEVICE_B`] for any other; everything else opens as
/// [`HostOpener`] opens it.
struct FakeDevices(HostOpener);

impl Opener for FakeDevices {
    fn open(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Box<dyn Source>> {
        let SourceSpec::Device(d) = source else { return self.0.open(source, spec) };
        let screen = Arc::new(Screen::default());
        let from = if d.device == "A" { DEVICE_A } else { DEVICE_B };
        let poster = Poster::start(screen.clone(), from);
        Ok(Box::new(DeviceSource::new(&d.device, d.region, screen).reading(Box::new(poster))))
    }
}

/// Posts a test card numbered `from`, `from + 1`, ... to a screen until dropped.
struct Poster {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Poster {
    fn start(screen: Arc<Screen>, from: u64) -> Poster {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            let mut card = synth::open(&SourceSpec::Pattern(Pattern::Testcard)).expect("test card");
            let spec = FrameSpec::new(SCREEN.0, SCREEN.1, 60).expect("spec");
            let mut n = from;
            while !flag.load(Ordering::SeqCst) {
                let tick = Tick { seq: n, pts: Duration::ZERO, wall: SystemTime::now() };
                let bgra = card.frame(spec, tick).expect("card").bgra;
                let rgba: Vec<u8> = bgra.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
                screen.post(SCREEN.0, SCREEN.1, SCREEN, rgba);
                n += 1;
                thread::sleep(spec.interval());
            }
        });
        Poster { stop, thread: Some(thread) }
    }
}

impl Drop for Poster {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The server's side: a hub over the fake device backend and a feed on a short socket.
fn serve(tag: &str, source: SourceSpec) -> (Arc<Hub>, Feed) {
    let hub = Arc::new(Hub::new(FakeDevices(HostOpener::new())));
    let path = sock(tag);
    let _ = std::fs::remove_file(&path);
    let feed = Feed::serve(hub.clone(), path, source, FrameSpec::default()).expect("serve the feed");
    (hub, feed)
}

/// A simulator's back and front cameras: two shim sessions on the device's one feed, each at its
/// own size and rate. Both are 4:3, as the fake screens are, so no edge of the counter strip is
/// cropped, and their widths are multiples of 32, so the strip's cells scale whole. The front's
/// size is no simple fraction of the back's, so a still fitted from the back's frame (instead of
/// drawn at the front's size) differs from the one drawn.
const BACK: Hello = Hello { width: 320, height: 240, fps: 30 };
const FRONT: Hello = Hello { width: 224, height: 168, fps: 15 };

struct Camera {
    name: &'static str,
    reader: FeedReader,
}

fn cameras(feed: &Feed) -> [Camera; 2] {
    let open = |name, hello| Camera { name, reader: FeedReader::connect(feed.path(), hello).expect("connect to the feed") };
    [open("back", BACK), open("front", FRONT)]
}

/// Runs `check` on both cameras at once, a thread each, as the shim's two sessions read: the feed
/// drops a reader that stops reading for its write timeout, so neither may wait for the other.
/// A failure on either camera fails the test with that camera's message.
fn on_both(cams: &mut [Camera; 2], check: impl Fn(&mut Camera) + Sync) {
    thread::scope(|s| {
        let check = &check;
        let running: Vec<_> = cams.iter_mut().map(|cam| s.spawn(move || check(cam))).collect();
        for r in running {
            if let Err(panic) = r.join() {
                std::panic::resume_unwind(panic);
            }
        }
    });
}

/// One frame as the shim gets it: its header, and its pixels as a frame (`tick.seq` from the header).
struct Got {
    header: FrameHeader,
    frame: CameraFrame,
}

fn next(cam: &mut Camera) -> Got {
    let (header, bgra) = cam.reader.next(GUARD).unwrap_or_else(|e| panic!("{} camera: {e}", cam.name));
    let tick = Tick { seq: header.seq, pts: Duration::from_nanos(header.pts_ns), wall: SystemTime::UNIX_EPOCH };
    Got { header, frame: CameraFrame { width: header.width, height: header.height, bgra: bgra.into(), tick } }
}

/// `n` frames of one generation in a row (skipping older generations still in flight), checking
/// that frame numbers and timestamps increase from each to the next.
fn frames(cam: &mut Camera, generation: u32, n: usize) -> Vec<Got> {
    let mut out: Vec<Got> = Vec::new();
    let started = Instant::now();
    while out.len() < n {
        assert!(started.elapsed() < GUARD, "{} camera: no {n} frames of generation {generation}", cam.name);
        let got = next(cam);
        assert!(got.header.generation <= generation, "{} camera: generation {} after {generation}", cam.name, got.header.generation);
        if got.header.generation < generation {
            assert!(out.is_empty(), "{} camera: an older generation after the new one", cam.name);
            continue;
        }
        if let Some(prev) = out.last() {
            assert!(got.header.seq > prev.header.seq, "{} camera: frame number {} after {}", cam.name, got.header.seq, prev.header.seq);
            assert!(
                got.header.pts_ns > prev.header.pts_ns,
                "{} camera: timestamp {} ns after {} ns",
                cam.name,
                got.header.pts_ns,
                prev.header.pts_ns
            );
        }
        out.push(got);
    }
    out
}

/// What a frame shows, told from its pixels alone.
#[derive(Debug, PartialEq, Eq)]
enum Shows {
    /// The test card, numbered with the frame number its header carries.
    Testcard,
    /// A device's screen: device A's or B's test card, by its counter.
    DeviceA(u32),
    DeviceB(u32),
    /// Exactly the still `expected` draws at the frame's size.
    Still,
    Black,
    Other,
}

/// The test card's colour bars, where `synth`'s own test samples them: a frame without them isn't a
/// test card, whatever its top strip reads (a black frame's strip reads 0).
fn has_bars(f: &CameraFrame) -> bool {
    let y = f.height / 3;
    synth::BARS.iter().enumerate().all(|(i, c)| f.pixel((f.width / 7) * i as u32 + f.width / 14, y) == c.bgra())
}

/// The fixture PNG's four quadrants, checked against the file's colours themselves (not only
/// against what the image source draws): red, green / blue, yellow.
fn quadrants_ok(f: &CameraFrame) -> bool {
    let (w, h) = (f.width, f.height);
    [
        (w / 4, h / 4, [0, 0, 255, 255]),
        (3 * w / 4, h / 4, [0, 255, 0, 255]),
        (w / 4, 3 * h / 4, [255, 0, 0, 255]),
        (3 * w / 4, 3 * h / 4, [0, 255, 255, 255]),
    ]
    .iter()
    .all(|&(x, y, want)| f.pixel(x, y) == want)
}

fn shows(got: &Got, still: Option<&SourceSpec>) -> Shows {
    if let Some(still) = still {
        let spec = FrameSpec::new(got.frame.width, got.frame.height, 30).expect("spec");
        let want = synth::open(still).expect("still").frame(spec, Tick::start()).expect("frame");
        if want.bgra == got.frame.bgra {
            return Shows::Still;
        }
    }
    if got.frame.bgra.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]) {
        return Shows::Black;
    }
    if !has_bars(&got.frame) {
        return Shows::Other;
    }
    match testcard_seq(&got.frame) {
        Some(n) if (DEVICE_B..DEVICE_B + SPAN).contains(&u64::from(n)) => Shows::DeviceB(n),
        Some(n) if (DEVICE_A..DEVICE_A + SPAN).contains(&u64::from(n)) => Shows::DeviceA(n),
        Some(n) if n == got.header.seq as u32 => Shows::Testcard,
        _ => Shows::Other,
    }
}

#[test]
fn testcard_counter_advances_and_carries_the_frame_number_and_timestamp_on_both_cameras() {
    let (hub, feed) = serve("card", SourceSpec::Pattern(Pattern::Testcard));
    let mut cams = cameras(&feed);
    on_both(&mut cams, |cam| {
        let got = frames(cam, 0, FRAMES);
        let want = if cam.name == "back" { BACK } else { FRONT };
        for g in &got {
            assert_eq!((g.header.width, g.header.height), (want.width, want.height), "{} camera: at the size it asked for", cam.name);
            // The counter the source drew is the frame number the header carries: the source's
            // frame number reaches the reader with its pixels.
            assert_eq!(testcard_seq(&g.frame), Some(g.header.seq as u32), "{} camera: the counter drawn is the frame's number", cam.name);
        }
        let (first, last) = (testcard_seq(&got[0].frame), testcard_seq(&got[FRAMES - 1].frame));
        assert!(last > first, "{} camera: the counter advances ({first:?} to {last:?})", cam.name);
    });
    let active = hub.active();
    assert_eq!(active.len(), 1, "one capture for both cameras: {:?}", active.iter().map(|a| &a.source).collect::<Vec<_>>());
    assert_eq!(active[0].subscribers, 2);
}

#[test]
fn qr_frames_are_the_code_pixel_for_pixel_on_both_cameras() {
    let qr = SourceSpec::Qr("simee-e2e-327".into());
    let (_hub, feed) = serve("qr", qr.clone());
    on_both(&mut cameras(&feed), |cam| {
        for g in frames(cam, 0, FRAMES) {
            assert_eq!(
                shows(&g, Some(&qr)),
                Shows::Still,
                "{} camera: the QR code as drawn at {}x{}",
                cam.name,
                g.header.width,
                g.header.height
            );
        }
    });
}

/// A file the test wrote, removed when dropped (a failing test too).
struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A 4x4 PNG of four colour quadrants, lossless, so a frame of it can be compared exactly.
fn png_fixture(tag: &str) -> Fixture {
    let path = PathBuf::from("/tmp").join(format!("simee-e2e-{tag}-{}.png", std::process::id()));
    let mut rgba = Vec::new();
    for y in 0..4 {
        for x in 0..4 {
            rgba.extend_from_slice(match (x < 2, y < 2) {
                (true, true) => &[255, 0, 0, 255],
                (false, true) => &[0, 255, 0, 255],
                (true, false) => &[0, 0, 255, 255],
                (false, false) => &[255, 255, 0, 255],
            });
        }
    }
    let fixture = Fixture(path);
    std::fs::write(&fixture.0, sim_core::frame::encode_rgba(4, 4, &rgba).expect("encode the PNG")).expect("write the PNG");
    fixture
}

#[test]
fn image_frames_are_the_file_on_both_cameras() {
    let fixture = png_fixture("image");
    let png = SourceSpec::Image(fixture.0.clone());
    let (_hub, feed) = serve("image", png.clone());
    on_both(&mut cameras(&feed), |cam| {
        for g in frames(cam, 0, FRAMES) {
            assert_eq!(
                shows(&g, Some(&png)),
                Shows::Still,
                "{} camera: the PNG as drawn at {}x{}",
                cam.name,
                g.header.width,
                g.header.height
            );
            assert!(quadrants_ok(&g.frame), "{} camera: the PNG's own colours", cam.name);
        }
    });
    drop(feed);
    // A JPEG decodes too (lossy: compared by colour, not byte for byte).
    let jpeg = SourceSpec::Image(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/red-16x16.jpg"));
    let (_hub, feed) = serve("jpeg", jpeg);
    on_both(&mut cameras(&feed), |cam| {
        let g = frames(cam, 0, 2).pop().expect("a frame");
        let [b, gr, r, _] = g.frame.pixel(g.frame.width / 2, g.frame.height / 2);
        assert!(r > 230 && gr < 30 && b < 30, "{} camera: red, within JPEG error: {r},{gr},{b}", cam.name);
    });
}

#[test]
fn a_device_screen_through_a_fake_backend_reaches_both_cameras() {
    let (_hub, feed) = serve("device", SourceSpec::parse("device:A").expect("device:A"));
    on_both(&mut cameras(&feed), |cam| {
        let got = frames(cam, 0, FRAMES);
        let counters: Vec<u32> = got
            .iter()
            .map(|g| match shows(g, None) {
                Shows::DeviceA(n) => n,
                other => panic!("{} camera: device A's screen, not {other:?}", cam.name),
            })
            .collect();
        // The device paces the capture: a still screen repeats its frame, a new one moves it on.
        assert!(counters.windows(2).all(|w| w[1] >= w[0]), "{} camera: A's screen counter never goes back: {counters:?}", cam.name);
        assert!(counters[FRAMES - 1] > counters[0], "{} camera: A's screen counter advances: {counters:?}", cam.name);
    });
}

/// A check of one frame.
type Check<'a> = &'a (dyn Fn(&Got) -> bool + Sync);

/// Reads frames until `generation`'s source shows, checking that each frame before it is the
/// previous source's; then [`FRAMES`] frames of the new one.
fn swap_to(cam: &mut Camera, generation: u32, old: Check, new: Check) {
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < GUARD, "{} camera: generation {generation} never came", cam.name);
        let g = next(cam);
        if g.header.generation == generation {
            assert!(new(&g), "{} camera: generation {generation}'s first frame is the new source's", cam.name);
            break;
        }
        assert!(g.header.generation < generation, "{} camera: generation {} after asking for {generation}", cam.name, g.header.generation);
        assert!(old(&g), "{} camera: a frame before the swap is the old source's", cam.name);
    }
    for g in frames(cam, generation, FRAMES) {
        assert!(new(&g), "{} camera: after the swap, only the new source's frames", cam.name);
    }
}

#[test]
fn a_swap_mid_stream_changes_the_source_on_both_cameras() {
    let qr = SourceSpec::Qr("swapped".into());
    let fixture = png_fixture("swap");
    let png = SourceSpec::Image(fixture.0.clone());
    let (_hub, feed) = serve("swap", SourceSpec::Pattern(Pattern::Testcard));
    let mut cams = cameras(&feed);
    on_both(&mut cams, |cam| {
        assert!(frames(cam, 0, FRAMES).iter().all(|g| shows(g, None) == Shows::Testcard), "{} camera: the test card", cam.name);
    });
    let testcard = |g: &Got| shows(g, None) == Shows::Testcard;
    let is_qr = |g: &Got| shows(g, Some(&qr)) == Shows::Still;
    let is_a = |g: &Got| matches!(shows(g, None), Shows::DeviceA(_));
    let is_b = |g: &Got| matches!(shows(g, None), Shows::DeviceB(_));
    let is_png = |g: &Got| shows(g, Some(&png)) == Shows::Still;
    let steps: [(SourceSpec, Check, Check); 4] = [
        (qr.clone(), &testcard, &is_qr),
        (SourceSpec::parse("device:A").expect("A"), &is_qr, &is_a),
        (SourceSpec::parse("device:B").expect("B"), &is_a, &is_b),
        (png.clone(), &is_b, &is_png),
    ];
    for (source, old, new) in steps {
        let generation = feed.set_source(source.clone());
        assert_eq!(feed.source(), source);
        on_both(&mut cams, |cam| swap_to(cam, generation, old, new));
    }
}

/// A host camera (a webcam, or a virtual camera such as OBS's): only where `SIMEE_E2E_CAMERA`
/// names one (an id, a name or part of it, or an index, as `webcam:` takes), with the camera
/// permission granted to the terminal running the tests. Its pictures can't be checked, so this
/// checks the stream: frames at each camera's size, numbers and timestamps going up, and swaps to
/// the test card and back.
#[test]
fn a_host_camera_through_the_feed() {
    let Some(id) = std::env::var("SIMEE_E2E_CAMERA").ok().filter(|v| !v.is_empty()) else {
        eprintln!("skipped: set SIMEE_E2E_CAMERA=<camera id, name or index> on a host with a webcam or virtual camera to run it");
        return;
    };
    let webcam = SourceSpec::Webcam(id.clone());
    let (_hub, feed) = serve("webcam", webcam.clone());
    let mut cams = cameras(&feed);
    on_both(&mut cams, |cam| {
        let got = frames(cam, 0, FRAMES);
        assert!(got.iter().all(|g| shows(g, None) != Shows::Testcard), "{} camera: webcam:{id}'s picture, not a stand-in", cam.name);
    });
    let generation = feed.set_source(SourceSpec::Pattern(Pattern::Testcard));
    let not_card = |g: &Got| shows(g, None) != Shows::Testcard;
    let card = |g: &Got| shows(g, None) == Shows::Testcard;
    on_both(&mut cams, |cam| swap_to(cam, generation, &not_card, &card));
    let generation = feed.set_source(webcam);
    on_both(&mut cams, |cam| swap_to(cam, generation, &card, &not_card));
}

/// A `video:` file through the feed (macOS, where it plays): a 4-frame, 10 fps clip the test
/// writes, uncompressed so every frame decodes to exactly its pixels. Every frame either camera
/// gets is exactly the clip frame due at the frame's own timestamp (frame `pts / 100 ms`, modulo
/// 4), drawn at the camera's size; reading on until the timestamps pass two whole passes shows the
/// file looping.
#[cfg(target_os = "macos")]
#[test]
fn a_video_file_plays_at_its_own_rate_and_loops_on_both_cameras() {
    let clip = mov::write("simee-e2e-video");
    let (_hub, feed) = serve("video", SourceSpec::Video(clip.0.clone()));
    let pass = mov::interval_ns() * mov::COLOURS.len() as u64;
    on_both(&mut cameras(&feed), |cam| {
        let started = Instant::now();
        let mut prev: Option<FrameHeader> = None;
        loop {
            assert!(started.elapsed() < GUARD, "{} camera: the clip never played through twice", cam.name);
            let g = next(cam);
            let h = g.header;
            assert_eq!(h.generation, 0, "{} camera: one link", cam.name);
            if let Some(p) = prev {
                assert!(
                    h.seq > p.seq && h.pts_ns > p.pts_ns,
                    "{} camera: frame {} at {} ns after {} at {} ns",
                    cam.name,
                    h.seq,
                    h.pts_ns,
                    p.seq,
                    p.pts_ns
                );
            }
            let due = (h.pts_ns / mov::interval_ns()) as usize % mov::COLOURS.len();
            assert_eq!(
                mov::which(&g.frame),
                Some(due),
                "{} camera: the clip frame due at {} ns, at {}x{}",
                cam.name,
                h.pts_ns,
                h.width,
                h.height
            );
            prev = Some(h);
            if h.pts_ns >= 2 * pass {
                break;
            }
        }
    });
}

/// A `video:` file that isn't there: an I/O error naming it on macOS, where `video:` plays; and
/// Unsupported anywhere else, where no `video:` plays.
#[test]
fn a_missing_video_file_is_an_io_error_on_macos_and_unsupported_elsewhere() {
    let path = PathBuf::from("/tmp").join(format!("simee-e2e-missing-{}.mov", std::process::id()));
    let err = synth::open(&SourceSpec::Video(path)).err();
    #[cfg(target_os = "macos")]
    assert!(matches!(&err, Some(simee_camera::Error::Io(m)) if m.contains("simee-e2e-missing")), "{err:?}");
    #[cfg(not(target_os = "macos"))]
    assert!(matches!(&err, Some(simee_camera::Error::Unsupported(m)) if m.contains("macOS")), "{err:?}");
}

/// Frames of a capture that a relink left behind never reach a reader again: a source dropped by
/// every reader stops (the hub holds nothing for it).
#[test]
fn a_swap_stops_the_old_capture() {
    let (hub, feed) = serve("stops", SourceSpec::Pattern(Pattern::Testcard));
    let mut cams = cameras(&feed);
    on_both(&mut cams, |cam| {
        let _ = frames(cam, 0, 2);
    });
    let qr = SourceSpec::Qr("after".into());
    let generation = feed.set_source(qr.clone());
    let is_qr = |g: &Got| shows(g, Some(&qr)) == Shows::Still;
    let any = |_: &Got| true;
    on_both(&mut cams, |cam| swap_to(cam, generation, &any, &is_qr));
    // The readers keep draining while the old capture winds down: the feed drops a reader that
    // stops reading for its write timeout, which would end the new capture too. No wall-clock
    // bound beyond the suite's own GUARD.
    let done = AtomicBool::new(false);
    thread::scope(|s| {
        let done = &done;
        for cam in cams.iter_mut() {
            s.spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let _ = next(cam);
                }
            });
        }
        let started = Instant::now();
        let stopped = loop {
            let now: Vec<String> = hub.active().into_iter().map(|a| a.source).collect();
            if now == [qr.to_string()] {
                break Ok(());
            }
            if started.elapsed() >= GUARD {
                break Err(format!("the test card's capture is still running: {now:?}"));
            }
            thread::sleep(Duration::from_millis(10));
        };
        done.store(true, Ordering::Relaxed);
        if let Err(m) = stopped {
            panic!("{m}");
        }
    });
}
