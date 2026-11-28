//! `qemud:camera`: the goldfish camera HAL's host service, fed by simee camera sources.
//!
//! The guest's camera provider opens `qemud:camera` (the factory) and asks `list`; each linked
//! camera is a line `name=<n> channel=0 pix=<fourcc> dir=<back|front> framedims=<WxH,...>`. A
//! camera session is a pipe of its own, `qemud:camera:name=<n>`, taking `connect`, `start
//! dim=WxH pix=<fourcc>`, `frame ...`, `stop` and `disconnect`. Every query is NUL-terminated;
//! every reply is an 8-hex-digit length and a payload starting `ok` or `ko` (`ok:<data>`,
//! `ko:<reason>`, a bare `ok` NUL-terminated).
//!
//! Frames come two ways, by the image's HAL:
//! - `frame video=<bytes> preview=<bytes> ...` (the libhardware HAL, API 30–33): the reply carries
//!   the frame in the started format, then an RGB32 preview, after `ok:`;
//! - `frame dim=WxH pix=<fourcc> offset=<n> ...` (both HALs; the only way on API 34+'s AIDL
//!   provider): the host writes the frame into the gralloc buffer at `offset` in the goldfish
//!   address-space area and answers `ok`.
//!
//! Each camera follows a link ([`Cameras::link`]) to a sim-camera source, set at start from
//! `-camera-back` / `-camera-front` and changeable while the guest has the camera open: the
//! session resubscribes at the size it is open at, so a relink never changes what the guest sees
//! but the pixels. Captures go through one [`Hub`], so two cameras on one source share it.
//!
//! Another device's screen comes as `feed:<socket>` (`sim_camera::feed`): the simee server
//! captures that device and serves it on the socket, at the size and rate this camera subscribes
//! at; emu only reads it, and links neither the Android nor the iOS stack (a `device:` link here
//! says who serves it). A linked source that fails, to open or before its first frame, shows the
//! test card and is tried again after [`RETRY_MIN`], doubling to [`RETRY_MAX`]: a feed that isn't
//! served yet (the server starts it as the device boots) or a source that can't work is never
//! reopened on every guest frame. A feed that is served but sends nothing (the other device isn't
//! booted yet, or shut down mid-stream) shows the test card too, after [`sim_camera::feed::STALL`]
//! without a frame, never black or the other device's last frame; it stays connected, so the
//! other device's frames replace the card as soon as they come again.
//!
//! The guest's pipe transport serves every pipe on one thread, so each session answers from a
//! thread of its own: waiting for a source's first frame and converting it never stall adb.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use emu_hvf::GuestRam;
pub use sim_camera::SourceSpec;
use sim_camera::{CameraFrame, FrameSpec, HostOpener, Hub, Subscription};
use sim_core::yuv::{self, CameraFormat};

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// Frame sizes `list` offers for every camera; sources are scaled to whichever the guest starts.
pub const FRAME_DIMS: &str = "1280x720,640x480,352x288,320x240,176x144";

const SIZES: [(u32, u32); 5] = [(1280, 720), (640, 480), (352, 288), (320, 240), (176, 144)];

/// Open sessions allowed per camera; a guest's HAL needs one or two.
const MAX_SESSIONS: usize = 4;

/// Replies of `ko:busy` a guest may leave unread before its pipe is closed.
const MAX_BUSY: usize = 16;

/// The first wait before a failed source is tried again; it doubles up to [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(8);

/// Whether `list` offered this frame size.
fn offered(width: u32, height: u32) -> bool {
    SIZES.contains(&(width, height))
}

/// The rate sessions subscribe at; the guest's own requests pace what it gets.
const FPS: u32 = 30;

/// How long a fresh subscription may take to deliver its first frame before the guest gets black.
const FIRST_FRAME: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Facing {
    Back,
    Front,
}

impl Facing {
    pub const ALL: [Facing; 2] = [Facing::Back, Facing::Front];

    /// `dir=` in `list`, and the console's name for it.
    pub fn dir(self) -> &'static str {
        match self {
            Facing::Back => "back",
            Facing::Front => "front",
        }
    }

    /// The device name `list` reports; the HAL opens `qemud:camera:name=<it>`.
    pub fn device_name(self) -> &'static str {
        match self {
            Facing::Back => "simee-back",
            Facing::Front => "simee-front",
        }
    }

    pub fn parse(s: &str) -> Option<Facing> {
        Facing::ALL.into_iter().find(|f| f.dir() == s || f.device_name() == s)
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A stock camera value (`-camera-back`, `-camera-front`, `hw.camera.back` in config.ini) as a
/// link: `none` (or empty) is no camera; `emulated` and `virtualscene` are the test card (stock
/// draws its own scene; simee's is `pattern:testcard`); anything else is a simee source id, and
/// `webcam0`, `webcam1`, ... are the host's cameras by index, as stock numbers them.
pub fn parse_link(value: &str) -> Result<Option<SourceSpec>, String> {
    match value.trim() {
        "" | "none" => Ok(None),
        "emulated" | "virtualscene" => Ok(Some(SourceSpec::Pattern(sim_camera::Pattern::Testcard))),
        v => SourceSpec::parse(v).map(Some).map_err(|e| e.to_string()),
    }
}

#[derive(Clone, Default)]
struct Link {
    source: Option<SourceSpec>,
    /// Bumped by every [`Cameras::link`], so open sessions notice a relink.
    generation: u64,
}

/// The emulator's cameras: what each one is linked to, and the capture hub that feeds them.
pub struct Cameras {
    links: Mutex<[Link; 2]>,
    hub: OnceLock<Hub>,
    /// Open sessions per camera.
    sessions: [AtomicUsize; 2],
    retry: Duration,
}

impl Default for Cameras {
    fn default() -> Cameras {
        Cameras { links: Mutex::new(Default::default()), hub: OnceLock::new(), sessions: Default::default(), retry: RETRY_MIN }
    }
}

impl Cameras {
    /// The process's cameras (one emulator per process), which the pipe service and the console
    /// share.
    pub fn global() -> Arc<Cameras> {
        static GLOBAL: OnceLock<Arc<Cameras>> = OnceLock::new();
        GLOBAL.get_or_init(Arc::default).clone()
    }

    /// Links `facing` to `source` (`None`: no camera). A session the guest has open switches to
    /// it on its next frame, at the size it is open at.
    pub fn link(&self, facing: Facing, source: Option<SourceSpec>) {
        let mut links = self.lock();
        let link = &mut links[facing.index()];
        link.source = source;
        link.generation += 1;
    }

    pub fn linked(&self, facing: Facing) -> Option<SourceSpec> {
        self.lock()[facing.index()].source.clone()
    }

    fn current(&self, facing: Facing) -> Link {
        self.lock()[facing.index()].clone()
    }

    fn lock(&self) -> MutexGuard<'_, [Link; 2]> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Cameras over another opener and retry delay (fakes in tests).
    #[cfg(test)]
    fn with_opener(opener: impl sim_camera::Opener + 'static, retry: Duration) -> Cameras {
        let c = Cameras { retry, ..Cameras::default() };
        let _ = c.hub.set(Hub::new(opener));
        c
    }

    /// Takes one of `facing`'s session slots, or None when they are all open.
    fn slot(self: &Arc<Cameras>, facing: Facing) -> Option<SessionSlot> {
        let n = self.sessions[facing.index()].fetch_add(1, Ordering::SeqCst);
        let slot = SessionSlot { cameras: self.clone(), facing };
        (n < MAX_SESSIONS).then_some(slot)
    }

    fn hub(&self) -> &Hub {
        self.hub.get_or_init(|| Hub::new(HostOpener::new()))
    }

    /// The cameras `list` reports, in its order (back first).
    fn listed(&self) -> Vec<Facing> {
        let links = self.lock();
        Facing::ALL.into_iter().filter(|f| links[f.index()].source.is_some()).collect()
    }

    /// `list`'s reply data: one line per linked camera.
    fn list(&self) -> String {
        self.listed()
            .into_iter()
            .map(|f| {
                format!("name={} channel=0 pix={} dir={} framedims={FRAME_DIMS}\n", f.device_name(), CameraFormat::Nv21.fourcc(), f.dir())
            })
            .collect()
    }

    /// The camera a session's pipe parameter names: `name=<device name>` as the HAL sends it, a
    /// bare name or direction, or an index into `list`.
    fn session_facing(&self, param: &str) -> Option<Facing> {
        let name = token(param, "name").unwrap_or(param.trim());
        Facing::parse(name).or_else(|| name.parse::<usize>().ok().and_then(|i| self.listed().get(i).copied()))
    }
}

/// An open session's place among its camera's, given back when the pipe goes.
struct SessionSlot {
    cameras: Arc<Cameras>,
    facing: Facing,
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.cameras.sessions[self.facing.index()].fetch_sub(1, Ordering::SeqCst);
    }
}

/// `key=value` from a space-separated query or parameter.
fn token<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    s.split_whitespace().find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

fn dims(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// A reply: its length in 8 hex digits, then the payload.
fn reply(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:08x}", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

fn ok() -> Vec<u8> {
    reply(b"ok\0")
}

fn ko(why: &str) -> Vec<u8> {
    reply(format!("ko:{why}\0").as_bytes())
}

fn log_frames() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("EMU_CAMERA_LOG").is_some_and(|v| v != "0"))
}

pub struct CameraService {
    cameras: Arc<Cameras>,
    /// The goldfish address-space area, where `offset=` frames go (None without graphics).
    area: Option<Arc<GuestRam>>,
}

impl CameraService {
    /// The service for this process's cameras ([`Cameras::global`]).
    pub fn new(area: Option<Arc<GuestRam>>) -> CameraService {
        CameraService::with(Cameras::global(), area)
    }

    pub fn with(cameras: Arc<Cameras>, area: Option<Arc<GuestRam>>) -> CameraService {
        CameraService { cameras, area }
    }
}

impl PipeService for CameraService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name == "qemud:camera" {
            return Some(Box::new(FactoryPipe { cameras: self.cameras.clone(), buf: Vec::new() }));
        }
        let param = name.strip_prefix("qemud:camera:")?;
        let facing = self.cameras.session_facing(param);
        eprintln!("emu: camera: the guest opened {name:?} ({})", facing.map_or("no such camera", Facing::dir));
        // No camera of that name, or all its sessions open: answered inline, with no thread.
        let Some(facing) = facing else { return Some(Box::new(RefusePipe { why: "no such camera", buf: Vec::new() })) };
        let Some(slot) = self.cameras.slot(facing) else {
            eprintln!("emu: camera: {} already has {MAX_SESSIONS} sessions open", facing.dir());
            return Some(Box::new(RefusePipe { why: "too many sessions", buf: Vec::new() }));
        };
        let (tx, rx) = mpsc::channel();
        let session = Session::new(Some(facing), self.cameras.clone(), self.area.clone());
        thread::Builder::new().name(format!("camera {}", facing.dir())).spawn(move || serve(session, rx, ctx)).ok()?;
        Some(Box::new(SessionPipe { tx, buf: Vec::new(), awaiting: false, unread: 0, busy: 0, _slot: slot }))
    }
}

/// Takes the NUL-terminated queries out of `buf`.
fn queries(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(nul) = buf.iter().position(|&b| b == 0) {
        let q: Vec<u8> = buf.drain(..=nul).collect();
        out.push(String::from_utf8_lossy(&q[..q.len() - 1]).into_owned());
    }
    out
}

/// A query longer than this without its NUL isn't the camera protocol: the pipe closes.
const MAX_QUERY: usize = 4096;

struct FactoryPipe {
    cameras: Arc<Cameras>,
    buf: Vec<u8>,
}

impl PipeHandler for FactoryPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        for q in queries(&mut self.buf) {
            eprintln!("emu: camera: factory query {q:?}");
            match q.split_whitespace().next() {
                Some("list") => {
                    let list = self.cameras.list();
                    if list.is_empty() {
                        to_guest.extend(ok());
                    } else {
                        to_guest.extend(reply(format!("ok:{list}\0").as_bytes()));
                    }
                }
                _ => to_guest.extend(ko("unknown query")),
            }
        }
        self.buf.len() < MAX_QUERY
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

/// A pipe that answers every query `ko:<why>`: a camera that isn't there, or has no session free.
struct RefusePipe {
    why: &'static str,
    buf: Vec<u8>,
}

impl PipeHandler for RefusePipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        for _ in queries(&mut self.buf) {
            to_guest.extend(ko(self.why));
        }
        self.buf.len() < MAX_QUERY
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

/// A camera session's pipe: queries go to its thread, replies come back as host messages. One
/// query is in flight at a time: the next waits until the guest has read the reply (the HAL does,
/// being synchronous), and one that comes sooner is answered `ko:busy` without reaching the
/// thread, so a guest that never reads can queue neither frames nor work.
struct SessionPipe {
    tx: Sender<String>,
    buf: Vec<u8>,
    /// A query is with the thread, its reply not back yet.
    awaiting: bool,
    /// Reply bytes handed to the transport that the guest hasn't taken.
    unread: usize,
    /// `ko:busy` replies among them.
    busy: usize,
    _slot: SessionSlot,
}

impl PipeHandler for SessionPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        for q in queries(&mut self.buf) {
            if self.awaiting || self.unread > 0 {
                self.busy += 1;
                if self.busy > MAX_BUSY {
                    eprintln!("emu: camera: the guest keeps asking without reading its replies; closing the pipe");
                    return false;
                }
                let r = ko("busy");
                self.unread += r.len();
                to_guest.extend(r);
            } else {
                self.awaiting = true;
                if self.tx.send(q).is_err() {
                    return false;
                }
            }
        }
        self.buf.len() < MAX_QUERY
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) => {
                self.awaiting = false;
                self.unread += d.len();
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn guest_took(&mut self, n: usize) {
        self.unread = self.unread.saturating_sub(n);
        if self.unread == 0 {
            self.busy = 0;
        }
    }
}

/// A session's thread: answers queries in order until the pipe closes (its sender drops), which
/// drops the subscription and so stops the capture if nobody else watches the source.
fn serve(mut session: Session, rx: Receiver<String>, ctx: PipeCtx) {
    while let Ok(q) = rx.recv() {
        if !ctx.send(HostMsg::Data(session.handle(&q))) {
            break;
        }
    }
}

/// What a started camera is fed by: a subscription at the size the guest opened it at, for one
/// generation of its link.
struct Feed {
    width: u32,
    height: u32,
    generation: u64,
    sub: Option<Subscription>,
    /// The newest frame: answered again when no newer one has come, and kept across a relink
    /// until the new source delivers.
    last: Option<CameraFrame>,
    /// The linked source failed (to open, or mid-stream): `sub` is the test card standing in for
    /// it ("source missing"), and the source is tried again at `retry_at`.
    missing: bool,
    retry_at: Instant,
    /// The wait before this try; the next failure in a row doubles it.
    backoff: Duration,
    /// The linked source delivered a frame: its failure starts the backoff over.
    delivered: bool,
}

struct Started {
    width: u32,
    height: u32,
    format: CameraFormat,
}

struct Session {
    facing: Option<Facing>,
    cameras: Arc<Cameras>,
    area: Option<Arc<GuestRam>>,
    started: Option<Started>,
    feed: Option<Feed>,
    /// Frame queries logged since the last start (the first few are, always).
    frames_logged: u32,
}

impl Session {
    fn new(facing: Option<Facing>, cameras: Arc<Cameras>, area: Option<Arc<GuestRam>>) -> Session {
        Session { facing, cameras, area, started: None, feed: None, frames_logged: 0 }
    }

    fn label(&self) -> &'static str {
        self.facing.map_or("?", Facing::dir)
    }

    /// One query's full reply.
    fn handle(&mut self, query: &str) -> Vec<u8> {
        let (verb, param) = query.split_once(' ').unwrap_or((query, ""));
        if verb != "frame" || self.frames_logged < 3 || log_frames() {
            eprintln!("emu: camera {}: {query:?}", self.label());
            self.frames_logged += u32::from(verb == "frame");
        }
        let Some(facing) = self.facing else { return ko("no such camera") };
        match verb {
            "connect" => {
                if self.cameras.linked(facing).is_none() {
                    return ko("the camera has no source linked");
                }
                ok()
            }
            "disconnect" => {
                self.started = None;
                self.feed = None;
                ok()
            }
            // API 34+'s provider starts with no parameters: each `frame` names its own size and
            // format.
            "start" if param.trim().is_empty() => ok(),
            "start" => {
                let Some((width, height)) = token(param, "dim").and_then(dims) else { return ko("invalid or missing 'dim'") };
                let Some(pix) = token(param, "pix").and_then(|p| p.parse::<u32>().ok()) else { return ko("invalid or missing 'pix'") };
                let Some(format) = CameraFormat::from_fourcc(pix) else {
                    eprintln!("emu: camera {}: pixel format {pix} ({}) isn't supported", self.label(), fourcc_text(pix));
                    return ko("unsupported pixel format");
                };
                if !offered(width, height) {
                    return ko("unsupported frame size");
                }
                self.started = Some(Started { width, height, format });
                self.frames_logged = 0;
                ok()
            }
            "stop" => {
                self.started = None;
                self.feed = None;
                ok()
            }
            "frame" => self.frame(facing, param),
            _ => ko("unknown query"),
        }
    }

    fn frame(&mut self, facing: Facing, param: &str) -> Vec<u8> {
        if let Some(offset) = token(param, "offset").and_then(|o| o.parse::<u64>().ok()) {
            return self.frame_to_buffer(facing, param, offset);
        }
        let Some(st) = &self.started else { return ko("camera is not started") };
        let (width, height, format) = (st.width, st.height, st.format);
        let size = |key| token(param, key).and_then(|v| v.parse::<usize>().ok());
        let (Some(video), Some(preview)) = (size("video"), size("preview")) else {
            return ko("invalid or missing 'video' or 'preview'");
        };
        let (w, h) = (width as usize, height as usize);
        if (video != 0 && video != format.frame_size(w, h)) || (preview != 0 && preview != CameraFormat::Rgb32.frame_size(w, h)) {
            return ko("frame size mismatch");
        }
        if video == 0 && preview == 0 {
            return ok();
        }
        let frame = self.next_frame(facing, width, height);
        match pack(&frame, w, h, &[(video, format), (preview, CameraFormat::Rgb32)]) {
            Some(payload) => reply(&payload),
            None => ko("conversion failed"),
        }
    }

    /// `frame dim=WxH pix=<fourcc> offset=<n>`: the frame goes into the guest buffer at `offset`
    /// in the address-space area (a guest-physical address is accepted too).
    fn frame_to_buffer(&mut self, facing: Facing, param: &str, offset: u64) -> Vec<u8> {
        let Some(area) = self.area.clone() else { return ko("no address space for offset frames") };
        let started = self.started.as_ref().map(|s| (s.width, s.height, s.format.fourcc()));
        let (width, height) = token(param, "dim").and_then(dims).or(started.map(|s| (s.0, s.1))).unwrap_or((0, 0));
        let pix = token(param, "pix").and_then(|p| p.parse::<u32>().ok()).or(started.map(|s| s.2)).unwrap_or(0);
        let Some(format) = CameraFormat::from_fourcc(pix) else {
            eprintln!("emu: camera {}: pixel format {pix} ({}) isn't supported", self.label(), fourcc_text(pix));
            return ko("unsupported pixel format");
        };
        if !offered(width, height) {
            return ko("unsupported frame size");
        }
        let frame = self.next_frame(facing, width, height);
        let Some(bytes) = yuv::convert(&frame, width as usize, height as usize, format) else { return ko("conversion failed") };
        let addr = if offset >= area.base() { offset } else { area.base() + offset };
        match area.write(addr, &bytes) {
            Ok(()) => ok(),
            Err(e) => {
                eprintln!("emu: camera {}: frame at offset {offset:#x} ({} bytes): {e:?}", self.label(), bytes.len());
                ko("offset out of range")
            }
        }
    }

    /// The linked source subscribed at `width` x `height`; when it can't be opened, the test card
    /// stands in (and the flag says the source is missing).
    fn subscribe(&self, source: &SourceSpec, width: u32, height: u32) -> (Option<Subscription>, bool) {
        let Ok(spec) = FrameSpec::new(width, height, FPS) else { return (None, false) };
        let hub = self.cameras.hub();
        match hub.subscribe(source, spec) {
            Ok(sub) => {
                eprintln!("emu: camera {}: {source} at {width}x{height}", self.label());
                (Some(sub), false)
            }
            Err(e) => {
                eprintln!("emu: camera {}: source missing: can't open {source}: {e}; the test card stands in", self.label());
                (hub.subscribe(&SourceSpec::Pattern(sim_camera::Pattern::Testcard), spec).ok(), true)
            }
        }
    }

    /// The newest frame of the linked source as BGRA at `width` x `height`: resubscribing when the
    /// size or the link changed, black when there's no source or it has delivered nothing yet. A
    /// source that fails to open or dies mid-stream is shown as the test card ("source missing")
    /// and retried with a growing delay.
    fn next_frame(&mut self, facing: Facing, width: u32, height: u32) -> Arc<[u8]> {
        let link = self.cameras.current(facing);
        let now = Instant::now();
        let stale = self.feed.as_ref().is_none_or(|f| {
            (f.width, f.height, f.generation) != (width, height, link.generation)
                || (f.missing && link.source.is_some() && now >= f.retry_at)
        });
        if stale {
            let old = self.feed.take();
            let retry = self.cameras.retry;
            let backoff =
                old.as_ref().filter(|f| f.missing && f.generation == link.generation).map_or(retry, |f| (f.backoff * 2).min(RETRY_MAX));
            // Keep the old frame across a relink (not a resize) until the new source delivers.
            let last = old.filter(|f| (f.width, f.height) == (width, height)).and_then(|f| f.last);
            let (sub, missing) = link.source.as_ref().map_or((None, false), |source| self.subscribe(source, width, height));
            let mut feed =
                Feed { width, height, generation: link.generation, sub, last, missing, retry_at: now + backoff, backoff, delivered: false };
            if let Some(sub) = &feed.sub
                && let Ok(Some(first)) = sub.recv_timeout(FIRST_FRAME)
            {
                feed.last = Some(first);
                feed.delivered = !feed.missing;
            }
            self.feed = Some(feed);
        }
        let label = self.label();
        let retry = self.cameras.retry;
        let feed = self.feed.as_mut().expect("set above");
        if let Some(sub) = &feed.sub {
            loop {
                match sub.try_recv() {
                    Ok(Some(f)) => {
                        feed.last = Some(f);
                        feed.delivered |= !feed.missing;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // A source that streamed and then died starts the backoff over; one that
                        // dies before its first frame (a region off the screen, a feed with no
                        // source) waits longer each time instead of reopening on every frame.
                        if feed.delivered {
                            feed.backoff = retry;
                        }
                        eprintln!("emu: camera {label}: source missing: {e}; trying it again in {:.1} s", feed.backoff.as_secs_f32());
                        feed.sub = None;
                        feed.missing = true;
                        feed.retry_at = Instant::now() + feed.backoff;
                        break;
                    }
                }
            }
        }
        match &feed.last {
            Some(f) if (f.width, f.height) == (width, height) => f.bgra.clone(),
            Some(f) => f.fit(width, height).bgra,
            None => vec![0; width as usize * height as usize * 4].into(),
        }
    }
}

/// `ok:` and each part of a frame, converted from BGRA; None when the frame isn't `w` x `h`.
fn pack(frame: &[u8], w: usize, h: usize, parts: &[(usize, CameraFormat)]) -> Option<Vec<u8>> {
    let mut payload = Vec::with_capacity(3 + parts.iter().map(|p| p.0).sum::<usize>());
    payload.extend_from_slice(b"ok:");
    for &(len, fmt) in parts {
        if len > 0 {
            let at = payload.len();
            payload.resize(at + len, 0);
            if !yuv::from_bgra(frame, w, h, fmt, &mut payload[at..]) {
                return None;
            }
        }
    }
    Some(payload)
}

/// A fourcc as its four characters, for logs.
fn fourcc_text(code: u32) -> String {
    code.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::pipe::WorkerEvent;

    fn nv21() -> u32 {
        CameraFormat::Nv21.fourcc()
    }

    fn cameras(back: Option<&str>, front: Option<&str>) -> Arc<Cameras> {
        let c = Arc::new(Cameras::default());
        c.link(Facing::Back, back.map(|s| SourceSpec::parse(s).expect("source")));
        c.link(Facing::Front, front.map(|s| SourceSpec::parse(s).expect("source")));
        c
    }

    /// The payload of one reply, checking its length prefix.
    fn payload(r: &[u8]) -> &[u8] {
        let len = usize::from_str_radix(std::str::from_utf8(&r[..8]).expect("hex"), 16).expect("len");
        assert_eq!(r.len(), 8 + len, "length prefix");
        &r[8..]
    }

    #[test]
    fn stock_values_map_to_links() {
        assert_eq!(parse_link("none"), Ok(None));
        assert_eq!(parse_link(""), Ok(None));
        let card = Some(SourceSpec::parse("pattern:testcard").expect("card"));
        assert_eq!(parse_link("emulated"), Ok(card.clone()));
        assert_eq!(parse_link("virtualscene"), Ok(card.clone()));
        assert_eq!(parse_link("pattern:testcard"), Ok(card));
        assert_eq!(parse_link("qr:hello"), Ok(Some(SourceSpec::Qr("hello".into()))));
        assert_eq!(parse_link("webcam0"), Ok(Some(SourceSpec::Webcam("webcam0".into()))), "resolved by index when it opens");
        assert!(parse_link("bogus:x").is_err());
    }

    #[test]
    fn list_without_links_is_empty() {
        let (tx, _rx) = mpsc::channel();
        let svc = CameraService::with(cameras(None, None), None);
        let mut p = svc.open("qemud:camera", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut out = Vec::new();
        assert!(p.guest_data(b"li", &mut out));
        assert!(out.is_empty(), "waits for the NUL");
        assert!(p.guest_data(b"st\0connect\0", &mut out));
        assert_eq!(out, [&b"00000003ok\x00"[..], b"00000011ko:unknown query\x00"].concat());
    }

    #[test]
    fn list_reports_linked_cameras() {
        let (tx, _rx) = mpsc::channel();
        let svc = CameraService::with(cameras(None, Some("qr:hi")), None);
        let mut p = svc.open("qemud:camera", PipeCtx { conn: 1, events: tx.clone() }).expect("open");
        let mut out = Vec::new();
        p.guest_data(b"list\0", &mut out);
        let want = format!("ok:name=simee-front channel=0 pix=825382478 dir=front framedims={FRAME_DIMS}\n\0");
        assert_eq!(payload(&out), want.as_bytes());

        let both = cameras(Some("pattern:testcard"), Some("qr:hi"));
        let list = both.list();
        let lines: Vec<&str> = list.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("name=simee-back ") && lines[0].contains(" dir=back "), "back first: {list}");
        assert!(lines[1].contains(" dir=front "));
        assert_eq!(both.session_facing("name=simee-front"), Some(Facing::Front));
        assert_eq!(both.session_facing("1"), Some(Facing::Front), "an index into list");
        assert_eq!(both.session_facing("name=webcam7"), None);
    }

    #[test]
    fn session_streams_the_started_format() {
        let c = cameras(Some("solid:red"), None);
        let mut s = Session::new(Some(Facing::Back), c, None);
        assert_eq!(s.handle("connect"), ok());
        assert_eq!(payload(&s.handle("frame video=38016 preview=0")), b"ko:camera is not started\0");
        assert_eq!(payload(&s.handle(&format!("start dim=176x144 pix={nv21}", nv21 = nv21()))), b"ok\0");
        assert_eq!(payload(&s.handle("start dim=176x144 pix=1448695129")), b"ko:unsupported pixel format\0", "YUYV");
        assert_eq!(payload(&s.handle("start dim=176x144")), b"ko:invalid or missing 'pix'\0");
        assert_eq!(payload(&s.handle("frame video=100 preview=0 whiteb=1,1,1 expcomp=1 time=0")), b"ko:frame size mismatch\0");

        let r = s.handle("frame video=38016 preview=101376 whiteb=1,1,1 expcomp=1 time=0");
        let p = payload(&r);
        assert_eq!(&p[..3], b"ok:");
        let (video, preview) = p[3..].split_at(38016);
        assert!(video[..176 * 144].iter().all(|&y| y == 82), "red luma");
        assert!(video[176 * 144..].chunks(2).all(|vu| vu == [240, 90]), "red V,U");
        assert!(preview.chunks(4).all(|px| px == [255, 0, 0, 255]), "RGBA preview");

        assert_eq!(payload(&s.handle("stop")), b"ok\0");
        assert_eq!(payload(&s.handle("disconnect")), b"ok\0");
    }

    #[test]
    fn unlinked_and_unknown_cameras_refuse() {
        let c = cameras(None, None);
        assert_eq!(payload(&Session::new(Some(Facing::Front), c.clone(), None).handle("connect")), b"ko:the camera has no source linked\0");
        assert_eq!(payload(&Session::new(None, c, None).handle("connect")), b"ko:no such camera\0");
    }

    fn luma_after(s: &mut Session, want: u8) -> bool {
        // A relink shows within a second.
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_secs(1) {
            let r = s.handle("frame video=38016 preview=0");
            if payload(&r)[3] == want {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn relink_keeps_the_open_size() {
        let c = cameras(Some("solid:red"), Some("solid:blue"));
        let mut back = Session::new(Some(Facing::Back), c.clone(), None);
        let mut front = Session::new(Some(Facing::Front), c.clone(), None);
        for s in [&mut back, &mut front] {
            s.handle(&format!("start dim=176x144 pix={}", nv21()));
        }
        assert!(luma_after(&mut back, 82), "red");
        assert!(luma_after(&mut front, 41), "blue: independent of the back camera");
        c.link(Facing::Back, Some(SourceSpec::parse("qr:hello").expect("qr")));
        let r = back.handle("frame video=38016 preview=0");
        assert_eq!(payload(&r).len(), 3 + 38016, "same size after the relink");
        assert!(luma_after(&mut back, 235), "the QR code's white quiet zone");
        assert!(luma_after(&mut front, 41), "the front camera didn't move");
        // Unlinked while open: the guest keeps getting frames of the open size (the last one).
        c.link(Facing::Back, None);
        assert_eq!(payload(&back.handle("frame video=38016 preview=0")).len(), 3 + 38016);
    }

    #[test]
    fn offset_frames_go_into_the_area() {
        let area = Arc::new(GuestRam::new(0x1_0000_0000, 1 << 20).expect("area"));
        let c = cameras(Some("solid:blue"), None);
        let mut s = Session::new(Some(Facing::Back), c.clone(), Some(area.clone()));
        assert_eq!(payload(&s.handle("connect")), b"ok\0");
        assert_eq!(payload(&s.handle("start")), b"ok\0", "API 34+: a bare start, sizes come per frame");
        let rgb32 = CameraFormat::Rgb32.fourcc();
        let q = format!("frame dim=176x144 pix={rgb32} offset=4096 whiteb=1,1,1 expcomp=1 time=0");
        assert_eq!(payload(&s.handle(&q)), b"ok\0");
        let mut px = [0u8; 8];
        area.read(0x1_0000_0000 + 4096, &mut px).expect("read");
        assert_eq!(px, [0, 0, 255, 255, 0, 0, 255, 255]);
        // A guest-physical address works too, and NV21 into it.
        let q = format!("frame dim=176x144 pix={} offset={} time=0", nv21(), 0x1_0000_0000u64 + 8192);
        assert_eq!(payload(&s.handle(&q)), b"ok\0");
        let mut y = [0u8; 2];
        area.read(0x1_0000_0000 + 8192, &mut y).expect("read");
        assert_eq!(y, [41, 41]);
        let q = format!("frame dim=176x144 pix={rgb32} offset={}", 1u64 << 20);
        assert_eq!(payload(&s.handle(&q)), b"ko:offset out of range\0");
        let mut none = Session::new(Some(Facing::Back), c, None);
        assert_eq!(payload(&none.handle(&q)), b"ko:no address space for offset frames\0");
    }

    #[test]
    fn session_pipe_answers_from_its_thread() {
        let (tx, rx) = mpsc::channel();
        let svc = CameraService::with(cameras(Some("pattern:testcard"), None), None);
        let mut p = svc.open("qemud:camera:name=simee-back", PipeCtx { conn: 9, events: tx }).expect("open");
        let mut out = Vec::new();
        let mut replies = Vec::new();
        for q in ["connect".to_string(), format!("start dim=320x240 pix={}", nv21()), "frame video=115200 preview=0".to_string()] {
            out.clear();
            assert!(p.guest_data(format!("{q}\0").as_bytes(), &mut out));
            assert!(out.is_empty(), "answered by the session thread");
            let Ok(WorkerEvent::Host { conn: 9, msg: HostMsg::Data(d) }) = rx.recv_timeout(Duration::from_secs(5)) else {
                panic!("no reply")
            };
            assert!(p.host_msg(HostMsg::Data(d.clone()), &mut out));
            assert_eq!(out, d);
            p.guest_took(out.len());
            replies.push(d);
        }
        assert_eq!(replies[0], ok());
        assert_eq!(replies[1], ok());
        assert_eq!(payload(&replies[2]).len(), 3 + 115_200);
        assert!(!p.host_msg(HostMsg::Close, &mut out));
    }

    /// Runs the pipe's query through its thread like the transport does: the reply comes back as
    /// a host message, then the guest takes it.
    fn ask(p: &mut Box<dyn PipeHandler>, rx: &Receiver<WorkerEvent>, q: &str) -> Vec<u8> {
        let mut out = Vec::new();
        assert!(p.guest_data(format!("{q}\0").as_bytes(), &mut out));
        if out.is_empty() {
            let Ok(WorkerEvent::Host { msg: HostMsg::Data(d), .. }) = rx.recv_timeout(Duration::from_secs(5)) else {
                panic!("no reply to {q}")
            };
            assert!(p.host_msg(HostMsg::Data(d), &mut out));
        }
        p.guest_took(out.len());
        out
    }

    fn pipe_for(c: Arc<Cameras>, conn: u64) -> (Box<dyn PipeHandler>, Receiver<WorkerEvent>) {
        let (tx, rx) = mpsc::channel();
        let p = CameraService::with(c, None).open("qemud:camera:name=simee-back", PipeCtx { conn, events: tx }).expect("open");
        (p, rx)
    }

    #[test]
    fn only_the_listed_frame_sizes_start() {
        let mut s = Session::new(Some(Facing::Back), cameras(Some("solid:red"), None), None);
        assert_eq!(payload(&s.handle(&format!("start dim=8192x8192 pix={}", nv21()))), b"ko:unsupported frame size\0");
        assert_eq!(payload(&s.handle(&format!("start dim=160x120 pix={}", nv21()))), b"ko:unsupported frame size\0", "not offered by list");
        assert_eq!(payload(&s.handle(&format!("start dim=640x480 pix={}", nv21()))), b"ok\0");
        let area = Arc::new(GuestRam::new(0x1_0000_0000, 1 << 20).expect("area"));
        let mut s = Session::new(Some(Facing::Back), cameras(Some("solid:red"), None), Some(area));
        let q = format!("frame dim=8192x8192 pix={} offset=0", CameraFormat::Rgb32.fourcc());
        assert_eq!(payload(&s.handle(&q)), b"ko:unsupported frame size\0");
    }

    #[test]
    fn a_guest_that_never_reads_stays_bounded() {
        let (mut p, rx) = pipe_for(cameras(Some("solid:red"), None), 5);
        assert_eq!(ask(&mut p, &rx, &format!("start dim=1280x720 pix={}", nv21())), ok());
        let mut out = Vec::new();
        let mut open = true;
        for _ in 0..200 {
            open &= p.guest_data(b"frame video=1382400 preview=3686400\0", &mut out);
        }
        while let Ok(WorkerEvent::Host { msg, .. }) = rx.recv_timeout(Duration::from_millis(300)) {
            p.host_msg(msg, &mut out);
        }
        assert!(out.len() < 8 << 20, "a guest that never reads got {} bytes queued", out.len());
        assert!(!open, "it keeps asking without reading: the pipe closes");
    }

    #[test]
    fn a_second_query_waits_for_the_first_reply_to_be_read() {
        let (mut p, rx) = pipe_for(cameras(Some("solid:red"), None), 5);
        let mut out = Vec::new();
        p.guest_data(b"connect\0", &mut out);
        let Ok(WorkerEvent::Host { msg, .. }) = rx.recv_timeout(Duration::from_secs(5)) else { panic!("no reply") };
        p.host_msg(msg, &mut out);
        assert_eq!(out, ok());
        let mut busy = Vec::new();
        p.guest_data(b"connect\0", &mut busy);
        assert_eq!(payload(&busy), b"ko:busy\0");
        p.guest_took(out.len() + busy.len());
        assert_eq!(ask(&mut p, &rx, "connect"), ok(), "reading the replies frees the session");
    }

    #[test]
    fn unknown_cameras_and_too_many_sessions_refuse_inline() {
        let (tx, _rx) = mpsc::channel();
        let svc = CameraService::with(cameras(Some("solid:red"), None), None);
        let open = |name: &str, conn: u64| svc.open(name, PipeCtx { conn, events: tx.clone() }).expect("open");
        let mut out = Vec::new();
        assert!(open("qemud:camera:name=webcam7", 1).guest_data(b"connect\0", &mut out));
        assert_eq!(payload(&out), b"ko:no such camera\0", "answered inline, no thread");
        let sessions: Vec<_> = (0..MAX_SESSIONS).map(|i| open("qemud:camera:name=simee-back", 10 + i as u64)).collect();
        out.clear();
        open("qemud:camera:name=simee-back", 99).guest_data(b"connect\0", &mut out);
        assert_eq!(payload(&out), b"ko:too many sessions\0");
        drop(sessions);
        out.clear();
        open("qemud:camera:name=simee-back", 100).guest_data(b"connect\0", &mut out);
        assert!(out.is_empty(), "a freed slot gets a session thread");
    }

    /// A webcam that fails to open `fail_opens` times, then opens and dies after `die_after`
    /// frames (only the first healthy open); every other source is synthetic.
    struct Flaky {
        opens: Arc<AtomicUsize>,
        fail_opens: usize,
        die_after: Option<usize>,
    }

    struct Dying {
        inner: Box<dyn sim_camera::Source>,
        left: Option<usize>,
    }

    impl sim_camera::Source for Dying {
        fn synthetic(&self) -> bool {
            self.inner.synthetic()
        }

        fn frame(&mut self, spec: FrameSpec, tick: sim_camera::Tick) -> sim_camera::Result<CameraFrame> {
            match &mut self.left {
                Some(0) => return Err(sim_camera::Error::Capture("unplugged".into())),
                Some(n) => *n -= 1,
                None => {}
            }
            self.inner.frame(spec, tick)
        }
    }

    impl sim_camera::Opener for Flaky {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> sim_camera::Result<Box<dyn sim_camera::Source>> {
            let SourceSpec::Webcam(_) = source else { return sim_camera::synth::open(source) };
            let n = self.opens.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_opens {
                return Err(sim_camera::Error::Capture("no such camera".into()));
            }
            let left = if n == self.fail_opens { self.die_after } else { None };
            Ok(Box::new(Dying { inner: sim_camera::synth::open(&SourceSpec::parse("solid:red").expect("red"))?, left }))
        }
    }

    fn flaky_session(fail_opens: usize, die_after: Option<usize>) -> (Session, Arc<AtomicUsize>) {
        let opens = Arc::new(AtomicUsize::new(0));
        let c = Arc::new(Cameras::with_opener(Flaky { opens: opens.clone(), fail_opens, die_after }, Duration::from_millis(40)));
        c.link(Facing::Back, Some(SourceSpec::Webcam("cam".into())));
        let mut s = Session::new(Some(Facing::Back), c, None);
        assert_eq!(payload(&s.handle(&format!("start dim=176x144 pix={}", nv21()))), b"ok\0");
        (s, opens)
    }

    #[test]
    fn a_source_that_fails_to_open_shows_the_test_card_and_is_retried() {
        let (mut s, opens) = flaky_session(2, None);
        let r = s.handle("frame video=38016 preview=0");
        let luma = &payload(&r)[3..3 + 176 * 144];
        assert!(luma.iter().any(|&y| y != luma[0]), "the test card, not black or a flat colour");
        assert!(luma_after(&mut s, 82), "red once the camera opens");
        assert_eq!(opens.load(Ordering::SeqCst), 3, "two failures, then the open that worked");
    }

    #[test]
    fn a_source_that_dies_mid_stream_comes_back() {
        let (mut s, opens) = flaky_session(0, Some(2));
        assert!(luma_after(&mut s, 82), "red at first");
        let t = Instant::now();
        while opens.load(Ordering::SeqCst) < 2 && t.elapsed() < Duration::from_secs(5) {
            s.handle("frame video=38016 preview=0");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(opens.load(Ordering::SeqCst) >= 2, "reopened after it died");
        assert!(luma_after(&mut s, 82), "red again");
    }

    /// A webcam that opens every time and fails on its first frame, like a region off the source
    /// device's screen used to.
    struct DiesAtOnce(Arc<AtomicUsize>);

    impl sim_camera::Opener for DiesAtOnce {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> sim_camera::Result<Box<dyn sim_camera::Source>> {
            let SourceSpec::Webcam(_) = source else { return sim_camera::synth::open(source) };
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Dying { inner: sim_camera::synth::open(&SourceSpec::parse("solid:red").expect("red"))?, left: Some(0) }))
        }
    }

    #[test]
    fn a_source_that_opens_but_never_delivers_is_retried_with_a_doubling_wait_not_every_frame() {
        let opens = Arc::new(AtomicUsize::new(0));
        let c = Arc::new(Cameras::with_opener(DiesAtOnce(opens.clone()), Duration::from_millis(40)));
        c.link(Facing::Back, Some(SourceSpec::Webcam("cam".into())));
        let mut s = Session::new(Some(Facing::Back), c, None);
        assert_eq!(payload(&s.handle(&format!("start dim=176x144 pix={}", nv21()))), b"ok\0");
        // The guest asks for frames every 5 ms for a second.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(1) {
            let r = s.handle("frame video=38016 preview=0");
            assert_eq!(payload(&r).len(), 3 + 38016, "a frame every time (the test card)");
            thread::sleep(Duration::from_millis(5));
        }
        // Waits of 40, 80, 160, 320 ms: about five opens, where reopening per frame made ~150.
        let n = opens.load(Ordering::SeqCst);
        assert!((3..=7).contains(&n), "{n} opens in a second");
    }

    #[cfg(unix)]
    /// Another device's screen as the simee server serves it: a feed on a socket. The camera shows
    /// the test card until the feed is served, then reads it, at the size the guest opened.
    #[test]
    fn a_feed_link_reads_the_feed_the_server_serves_once_it_is_there() {
        let path = std::path::PathBuf::from("/tmp").join(format!("emu-cam-feed-{}.sock", std::process::id()));
        std::fs::remove_file(&path).ok();
        let c = Arc::new(Cameras::with_opener(HostOpener::new(), Duration::from_millis(40)));
        c.link(Facing::Back, parse_link(&format!("feed:{}", path.display())).expect("feed link"));
        let mut s = Session::new(Some(Facing::Back), c, None);
        assert_eq!(payload(&s.handle(&format!("start dim=176x144 pix={}", nv21()))), b"ok\0");
        let r = s.handle("frame video=38016 preview=0");
        let luma = &payload(&r)[3..3 + 176 * 144];
        assert!(luma.iter().any(|&y| y != luma[0]), "the test card while nothing serves the feed");
        // The server starts the feed (here, a red source behind it).
        let hub = Arc::new(Hub::new(HostOpener::new()));
        let feed =
            sim_camera::feed::Feed::serve(hub, &path, SourceSpec::parse("solid:red").expect("red"), FrameSpec::default()).expect("serve");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) && payload(&s.handle("frame video=38016 preview=0"))[3] != 82 {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(luma_after(&mut s, 82), "red from the feed");
        assert_eq!(feed.readers(), 1, "one connection, at the camera's size");
        drop(feed);
    }

    /// Whether a 176x144 frame's luma isn't flat: the test card (black, red or blue is flat).
    #[cfg_attr(not(unix), allow(dead_code))]
    fn card(r: &[u8]) -> bool {
        let luma = &payload(r)[3..3 + 176 * 144];
        luma.iter().any(|&y| y != luma[0])
    }

    /// The server's side of a feed whose source device A is red until the flag says it shut down:
    /// then its capture ends and it doesn't open. Any other source opens as usual.
    #[cfg_attr(not(unix), allow(dead_code))]
    struct ShutsDown(Arc<AtomicBool>);

    #[cfg_attr(not(unix), allow(dead_code))]
    struct Red(Box<dyn sim_camera::Source>, Arc<AtomicBool>);

    impl sim_camera::Source for Red {
        fn synthetic(&self) -> bool {
            true
        }

        fn frame(&mut self, spec: FrameSpec, tick: sim_camera::Tick) -> sim_camera::Result<CameraFrame> {
            if self.1.load(Ordering::SeqCst) {
                return Err(sim_camera::Error::Missing("A shut down".into()));
            }
            self.0.frame(spec, tick)
        }
    }

    impl sim_camera::Opener for ShutsDown {
        fn open(&self, source: &SourceSpec, _spec: FrameSpec) -> sim_camera::Result<Box<dyn sim_camera::Source>> {
            let SourceSpec::Webcam(_) = source else { return sim_camera::synth::open(source) };
            if self.0.load(Ordering::SeqCst) {
                return Err(sim_camera::Error::Missing("A isn't running".into()));
            }
            Ok(Box::new(Red(sim_camera::synth::open(&SourceSpec::parse("solid:red").expect("red"))?, self.0.clone())))
        }
    }

    #[cfg(unix)]
    fn feed_session(tag: &str) -> (std::path::PathBuf, Session) {
        let path = std::path::PathBuf::from("/tmp").join(format!("emu-cam-{tag}-{}.sock", std::process::id()));
        std::fs::remove_file(&path).ok();
        let c = Arc::new(Cameras::with_opener(HostOpener::new(), Duration::from_millis(40)));
        c.link(Facing::Back, parse_link(&format!("feed:{}", path.display())).expect("feed link"));
        let mut s = Session::new(Some(Facing::Back), c, None);
        assert_eq!(payload(&s.handle(&format!("start dim=176x144 pix={}", nv21()))), b"ok\0");
        (path, s)
    }

    #[cfg(unix)]
    /// B booted before A: the server serves B's feed, but A isn't there to fill it. The camera
    /// shows the test card within a second, not black, and A's frames once it is up.
    #[test]
    fn a_feed_that_is_served_but_sends_nothing_shows_the_test_card_until_it_does() {
        let (path, mut s) = feed_session("unfed");
        let hub = Arc::new(Hub::new(HostOpener::new()));
        // `device:` never opens in emu's opener: a feed that is up and silent, as with A shut down.
        let feed =
            sim_camera::feed::Feed::serve(hub, &path, SourceSpec::parse("device:A").expect("A"), FrameSpec::default()).expect("serve");
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(1500) && !card(&s.handle("frame video=38016 preview=0")) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(t.elapsed() < Duration::from_millis(1500), "the test card within a second, not black");
        for _ in 0..50 {
            assert!(card(&s.handle("frame video=38016 preview=0")), "the test card stays");
            thread::sleep(Duration::from_millis(10));
        }
        // A comes up: its frames replace the test card.
        feed.set_source(SourceSpec::parse("solid:red").expect("red"));
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) && payload(&s.handle("frame video=38016 preview=0"))[3] != 82 {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(luma_after(&mut s, 82), "red once the feed sends it");
    }

    #[cfg(unix)]
    /// A shuts down mid-stream: the server's feed stays up but goes silent. The camera shows the
    /// test card within a second (not A's last frame), and A's frames again when they resume.
    #[test]
    fn a_feed_that_goes_silent_mid_stream_shows_the_test_card_then_the_frames_again() {
        let (path, mut s) = feed_session("silent");
        let down = Arc::new(AtomicBool::new(false));
        let hub = Arc::new(Hub::new(ShutsDown(down.clone())));
        let feed = sim_camera::feed::Feed::serve(hub, &path, SourceSpec::Webcam("A".into()), FrameSpec::default()).expect("serve");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) && payload(&s.handle("frame video=38016 preview=0"))[3] != 82 {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(luma_after(&mut s, 82), "A's red");
        // A shuts down.
        down.store(true, Ordering::SeqCst);
        let stopped = Instant::now();
        while stopped.elapsed() < Duration::from_millis(1500) && !card(&s.handle("frame video=38016 preview=0")) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(stopped.elapsed() < Duration::from_millis(1500), "the test card within a second of A's last frame, not A's frame");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            assert!(card(&s.handle("frame video=38016 preview=0")), "the test card, never A's last frame, while A is down");
            thread::sleep(Duration::from_millis(20));
        }
        // A is back (here: the feed relinked to blue): its frames replace the test card.
        feed.set_source(SourceSpec::parse("solid:blue").expect("blue"));
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) && payload(&s.handle("frame video=38016 preview=0"))[3] != 41 {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(luma_after(&mut s, 41), "blue once frames resume");
    }

    #[test]
    fn a_device_link_says_the_server_serves_it() {
        let link = parse_link("device:Pixel_9").expect("parses");
        let err = sim_camera::Opener::open(&HostOpener::new(), link.as_ref().expect("a source"), FrameSpec::default()).err();
        assert!(matches!(&err, Some(sim_camera::Error::Unsupported(m)) if m.contains("simee server")), "{err:?}");
    }

    #[test]
    fn frame_dims_match_the_sizes_offered() {
        let listed: Vec<(u32, u32)> = FRAME_DIMS.split(',').filter_map(dims).collect();
        assert_eq!(listed, SIZES);
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_ko_not_zeros() {
        assert_eq!(pack(&[0u8; 4], 320, 240, &[(115_200, CameraFormat::Nv21)]), None);
        let frame = vec![0u8; 32 * 32 * 4];
        assert_eq!(pack(&frame, 32, 32, &[(1536, CameraFormat::Nv21)]).map(|p| p.len()), Some(3 + 1536));
    }
}
