//! End to end on the host, no VM (#327): every synthetic source kind, and a `device:` source over
//! a fake screen backend, through the path an Android guest's camera reads: the source, emu's
//! camera hub, and the goldfish camera HAL's pipe service (`qemud:camera:name=simee-back` /
//! `simee-front`), driven query by query as the guest's HAL drives it. The back camera is read the
//! libhardware way (API 30–33: `frame video=<NV21 bytes> preview=<RGB32 bytes>`, the frame in the
//! reply), the front the AIDL way (API 34+: a bare `start`, then `frame dim=WxH pix=<fourcc>
//! offset=<n>` into the goldfish address-space area), so both HALs are covered.
//!
//! A `device:` source reaches an Android camera as the simee server serves it: a camera feed on a
//! Unix socket that emu reads as `feed:<socket>`. Here the server side is a hub over a fake screen
//! backend (test cards posted to a `Screen`, as `sim-api`'s readers post a device's frames) and a
//! `Feed`, and emu's camera is linked to `feed:<socket>`, one socket per camera as the server
//! serves them.
//!
//! The checks are structural, never timed (a loaded CI runner breaks timing thresholds; every wait
//! below only guards against a hang):
//! - the test card's frame counter, decoded from the guest's frame, advances and never goes back.
//!   The HAL protocol carries no timestamp (the guest's HAL stamps frames with its own clock), so
//!   the counter in the pixels is the source's timestamp as the guest gets it;
//! - QR codes and images reach the guest exactly as the source draws them;
//! - the back camera's NV21 frame (what camera apps record) is its RGB32 preview's frame, by
//!   independently computed luma;
//! - a relink mid-stream (`Cameras::link`, and a server-side relink of a served feed) changes the
//!   frames' source: the old source's frames, then the new one's, never back; the other camera
//!   keeps its own source;
//! - a `video:` file (a clip the test writes; macOS only, where `video:` plays) shows only the
//!   clip's frames, every one of them, and loops.
//!
//! emu stands in for a source that has no frame yet, and the tests accept exactly those stand-ins
//! before a source's first frame, never after it: black while a fresh link waits for its first
//! frame (`FIRST_FRAME`, 500 ms), and its test card once a feed has sent nothing for its stall time
//! (1 s; a feed relinked server-side to another device does that while the device opens).
//!
//! A host camera runs only where `SIMEE_E2E_CAMERA=<id>` names one.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use emu_hvf::GuestRam;
use sim_camera::device::{DeviceSource, Screen};
use sim_camera::feed::Feed;
use sim_camera::synth::{self, testcard_seq};
use sim_camera::{CameraFrame, FrameSpec, HostOpener, Hub, Opener, Pattern, Source, SourceSpec, Tick};
use sim_core::yuv::{self, CameraFormat};
use simee_emu_vmm::pipe::camera::{CameraService, Cameras, Facing};
use simee_emu_vmm::pipe::{HostMsg, PipeCtx, PipeHandler, PipeService, WorkerEvent};

#[cfg(target_os = "macos")]
#[path = "../../sim-camera/tests/support/mov.rs"]
mod mov;

/// A guard against a hang, never a threshold.
const GUARD: Duration = Duration::from_secs(30);

/// Distinct frames each camera checks per source.
const FRAMES: usize = 6;

/// The fake devices' screen counters start here, far from any capture's own frame numbers.
const DEVICE_A: u64 = 500_000;
const DEVICE_B: u64 = 2_000_000;
const SPAN: u64 = 1_000_000;

/// A fake device's screen: 4:3, as the cameras below are, so no edge of the counter strip is cropped.
const SCREEN: (u32, u32) = (640, 480);

/// The cameras' sizes (both in `list`'s framedims).
const BACK: (u32, u32) = (320, 240);
const FRONT: (u32, u32) = (640, 480);

/// Where the front camera's frames go in the address-space area.
const AREA_BASE: u64 = 0x1_0000_0000;
const OFFSET: u64 = 0x1000;

fn sock(tag: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!("emu-e2e-{tag}-{}.sock", std::process::id()))
}

/// The simee server's side of a `device:` link: `device:<name>` a [`DeviceSource`] over a fake
/// screen (device A's counter from [`DEVICE_A`], any other's from [`DEVICE_B`]); the rest as
/// [`HostOpener`] opens it.
struct FakeDevices(HostOpener);

impl Opener for FakeDevices {
    fn open(&self, source: &SourceSpec, spec: FrameSpec) -> sim_camera::Result<Box<dyn Source>> {
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
                let bgra = card.frame(spec, Tick { seq: n, pts: Duration::ZERO, wall: SystemTime::now() }).expect("card").bgra;
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

/// The server serving one camera's `device:` link on its own socket.
fn serve(tag: &str, source: &str) -> Feed {
    let path = sock(tag);
    let _ = std::fs::remove_file(&path);
    let hub = Arc::new(Hub::new(FakeDevices(HostOpener::new())));
    Feed::serve(hub, path, SourceSpec::parse(source).expect("source"), FrameSpec::default()).expect("serve the feed")
}

fn feed_link(feed: &Feed) -> SourceSpec {
    SourceSpec::parse(&format!("feed:{}", feed.path().display())).expect("feed link")
}

/// One of the guest's camera sessions, driven through the pipe service as the guest's HAL drives
/// it: each query through the session's thread, its reply taken by the guest before the next.
struct Guest {
    facing: Facing,
    pipe: Box<dyn PipeHandler>,
    rx: Receiver<WorkerEvent>,
    /// The front camera's frames land here (the AIDL HAL's gralloc buffers).
    area: Arc<GuestRam>,
}

impl Guest {
    fn open(svc: &CameraService, facing: Facing, area: Arc<GuestRam>, conn: u64) -> Guest {
        let (tx, rx) = mpsc::channel();
        let pipe =
            svc.open(&format!("qemud:camera:name={}", facing.device_name()), PipeCtx { conn, events: tx }).expect("open the session");
        let mut g = Guest { facing, pipe, rx, area };
        assert_eq!(g.ask("connect"), b"ok\0");
        let start = match facing {
            Facing::Back => format!("start dim={}x{} pix={}", BACK.0, BACK.1, CameraFormat::Nv21.fourcc()),
            Facing::Front => "start".to_string(),
        };
        assert_eq!(g.ask(&start), b"ok\0");
        g
    }

    /// One query's reply payload.
    fn ask(&mut self, q: &str) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(self.pipe.guest_data(format!("{q}\0").as_bytes(), &mut out), "the pipe stays open");
        if out.is_empty() {
            let started = Instant::now();
            loop {
                match self.rx.recv_timeout(GUARD) {
                    Ok(WorkerEvent::Host { msg: HostMsg::Data(d), .. }) => {
                        assert!(self.pipe.host_msg(HostMsg::Data(d), &mut out));
                        break;
                    }
                    Ok(_) => assert!(started.elapsed() < GUARD, "no reply to {q}"),
                    Err(e) => panic!("{} camera: no reply to {q}: {e}", self.facing.dir()),
                }
            }
        }
        self.pipe.guest_took(out.len());
        let len = usize::from_str_radix(std::str::from_utf8(&out[..8]).expect("hex length"), 16).expect("length");
        assert_eq!(out.len(), 8 + len, "the length prefix");
        out.split_off(8)
    }

    /// The guest's next frame, as BGRA: the back camera's RGB32 preview from the reply (its NV21
    /// frame checked to be the same frame), the front camera's RGB32 frame from the address-space
    /// area.
    fn frame(&mut self) -> CameraFrame {
        let (rgba, (w, h)) = match self.facing {
            Facing::Back => {
                let (w, h) = (BACK.0 as usize, BACK.1 as usize);
                let video = CameraFormat::Nv21.frame_size(w, h);
                let preview = CameraFormat::Rgb32.frame_size(w, h);
                let r = self.ask(&format!("frame video={video} preview={preview} whiteb=1,1,1 expcomp=1 time=0"));
                assert_eq!(&r[..3], b"ok:", "back camera: {}", String::from_utf8_lossy(&r[..r.len().min(40)]));
                assert_eq!(r.len(), 3 + video + preview);
                let (nv21, rgb32) = r[3..].split_at(video);
                nv21_is(nv21, rgb32, w, h);
                (rgb32.to_vec(), BACK)
            }
            Facing::Front => {
                let q = format!(
                    "frame dim={}x{} pix={} offset={OFFSET} whiteb=1,1,1 expcomp=1 time=0",
                    FRONT.0,
                    FRONT.1,
                    CameraFormat::Rgb32.fourcc()
                );
                assert_eq!(self.ask(&q), b"ok\0", "front camera: an offset frame");
                let mut px = vec![0u8; CameraFormat::Rgb32.frame_size(FRONT.0 as usize, FRONT.1 as usize)];
                self.area.read(AREA_BASE + OFFSET, &mut px).expect("read the area");
                (px, FRONT)
            }
        };
        let bgra: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        CameraFrame { width: w, height: h, bgra: bgra.into(), tick: Tick::start() }
    }

    /// The next frame that differs from `last` (the HAL answers the newest frame again when the
    /// guest asks faster than the source draws), or the same one once [`GUARD`] has passed: the
    /// caller decides whether a frame that doesn't change is wrong.
    fn new_frame(&mut self, last: Option<&CameraFrame>) -> CameraFrame {
        let deadline = Instant::now() + GUARD;
        loop {
            let f = self.frame();
            if last.is_none_or(|l| l.bgra != f.bgra) || Instant::now() >= deadline {
                return f;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The back camera's NV21 frame (`video=`, what a camera app records) holds the same frame as its
/// RGB32 preview: every luma sample is the preview pixel's BT.601 limited-range luma, computed here
/// independently of `sim-core`'s converter (to within its integer rounding), and the whole frame,
/// chroma too, is exactly that converter's NV21 of the preview.
fn nv21_is(nv21: &[u8], rgb32: &[u8], w: usize, h: usize) {
    let (luma, _) = nv21.split_at(w * h);
    for (i, (&y, p)) in luma.iter().zip(rgb32.as_chunks::<4>().0).enumerate() {
        let [r, g, b] = [p[0], p[1], p[2]].map(f64::from);
        let want = 16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0;
        assert!((f64::from(y) - want).abs() <= 1.5, "back camera: NV21 luma {y} at ({}, {}), the preview's is {want:.1}", i % w, i / w);
    }
    let bgra: Vec<u8> = rgb32.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    assert!(
        yuv::convert(&bgra, w, h, CameraFormat::Nv21).as_deref() == Some(nv21),
        "back camera: the NV21 frame is the preview's, chroma too"
    );
}

/// What a guest frame shows, told from its pixels alone.
#[derive(Debug, PartialEq, Eq)]
enum Shows {
    /// A test card (its counter), drawn by emu.
    Testcard(u32),
    /// A device's test card screen, by its counter's range.
    DeviceA(u32),
    DeviceB(u32),
    Black,
    /// Anything else: a QR code, an image, a solid colour, a camera's picture.
    Other,
}

/// The test card's colour bars, where `synth`'s own test samples them: a frame without them isn't a
/// test card, whatever its top strip reads (a black frame's strip reads 0).
fn has_bars(f: &CameraFrame) -> bool {
    let y = f.height / 3;
    synth::BARS.iter().enumerate().all(|(i, c)| f.pixel((f.width / 7) * i as u32 + f.width / 14, y) == c.bgra())
}

fn shows(f: &CameraFrame) -> Shows {
    if f.bgra.as_chunks::<4>().0.iter().all(|p| p[..3] == [0, 0, 0]) {
        return Shows::Black;
    }
    if !has_bars(f) {
        return Shows::Other;
    }
    match testcard_seq(f).map(u64::from) {
        Some(n) if (DEVICE_B..DEVICE_B + SPAN).contains(&n) => Shows::DeviceB(n as u32),
        Some(n) if (DEVICE_A..DEVICE_A + SPAN).contains(&n) => Shows::DeviceA(n as u32),
        Some(n) => Shows::Testcard(n as u32),
        None => Shows::Other,
    }
}

/// A check of one frame, and whether its source is a still (one frame shows it).
struct Is<'a> {
    name: String,
    check: Box<dyn Fn(&CameraFrame) -> bool + 'a>,
    still: bool,
}

fn card() -> Is<'static> {
    Is { name: "emu's test card".into(), check: Box::new(|f| matches!(shows(f), Shows::Testcard(_))), still: false }
}

fn device_a() -> Is<'static> {
    Is { name: "device A's screen".into(), check: Box::new(|f| matches!(shows(f), Shows::DeviceA(_))), still: false }
}

fn device_b() -> Is<'static> {
    Is { name: "device B's screen".into(), check: Box::new(|f| matches!(shows(f), Shows::DeviceB(_))), still: false }
}

/// Exactly what `source` draws at the frame's size (and, for the fixture PNG, its own colours).
fn still(source: &SourceSpec) -> Is<'_> {
    Is {
        name: source.to_string(),
        check: Box::new(move |f| {
            let spec = FrameSpec::new(f.width, f.height, 30).expect("spec");
            let drawn = synth::open(source).expect("still").frame(spec, Tick::start()).expect("frame");
            drawn.bgra == f.bgra && (!matches!(source, SourceSpec::Image(_)) || quadrants_ok(f))
        }),
        still: true,
    }
}

/// The fixture PNG's quadrants, checked against the file's own colours: red, green / blue, yellow.
fn quadrants_ok(f: &CameraFrame) -> bool {
    let (w, h) = (f.width, f.height);
    let want = [(w / 4, h / 4, [0, 0, 255, 255]), (3 * w / 4, h / 4, [0, 255, 0, 255]), (w / 4, 3 * h / 4, [255, 0, 0, 255])];
    want.iter().chain(&[(3 * w / 4, 3 * h / 4, [0, 255, 255, 255])]).all(|&(x, y, px)| f.pixel(x, y) == px)
}

/// What emu shows for a source that has no frame yet: black (a fresh link's first 500 ms) or its
/// test card (a feed silent for its 1 s stall time).
fn stand_in(f: &CameraFrame) -> bool {
    matches!(shows(f), Shows::Black | Shows::Testcard(_))
}

/// Nothing before a camera's first source: only emu's stand-ins may come first.
fn nothing() -> Is<'static> {
    Is { name: "nothing yet".into(), check: Box::new(|_| false), still: false }
}

/// Reads [`FRAMES`] distinct frames (one, for a still), each of which `is` accepts; when they carry
/// a counter, it never goes back and it advances.
fn stream(g: &mut Guest, is: &Is) {
    let mut last: Option<CameraFrame> = None;
    let mut counters = Vec::new();
    for _ in 0..if is.still { 1 } else { FRAMES } {
        let f = g.new_frame(last.as_ref());
        assert!((is.check)(&f), "{} camera: {} expected, got {:?}", g.facing.dir(), is.name, shows(&f));
        if let Shows::Testcard(n) | Shows::DeviceA(n) | Shows::DeviceB(n) = shows(&f) {
            counters.push(n);
        }
        last = Some(f);
    }
    if !counters.is_empty() {
        assert!(counters.windows(2).all(|w| w[1] >= w[0]), "{} camera: the counter never goes back: {counters:?}", g.facing.dir());
        assert!(counters.last() > counters.first(), "{} camera: the counter advances: {counters:?}", g.facing.dir());
    }
}

/// Reads frames until `new`'s show, each one before it `old`'s or one of emu's stand-ins (until
/// [`GUARD`]: the new source's first frame may be late on a loaded host); then [`stream`]s the new
/// source, which allows no stand-in.
fn swap(g: &mut Guest, old: &Is, new: &Is) {
    let started = Instant::now();
    let mut last: Option<CameraFrame> = None;
    loop {
        let f = g.new_frame(last.as_ref());
        if (new.check)(&f) {
            break;
        }
        assert!(
            (old.check)(&f) || stand_in(&f),
            "{} camera: before {}, only {}'s frames or emu's stand-in: got {:?}",
            g.facing.dir(),
            new.name,
            old.name,
            shows(&f)
        );
        assert!(started.elapsed() < GUARD, "{} camera: {} never showed (still {:?})", g.facing.dir(), new.name, shows(&f));
        last = Some(f);
    }
    stream(g, new);
}

/// A camera's first source: emu's stand-ins until its first frame, then [`stream`].
fn first(g: &mut Guest, new: &Is) {
    swap(g, &nothing(), new);
}

fn cameras() -> (Arc<Cameras>, CameraService, Arc<GuestRam>) {
    let cameras = Arc::new(Cameras::default());
    let area = Arc::new(GuestRam::new(AREA_BASE, 4 << 20).expect("the address-space area"));
    let svc = CameraService::with(cameras.clone(), Some(area.clone()));
    (cameras, svc, area)
}

fn link(c: &Cameras, facing: Facing, source: &str) {
    c.link(facing, Some(SourceSpec::parse(source).expect("source")));
}

#[test]
fn testcard_counter_advances_on_both_cameras() {
    let (c, svc, area) = cameras();
    link(&c, Facing::Back, "pattern:testcard");
    link(&c, Facing::Front, "pattern:testcard");
    for (conn, facing) in [(1, Facing::Back), (2, Facing::Front)] {
        let mut g = Guest::open(&svc, facing, area.clone(), conn);
        first(&mut g, &card());
    }
}

#[test]
fn qr_and_image_reach_both_cameras_exactly() {
    let png = png_fixture("still");
    let qr = SourceSpec::Qr("simee-e2e-327".into());
    let image = SourceSpec::Image(png.0.clone());
    let (c, svc, area) = cameras();
    for (conn, facing) in [(1, Facing::Back), (2, Facing::Front)] {
        c.link(facing, Some(qr.clone()));
        let mut g = Guest::open(&svc, facing, area.clone(), conn);
        first(&mut g, &still(&qr));
        c.link(facing, Some(image.clone()));
        swap(&mut g, &still(&qr), &still(&image));
    }
}

#[test]
fn a_device_screen_reaches_both_cameras_through_the_server_feed() {
    let (c, svc, area) = cameras();
    let back = serve("dev-back", "device:A");
    let front = serve("dev-front", "device:B");
    c.link(Facing::Back, Some(feed_link(&back)));
    c.link(Facing::Front, Some(feed_link(&front)));
    let mut b = Guest::open(&svc, Facing::Back, area.clone(), 1);
    let mut f = Guest::open(&svc, Facing::Front, area, 2);
    // Until the feed's first frame the camera is black, or emu's test card stands in; then the
    // device's screen.
    first(&mut b, &device_a());
    first(&mut f, &device_b());
    assert_eq!((back.readers(), front.readers()), (1, 1), "one feed connection per camera");
}

#[test]
fn a_relink_mid_stream_changes_the_source_and_only_on_that_camera() {
    let png = png_fixture("relink");
    let qr = SourceSpec::Qr("relinked".into());
    let image = SourceSpec::Image(png.0.clone());
    let blue = SourceSpec::parse("solid:blue").expect("blue");
    let (c, svc, area) = cameras();
    link(&c, Facing::Back, "pattern:testcard");
    c.link(Facing::Front, Some(blue.clone()));
    let mut b = Guest::open(&svc, Facing::Back, area.clone(), 1);
    let mut f = Guest::open(&svc, Facing::Front, area, 2);
    first(&mut b, &card());
    first(&mut f, &still(&blue));

    // emu's own relink (the console's `camera link`): test card to QR code.
    c.link(Facing::Back, Some(qr.clone()));
    swap(&mut b, &card(), &still(&qr));
    // To another device's screen, served on a feed: until the feed's first frame, the last QR
    // frame, or emu's stand-in.
    let feed = serve("relink", "device:A");
    c.link(Facing::Back, Some(feed_link(&feed)));
    swap(&mut b, &still(&qr), &device_a());
    // The server relinks the feed (a `camera_link` to another device): emu's link stays the feed,
    // which shows A's last frame, or the test card if B takes over 1 s to open, until B's.
    feed.set_source(SourceSpec::parse("device:B").expect("B"));
    swap(&mut b, &device_a(), &device_b());
    // And to a still.
    c.link(Facing::Back, Some(image.clone()));
    swap(&mut b, &device_b(), &still(&image));

    // The front camera kept its own source throughout.
    stream(&mut f, &still(&blue));
}

/// A host camera, only where `SIMEE_E2E_CAMERA` names one (as `webcam:` takes it), with the camera
/// permission granted to the terminal running the tests. Its pictures can't be checked: this checks
/// that both cameras get its frames (not black, not emu's test card standing in for a source that
/// failed), and that relinking to the test card and back works mid-stream.
#[test]
fn a_host_camera_reaches_both_cameras() {
    let Some(id) = std::env::var("SIMEE_E2E_CAMERA").ok().filter(|v| !v.is_empty()) else {
        eprintln!("skipped: set SIMEE_E2E_CAMERA=<camera id, name or index> on a host with a webcam or virtual camera to run it");
        return;
    };
    let webcam = format!("webcam:{id}");
    let (c, svc, area) = cameras();
    link(&c, Facing::Back, &webcam);
    link(&c, Facing::Front, &webcam);
    // A camera's picture may hold still: one frame of it is enough.
    let picture = || Is { name: "the camera picture".into(), check: Box::new(|f| shows(f) == Shows::Other), still: true };
    for (conn, facing) in [(1, Facing::Back), (2, Facing::Front)] {
        let mut g = Guest::open(&svc, facing, area.clone(), conn);
        first(&mut g, &picture());
        link(&c, facing, "pattern:testcard");
        swap(&mut g, &picture(), &card());
        link(&c, facing, &webcam);
        swap(&mut g, &card(), &picture());
    }
}

/// A `video:` file on both cameras (macOS, where it plays): a 4-frame clip the test writes,
/// uncompressed so every frame decodes to its pixels. The HAL carries no timestamp, so this checks
/// what a guest can tell: after emu's stand-ins, every frame is one of the clip's, drawn at the
/// camera's size; every one of them shows; and an earlier one follows a later one (the file loops).
#[cfg(target_os = "macos")]
#[test]
fn a_video_file_plays_and_loops_on_both_cameras() {
    let clip = mov::write("emu-e2e-video");
    let (c, svc, area) = cameras();
    for (conn, facing) in [(1, Facing::Back), (2, Facing::Front)] {
        c.link(facing, Some(SourceSpec::Video(clip.0.clone())));
        let mut g = Guest::open(&svc, facing, area.clone(), conn);
        let clip_frame = Is { name: "a clip frame".into(), check: Box::new(|f| mov::which(f).is_some()), still: true };
        first(&mut g, &clip_frame);
        let (mut seen, mut looped) = ([false; mov::COLOURS.len()], false);
        let (mut last, mut prev): (Option<CameraFrame>, Option<usize>) = (None, None);
        let started = Instant::now();
        while !(looped && seen.iter().all(|&s| s)) {
            assert!(started.elapsed() < GUARD, "{} camera: the clip never played through (seen {seen:?}, looped {looped})", facing.dir());
            let f = g.new_frame(last.as_ref());
            let Some(i) = mov::which(&f) else { panic!("{} camera: only the clip's frames, got {:?}", facing.dir(), shows(&f)) };
            seen[i] = true;
            looped |= prev.is_some_and(|p| i < p);
            (prev, last) = (Some(i), Some(f));
        }
    }
}

/// A `video:` file that isn't there: an I/O error naming it on macOS, where `video:` plays; and
/// Unsupported anywhere else, where no `video:` plays.
#[test]
fn a_missing_video_file_is_an_io_error_on_macos_and_unsupported_elsewhere() {
    let path = PathBuf::from("/tmp").join(format!("emu-e2e-missing-{}.mov", std::process::id()));
    let err = synth::open(&SourceSpec::Video(path)).err();
    #[cfg(target_os = "macos")]
    assert!(matches!(&err, Some(sim_camera::Error::Io(m)) if m.contains("emu-e2e-missing")), "{err:?}");
    #[cfg(not(target_os = "macos"))]
    assert!(matches!(&err, Some(sim_camera::Error::Unsupported(m)) if m.contains("macOS")), "{err:?}");
}

/// A file the test wrote, removed when dropped (a failing test too).
struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A 4x4 PNG of four colour quadrants: lossless, so the guest's RGB32 frame can be compared exactly.
fn png_fixture(tag: &str) -> Fixture {
    let path = PathBuf::from("/tmp").join(format!("emu-e2e-{tag}-{}.png", std::process::id()));
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
