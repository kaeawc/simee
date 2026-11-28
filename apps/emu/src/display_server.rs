//! Live display and input for local front ends (the desktop app) over a unix socket at
//! `<avd>/simee/display.sock`. The protocol (frame records one way, input lines the other) is
//! `sim_core::stream`, shared with the clients that read it. A frame goes out when the guest posted
//! a new one, at most every [`DEFAULT_INTERVAL`] (or what the client asked for with `fps <n>`),
//! read back once however many clients and screenshots want it ([`FrameStore::latest`]).
//!
//! A client that sends `shm` gets its frames through a shared ring: a file in `<avd>/simee/`
//! this process maps, writes each frame into (downscaled straight into its slot), and announces
//! with a 32-byte record; only that record crosses the socket. Other clients get the RGBA rows on
//! the socket, as before.
//!
//! A client that sends `surfaces <name>` (simee's encoder) gets each posted frame, under
//! `-gpu metal`, as the IOSurface the renderer's GPU copied it into: the surface's Mach send right
//! goes to the port it checked in under `<name>`, and an `EMUM` record says which. The surface is
//! held for that client until it sends `release <seq>`. A frame with no surface (a presented one,
//! a downscaled stream, a turned one for a client that doesn't turn frames, every surface held)
//! goes the client's other way (ring or socket).
//!
//! A turned device's frames go to a client that sent `rotates` as the panel lies, with an `EMUO`
//! record saying how to turn them, so it turns them as it draws and nothing here turns them; any
//! other client gets them turned upright ([`FrameStore::latest_upright`]). Touches are in pixels
//! of the upright screen either way ([`FrameStore::to_scanout`]).
use sim_core::hostfs::OpenOptionsExt;
use sim_core::net::{UnixListener, UnixStream};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use emu_hvf::SharedMap;
use emu_vmm::display::{FrameStore, SharedSurface};
use sim_core::gesture::Phase;
use sim_core::pixels::{downscale_factor, downscale_into, downscaled_size};
use sim_core::rotation::Rotation;
use sim_core::stream::{FrameHeader, Input, Record, ring};

/// The shortest time between two frames to one client unless it asks otherwise (`fps <n>`):
/// 16 ms, so 60 Hz content goes out frame for frame. A frame posted after a quiet spell goes out
/// at once; under a steady stream each goes out at most one interval after the last.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(16);
/// The rates a client may ask for with `fps <n>`.
const FPS: std::ops::RangeInclusive<u32> = 1..=240;
/// Slots per ring: a client reading a frame within two frame intervals never races the writer.
const RING_SLOTS: u32 = 3;
/// Surfaces a client may hold unreleased; past that its frames go the other way until it
/// releases some.
const MAX_LEASES: usize = 4;

pub struct DisplayServer {
    path: PathBuf,
}

impl Drop for DisplayServer {
    /// Removes the socket and this process's rings: streams still running when the emulator
    /// exits never drop theirs.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent()
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let prefix = ring_prefix();
            for e in entries.flatten() {
                if e.file_name().to_str().is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".ring")) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Starts serving; the socket is removed when the returned guard drops.
pub fn start(
    avd_dir: &Path,
    display: Arc<FrameStore>,
    inputs: Arc<Mutex<Option<emu_vmm::Inputs>>>,
    stop: Arc<AtomicBool>,
) -> Option<DisplayServer> {
    let dir = emu_avd::state_dir(avd_dir);
    remove_stale_rings(&dir);
    let path = dir.join("display.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    std::thread::Builder::new()
        .name("display-server".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (d, i, s, dir) = (display.clone(), inputs.clone(), stop.clone(), dir.clone());
                let _ = std::thread::Builder::new().name("display-client".into()).spawn(move || client(conn, d, i, s, dir));
            }
        })
        .ok()?;
    Some(DisplayServer { path })
}

/// What a client asked for.
#[derive(Default)]
struct Wants {
    /// From `hello`; 0 pauses the stream (the client keeps its last frame).
    max_width: AtomicU32,
    shared: AtomicBool,
    /// The client turns frames itself (`rotates`): frames as the panel lies, with `EMUO`.
    rotates: AtomicBool,
    done: AtomicBool,
    /// Frames per second asked for with `fps <n>`; 0 for [`DEFAULT_INTERVAL`].
    fps: AtomicU32,
    /// Held while `max_width` or `done` changes and while a paused stream checks them, so a
    /// change can't land between that check and its wait on `changed`.
    lock: Mutex<()>,
    /// Notified when `max_width` or `done` changes.
    changed: Condvar,
    /// Passes of the frame loop, for telling a parked stream from a spinning one in tests.
    passes: AtomicU64,
    /// Where this client's surfaces go (`surfaces <name>`).
    #[cfg(target_os = "macos")]
    surfaces: Mutex<Option<emu_gfxstream::metal::MachSender>>,
    /// Surfaces sent and not yet released, by sequence number: each is held for the client.
    leases: Mutex<std::collections::HashMap<u32, Arc<dyn SharedSurface>>>,
    #[cfg(target_os = "macos")]
    next_seq: AtomicU32,
}

impl Wants {
    fn set_max_width(&self, max_width: u32) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.max_width.store(max_width, Ordering::Relaxed);
        self.changed.notify_all();
    }

    fn finish(&self) {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.done.store(true, Ordering::Relaxed);
        self.changed.notify_all();
    }

    /// While paused (`hello 0`), waits until the client resumes or goes away, up to `timeout`
    /// (the caller's backstop for the emulator stopping). A paused stream waits here, not on
    /// [`FrameStore::wait_newer`]: the frame it already sent is older than the guest's newest,
    /// so that wait would return at once, round and round, and its reader mark would keep the
    /// store from freeing its buffers.
    fn wait_unpaused(&self, timeout: Duration) {
        let g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .changed
            .wait_timeout_while(g, timeout, |_| self.max_width.load(Ordering::Relaxed) == 0 && !self.done.load(Ordering::Relaxed));
    }
    /// Surfaces go to the port the client checked in under `name` from the next frame on; none
    /// for `-` ([`sim_core::stream::NO_SURFACES`]: the client can't use them).
    fn send_surfaces_to(&self, name: &str) {
        #[cfg(target_os = "macos")]
        {
            let off = name == sim_core::stream::NO_SURFACES;
            let sender = if off { None } else { emu_gfxstream::metal::MachSender::look_up(name) };
            if sender.is_none() && !off {
                eprintln!("emu: display: no surface port named {name}; sending frames as pixels");
            }
            *self.surfaces.lock().unwrap_or_else(|e| e.into_inner()) = sender;
        }
        #[cfg(not(target_os = "macos"))]
        let _ = name;
    }

    fn release(&self, seq: u32) {
        self.leases.lock().unwrap_or_else(|e| e.into_inner()).remove(&seq);
    }

    fn interval(&self) -> Duration {
        match self.fps.load(Ordering::Relaxed) {
            0 => DEFAULT_INTERVAL,
            fps => Duration::from_secs(1) / fps,
        }
    }
}

/// `fps <n>`: the client's frame rate cap, an optional line ahead of or after `hello` (servers
/// that don't know it ignore it, as they do any line they can't parse). `n` outside [`FPS`] is
/// ignored. Not in `sim_core::stream` yet, so only this server knows it.
fn parse_fps(line: &str) -> Option<u32> {
    let mut w = line.split_whitespace();
    let (Some("fps"), Some(n), None) = (w.next(), w.next(), w.next()) else { return None };
    n.parse().ok().filter(|n| FPS.contains(n))
}

fn client(conn: UnixStream, display: Arc<FrameStore>, inputs: Arc<Mutex<Option<emu_vmm::Inputs>>>, stop: Arc<AtomicBool>, dir: PathBuf) {
    let wants = Arc::new(Wants::default());
    // Frames go only to clients that ask (`hello`): input-only connections (one per API touch)
    // get no frame thread.
    let mut writer = conn.try_clone().ok();
    let mut finger_down = false;
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        if let Some(fps) = parse_fps(&line) {
            wants.fps.store(fps, Ordering::Relaxed);
            continue;
        }
        let Some(input) = Input::parse(&line) else { continue };
        let g = inputs.lock().unwrap_or_else(|e| e.into_inner());
        match (input, g.as_ref()) {
            (Input::Hello { max_width: mw }, _) => {
                wants.set_max_width(mw);
                // A stream waiting for a newer frame notices the new width now, not at its timeout.
                display.wake();
                if let Some(writer) = writer.take() {
                    let (display, wants, stop, dir) = (display.clone(), wants.clone(), stop.clone(), dir.clone());
                    let _ = std::thread::Builder::new()
                        .name("display-frames".into())
                        .spawn(move || frames(writer, &display, &wants, &stop, &dir));
                }
            }
            (Input::Shared, _) => wants.shared.store(true, Ordering::Relaxed),
            (Input::Socket, _) => wants.shared.store(false, Ordering::Relaxed),
            (Input::Rotates, _) => wants.rotates.store(true, Ordering::Relaxed),
            (Input::Surfaces(name), _) => wants.send_surfaces_to(&name),
            (Input::Release(seq), _) => wants.release(seq),
            (Input::Touch { x, y, phase }, Some(emu_vmm::Inputs { touch: Some(touch), .. })) => {
                // Clients point at the screen they show: upright as the guest rotated its
                // display, a folded foldable's view from its origin on the scanout.
                let (x, y) = display.to_scanout(f64::from(x), f64::from(y));
                match phase {
                    Phase::Down | Phase::Move => {
                        finger_down = true;
                        touch.touch(x, y, true);
                    }
                    Phase::Up => {
                        if finger_down {
                            touch.touch(x, y, false);
                        }
                        finger_down = false;
                    }
                }
            }
            // A console key name, or a raw Linux keycode (as the console's `event send` takes).
            (Input::Key(name), Some(i)) => {
                if let Some(code) = sim_core::goldfish::keycode(&name) {
                    i.keyboard.tap_key(code);
                }
            }
            // Clicks become the rotary's units here, once; the console's `event send
            // EV_REL:REL_WHEEL:<n>` stays raw.
            (Input::Wheel(clicks), Some(i)) => {
                i.rotary.send(vec![
                    emu_vmm::virtio::input::InputEvent::new(
                        emu_vmm::virtio::input::EV_REL,
                        emu_vmm::virtio::input::REL_WHEEL,
                        sim_core::stream::wheel_units(clicks),
                    ),
                    emu_vmm::virtio::input::InputEvent::syn(),
                ]);
            }
            _ => {}
        }
    }
    wants.finish();
    // Its frame thread may be waiting for a frame on a static screen: end that wait now.
    display.wake();
}

/// One frame, ready to send once the capture is released.
enum Out {
    /// RGBA rows in the stream's buffer, for the socket.
    Socket(FrameHeader),
    /// In the ring's `slot`; `announce` when the ring is new.
    Shared { header: FrameHeader, slot: u32, announce: Option<PathBuf> },
    /// Surface `seq`, already sent to the client's port.
    Surface { header: FrameHeader, seq: u32 },
}

/// How a stream's frames are turned: as the panel lies for a client that turns them itself, else
/// upright.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Turn {
    Panel,
    Upright,
}

fn frames(mut out: UnixStream, display: &FrameStore, wants: &Wants, stop: &AtomicBool, dir: &Path) {
    let mut sent = 0u64;
    // The `hello` width the last frame went out for: a client that asks for another size of a
    // screen that isn't drawing gets the newest frame again at that size.
    let mut sent_mw = 0u32;
    // Pacing is a GCRA (a token bucket kept as one instant): `tat` is when the next frame would
    // be due at exactly one per interval; a frame may go up to half an interval before that.
    // The long-run rate stays at most one per interval, while a frame that comes in a little
    // early after a late one (vsync jitter) isn't held back. None: the next goes out at once.
    let mut tat: Option<Instant> = None;
    // For socket frames: one buffer for the stream's life.
    let mut rows = Vec::new();
    let mut ring: Option<Ring> = None;
    let mut ring_failed = false;
    // The rotation last told to a client that turns frames itself.
    let mut told: Option<Rotation> = None;
    let mut stats = std::env::var_os("EMU_FRAME_STATS").is_some().then(Stats::default);
    while !wants.done.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        wants.passes.fetch_add(1, Ordering::Relaxed);
        if wants.max_width.load(Ordering::Relaxed) == 0 {
            // Paused: wait for `hello <w>`, then send the newest frame (if it's newer than the
            // last sent) at once.
            wants.wait_unpaused(Duration::from_secs(1));
            continue;
        }
        // Wait for a new frame (woken by the frame, or by `wake` when the client goes away; the
        // 1 s timeout only notices the emulator stopping); then, if it's early, wait until it may
        // go and send the newest by then. A client that was idle gets it at once.
        // (Not when a new width is already asked for: a wake that came before this wait is gone.)
        let asked = wants.max_width.load(Ordering::Relaxed);
        let n = if sent_mw != 0 && asked != 0 && asked != sent_mw {
            display.frames()
        } else {
            display.wait_newer(sent, Duration::from_secs(1))
        };
        let mw = wants.max_width.load(Ordering::Relaxed);
        if mw == 0 || (n == sent && mw == sent_mw) {
            continue;
        }
        let interval = wants.interval();
        if let Some(left) = tat.and_then(|t| t.checked_sub(interval / 2)).and_then(|t| t.checked_duration_since(Instant::now())) {
            std::thread::sleep(left);
        }
        let n = display.frames().max(n);
        let started = Instant::now();
        let turn = if wants.rotates.load(Ordering::Relaxed) { Turn::Panel } else { Turn::Upright };
        let frame = if let Some(surface) = surface_frame(display, wants, mw, turn) {
            Some(surface)
        } else if wants.shared.load(Ordering::Relaxed) && !ring_failed {
            shared_frame(display, mw, n, &mut ring, dir, turn).unwrap_or_else(|e| {
                // No ring: this client gets its frames on the socket from the next one.
                eprintln!("emu: display: no shared frame ring ({e}); sending frames on the socket");
                ring_failed = true;
                None
            })
        } else {
            socket_frame(display, mw, &mut rows, turn)
        };
        let Some(frame) = frame else {
            // Nothing readable: try again after the interval, not in a spin.
            tat = Some(Instant::now() + interval);
            continue;
        };
        sent_mw = mw;
        let captured = started.elapsed();
        // Read after the frame: a turn counts the frame again once it's set, so a frame taken
        // before a turn is followed by one that carries it.
        let rotation = display.rotation();
        let tell = (turn == Turn::Panel && told != Some(rotation)).then(|| Record::Rotation(rotation).encode());
        let result = match &frame {
            Out::Socket(header) => {
                sent = header.frame;
                let mut head = Vec::with_capacity(64);
                head.extend(tell.iter().flatten());
                head.extend(header.encode());
                out.write_all(&head).and_then(|()| out.write_all(&rows))
            }
            Out::Shared { header, slot, announce } => {
                sent = header.frame;
                let mut msg = Vec::with_capacity(160);
                msg.extend(tell.iter().flatten());
                if let Some(path) = announce {
                    let p = path.as_os_str().as_encoded_bytes();
                    msg.extend(Record::Ring { path_len: p.len() as u32 }.encode());
                    msg.extend_from_slice(p);
                }
                msg.extend(Record::Shared { header: *header, slot: *slot }.encode());
                out.write_all(&msg)
            }
            Out::Surface { header, seq } => {
                sent = header.frame;
                let mut msg = Vec::with_capacity(64);
                msg.extend(tell.iter().flatten());
                msg.extend(Record::Surface { header: *header, seq: *seq }.encode());
                out.write_all(&msg)
            }
        };
        if result.is_err() {
            break;
        }
        if tell.is_some() {
            told = Some(rotation);
        }
        let done = Instant::now();
        // From when this frame was due (or taken, if later), not when it finished going out: a
        // capture's time and sleep overshoot then don't add up frame after frame (which made a
        // steady 60 Hz stream lag further each frame until one was dropped).
        tat = Some(tat.map_or(started, |t| t.max(started)) + interval);
        if let Some(line) = stats.as_mut().and_then(|s| s.add(&frame, captured, done - started - captured)) {
            eprintln!("emu: display: {line}");
        }
    }
}

/// `EMU_FRAME_STATS=1`: each stream reports, every [`Stats::EVERY`] frames, how long getting a
/// frame (readback when it's new, then scaling it into the ring or the socket buffer) and sending
/// it took, for measuring on a running guest.
#[derive(Default)]
struct Stats {
    capture: Vec<f64>,
    send: Vec<f64>,
}

impl Stats {
    const EVERY: usize = 100;

    fn add(&mut self, frame: &Out, capture: Duration, send: Duration) -> Option<String> {
        self.capture.push(capture.as_secs_f64() * 1000.0);
        self.send.push(send.as_secs_f64() * 1000.0);
        if self.capture.len() < Self::EVERY {
            return None;
        }
        let (header, how) = match frame {
            Out::Socket(h) => (h, "socket"),
            Out::Shared { header, .. } => (header, "ring"),
            Out::Surface { header, .. } => (header, "surface"),
        };
        let summary = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            let s = format!("median {:.2} ms, max {:.2} ms", v[v.len() / 2], v[v.len() - 1]);
            v.clear();
            s
        };
        Some(format!(
            "{} frames of {}x{} (device {}x{}) by {how}: capture+scale {}; send {}",
            Self::EVERY,
            header.width,
            header.height,
            header.device_width,
            header.device_height,
            summary(&mut self.capture),
            summary(&mut self.send)
        ))
    }
}

/// The latest frame as a surface sent to the client's port (its lease kept until `release`), for
/// a client that asked for surfaces: only a full-size frame, as the client shows it (unturned, or
/// to a client that turns frames itself). `None` sends the frame the client's other way.
fn surface_frame(display: &FrameStore, wants: &Wants, max_width: u32, turn: Turn) -> Option<Out> {
    #[cfg(target_os = "macos")]
    {
        let sender = wants.surfaces.lock().unwrap_or_else(|e| e.into_inner());
        let sender = sender.as_ref()?;
        if turn == Turn::Upright && display.rotation() != Rotation::Deg0 {
            return None;
        }
        let mut leases = wants.leases.lock().unwrap_or_else(|e| e.into_inner());
        if leases.len() >= MAX_LEASES {
            return None;
        }
        let (frame, surface) = display.latest_surface()?;
        let (w, h) = surface.size();
        if downscale_factor(w, max_width) != 1 {
            return None;
        }
        let header = FrameHeader::new(w, h, w, h, frame)?;
        let seq = wants.next_seq.fetch_add(1, Ordering::Relaxed);
        // Held before it goes: a release can't come before the lease.
        leases.insert(seq, surface.clone());
        if !surface.mach_port().is_some_and(|port| sender.send(port, seq)) {
            leases.remove(&seq);
            return None;
        }
        Some(Out::Surface { header, seq })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (display, wants, max_width, turn, MAX_LEASES);
        None
    }
}

/// The latest frame, scaled into the socket buffer `rows` while the capture is held (sent after,
/// so a slow client never holds up other streams or screenshots).
fn socket_frame(display: &FrameStore, max_width: u32, rows: &mut Vec<u8>, turn: Turn) -> Option<Out> {
    let scale = |cap: &emu_vmm::display::Capture| {
        let factor = downscale_factor(cap.width, max_width);
        let (w, h) = downscaled_size(cap.width, cap.height, factor);
        let header = FrameHeader::new(w, h, cap.width, cap.height, cap.frame)?;
        rows.resize(header.len as usize, 0);
        downscale_into(cap.width, cap.height, &cap.rgba, factor, rows);
        Some(Out::Socket(header))
    };
    match turn {
        Turn::Panel => display.latest(scale),
        Turn::Upright => display.latest_upright(scale),
    }
    .flatten()
}

/// Frame `frame` written into the next slot of `ring` (created, or replaced by a larger one, as
/// needed, and then announced): downscaled from the shared capture, or, at full size, read back
/// from the renderer straight into the slot. `Ok(None)` when the frame can't be read; `Err` when
/// no ring can be made.
fn shared_frame(
    display: &FrameStore,
    max_width: u32,
    frame: u64,
    ring: &mut Option<Ring>,
    dir: &Path,
    turn: Turn,
) -> std::io::Result<Option<Out>> {
    // Turned upright, frames come from the turned copy; unturned, that's the capture itself, so
    // a full-size frame still reads straight into the slot.
    let upright = turn == Turn::Upright && display.rotation() != Rotation::Deg0;
    let (width, height, _) = if upright { display.upright_geometry() } else { display.geometry() };
    let factor = downscale_factor(width, max_width);
    let (w, h) = downscaled_size(width, height, factor);
    let Some(header) = FrameHeader::new(w, h, width, height, frame) else { return Ok(None) };
    let ring = match ring {
        Some(r) if r.fits(header.len) => r,
        _ => {
            // A new size (or the first frame): a new ring.
            *ring = None;
            ring.insert(Ring::create(dir, u64::from(header.len))?)
        }
    };
    let mut ok = false;
    let slot = if factor == 1 && !upright {
        ring.publish(&header, |dst| ok = display.read_into(width, height, frame, dst))
    } else {
        ring.publish(&header, |dst| {
            // The capture may hold a newer frame than `frame` by now: it's at least as new.
            let scale = |cap: &emu_vmm::display::Capture| {
                let same = (cap.width, cap.height) == (width, height);
                if same {
                    downscale_into(width, height, &cap.rgba, factor, dst);
                }
                same
            };
            let scaled = if upright { display.latest_upright(scale) } else { display.latest(scale) };
            ok = scaled == Some(true);
        })
    };
    // A slot that wasn't filled is never announced; a client still reading an older frame there
    // sees its sequence number move and skips it. The ring itself is announced with the first
    // frame that is sent from it, however many tries that takes.
    if !ok {
        return Ok(None);
    }
    let announce = (!ring.announced).then(|| ring.path.clone());
    ring.announced = true;
    Ok(Some(Out::Shared { header, slot, announce }))
}

/// A frame ring this process writes (layout in `sim_core::stream::ring`): a private file in the
/// AVD's `simee/`, removed when the ring is dropped.
struct Ring {
    path: PathBuf,
    map: SharedMap,
    layout: ring::Layout,
    next: u32,
    /// Its path has gone to the client.
    announced: bool,
}

static RING_IDS: AtomicU64 = AtomicU64::new(0);

/// This process's ring names start with this.
fn ring_prefix() -> String {
    format!("frames-{}-", std::process::id())
}

impl Ring {
    /// A new ring for frames of up to `frame_bytes`, named for this process
    /// (`frames-<pid>-<n>.ring`) so a crashed emulator's rings can be found and removed.
    ///
    /// The file is written out in full before it's mapped: its blocks are allocated (a full disk
    /// is an error here, not a `SIGBUS` when a frame lands in a hole) and its pages are resident
    /// before the first frame.
    fn create(dir: &Path, frame_bytes: u64) -> std::io::Result<Ring> {
        let layout = ring::Layout::new(RING_SLOTS, frame_bytes);
        let (path, file) = Ring::open_new(dir)?;
        let ring = Ring::fill_zero(&file, layout.file_len()).and_then(|()| SharedMap::new(&file, layout.file_len() as usize));
        match ring {
            Ok(mut map) => {
                map.as_mut_slice()[..ring::FILE_HEADER_LEN].copy_from_slice(&layout.encode());
                Ok(Ring { path, map, layout, next: 0, announced: false })
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// A fresh private file `frames-<pid>-<n>.ring`, skipping names a previous process with this
    /// PID left behind.
    fn open_new(dir: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
        let mut tries = 0;
        loop {
            let path = dir.join(format!("{}{}.ring", ring_prefix(), RING_IDS.fetch_add(1, Ordering::Relaxed)));
            match std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 16 => tries += 1,
                other => return other.map(|f| (path, f)),
            }
        }
    }

    fn fill_zero(file: &std::fs::File, len: u64) -> std::io::Result<()> {
        use sim_core::hostfs::FileExt;
        let zeros = vec![0u8; 1 << 20];
        let mut at = 0;
        while at < len {
            let n = (len - at).min(zeros.len() as u64) as usize;
            file.write_all_at(&zeros[..n], at)?;
            at += n as u64;
        }
        Ok(())
    }

    fn fits(&self, len: u32) -> bool {
        u64::from(len) <= self.layout.capacity
    }

    /// Writes the next slot (`fill` gets exactly the frame's bytes) under its seqlock: the
    /// sequence number goes odd before the first byte and even after the last.
    fn publish(&mut self, header: &FrameHeader, fill: impl FnOnce(&mut [u8])) -> u32 {
        let slot = self.next;
        self.next = (slot + 1) % self.layout.slots;
        let at = self.layout.slot_header_offset(slot) as usize;
        let seq = self.map.load_u64(at) | 1;
        self.map.store_u64(at, seq);
        // The odd sequence number is visible before any byte of the new frame.
        std::sync::atomic::fence(Ordering::SeqCst);
        let data = self.layout.slot_data_offset(slot) as usize;
        let bytes = self.map.as_mut_slice();
        fill(&mut bytes[data..data + header.len as usize]);
        let meta = ring::Slot {
            seq,
            frame: header.frame,
            width: header.width,
            height: header.height,
            device_width: header.device_width,
            device_height: header.device_height,
            len: header.len,
        };
        bytes[at + 8..at + ring::SLOT_HEADER_LEN].copy_from_slice(&meta.encode()[8..]);
        // Release: the pixels and the header before the even sequence number.
        self.map.store_u64(at, seq + 1);
        slot
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether no process `pid` exists (another user's live process answers EPERM, so isn't gone).
#[cfg(unix)]
fn process_gone(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid).is_none_or(|p| rustix::process::test_kill_process(p).is_err_and(|e| e == rustix::io::Errno::SRCH))
}

#[cfg(windows)]
fn process_gone(pid: i32) -> bool {
    u32::try_from(pid).is_ok_and(|p| !emu_hvf::host::process_alive(p))
}

/// Removes rings left by emulators that are gone (a crash skips their cleanup).
fn remove_stale_rings(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str().filter(|n| n.ends_with(".ring")) else { continue };
        let Some(pid) = name.strip_prefix("frames-").and_then(|n| n.split('-').next()?.parse::<i32>().ok()) else { continue };
        if process_gone(pid) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// Unix only: the tests drive the server over real Unix sockets, which Windows has none of yet
// (`sim_core::net`).
#[cfg(all(test, unix))]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use emu_vmm::display::{DisplaySink, Readback};
    use emu_vmm::virtio::input::{EV_KEY, EV_REL, InputEvent, Kind, REL_WHEEL};
    use sim_android::display::{Display, Frames, read_frame};
    use sim_core::pixels::downscale;

    use super::*;

    /// A frame's pixels, distinct per frame number.
    fn pixels(w: u32, h: u32, frame: u64) -> Vec<u8> {
        (0..w * h * 4).map(|i| (u64::from(i).wrapping_mul(2_654_435_761) >> 7) as u8 ^ frame as u8).collect()
    }

    /// A short directory (a unix socket address is at most 104 bytes on macOS), in memory where
    /// there's a tmpfs, so rings aren't timed against disk writeback.
    fn scratch(tag: &str) -> PathBuf {
        let root = ["/dev/shm", "/tmp"].into_iter().map(PathBuf::from).find(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!("emu-ds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        dir
    }

    struct Rig {
        dir: PathBuf,
        store: Arc<FrameStore>,
        reads: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        server: Option<DisplayServer>,
    }

    impl Rig {
        /// A display server over a store whose readback produces [`pixels`] for the latest frame.
        fn new(tag: &str) -> Rig {
            Rig::with(tag, None, None)
        }

        /// With these input devices, and this readback instead of [`pixels`].
        fn with(tag: &str, inputs: Option<emu_vmm::Inputs>, readback: Option<Readback>) -> Rig {
            let dir = scratch(tag);
            let store = Arc::new(FrameStore::default());
            let reads = Arc::new(AtomicUsize::new(0));
            let (counter, frames) = (reads.clone(), Arc::downgrade(&store));
            store.set_readback(readback.unwrap_or_else(|| {
                Box::new(move |w, h, buf| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    let n = frames.upgrade().map_or(0, |s| s.frames());
                    buf.copy_from_slice(&pixels(w, h, n));
                    true
                })
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let server = start(&dir, store.clone(), Arc::new(Mutex::new(inputs)), stop.clone());
            assert!(server.is_some(), "serving");
            Rig { dir, store, reads, stop, server }
        }

        fn connect(&self, shared: bool, max_width: u32) -> (Display, UnixStream) {
            let mut d = Display::connect(&self.dir).expect("connect");
            if shared {
                d.request_shared().expect("shm");
            }
            d.send(&Input::Hello { max_width }).expect("hello");
            let r = d.try_clone().expect("clone");
            r.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            (d, r)
        }

        fn rings(&self) -> Vec<String> {
            let Ok(entries) = std::fs::read_dir(self.dir.join("simee")) else { return Vec::new() };
            let mut names: Vec<String> =
                entries.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".ring")).collect();
            names.sort();
            names
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            drop(self.server.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn shared_frames_arrive_through_the_ring_and_follow_size_changes() {
        let rig = Rig::new("ring");
        let (_d, reader) = rig.connect(true, 720);
        let mut frames = Frames::new(reader);
        rig.store.posted(1440, 200);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, h.device_width, h.device_height, h.frame), (720, 100, 1440, 200, 1));
        assert_eq!(rgba, downscale(1440, 200, &pixels(1440, 200, 1), 2).2, "downscaled by 2 into the ring");
        let first = rig.rings();
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].starts_with(&format!("frames-{}-", std::process::id())), "{first:?}");
        // The same size reuses the ring, going round its slots.
        for n in 2..=4u64 {
            rig.store.posted(1440, 200);
            let (h, rgba) = frames.next_frame().expect("frame");
            assert_eq!(h.frame, n);
            assert_eq!(rgba, downscale(1440, 200, &pixels(1440, 200, n), 2).2);
        }
        assert_eq!(rig.rings(), first);
        // A larger frame needs a larger ring: a new one is announced and the old one removed.
        rig.store.posted(700, 1600);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, h.frame), (700, 1600, 5), "no downscale under the width");
        assert_eq!(rgba, pixels(700, 1600, 5));
        let second = rig.rings();
        assert_eq!(second.len(), 1);
        assert_ne!(second, first);
        assert_eq!(frames.torn, 0);
        // The ring goes when the client does.
        drop((frames, _d));
        wait_until("the ring's removal", || rig.rings().is_empty());
    }

    /// On a static screen a stream's frame thread waits with a 1 s backstop; a client going away
    /// wakes it, so its ring goes at once, not up to a second later.
    #[test]
    fn a_closed_client_ends_its_stream_at_once_on_a_static_screen() {
        let rig = Rig::new("close");
        let (d, reader) = rig.connect(true, 720);
        let mut frames = Frames::new(reader);
        rig.store.posted(1440, 200);
        frames.next_frame().expect("frame");
        // Let the frame thread go back to waiting for a frame that won't come.
        std::thread::sleep(Duration::from_millis(50));
        let closed = Instant::now();
        drop((frames, d));
        wait_until("the ring's removal", || rig.rings().is_empty());
        assert!(closed.elapsed() < Duration::from_millis(500), "took {:?}", closed.elapsed());
    }

    /// A real IOSurface as the store holds a `-gpu metal` frame, counting its holders' drops.
    #[cfg(target_os = "macos")]
    struct TestSurface(sim_coresim::Surface, Arc<AtomicUsize>);

    #[cfg(target_os = "macos")]
    impl SharedSurface for TestSurface {
        fn size(&self) -> (u32, u32) {
            self.0.size()
        }
        fn mach_port(&self) -> Option<u32> {
            self.0.mach_port()
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for TestSurface {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A store that copies each post into a new [`TestSurface`]: the IDs made, in order, and the
    /// count of those dropped.
    #[cfg(target_os = "macos")]
    fn with_surfaces(rig: &Rig) -> (Arc<Mutex<Vec<u32>>>, Arc<AtomicUsize>) {
        let (ids, dropped) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)));
        let (i, d) = (ids.clone(), dropped.clone());
        rig.store.set_surface_readback(Box::new(move |w, h| {
            let s = sim_coresim::Surface::new_bgra(w, h).ok()?;
            i.lock().expect("ids").push(s.id());
            Some(Arc::new(TestSurface(s, d.clone())) as Arc<dyn SharedSurface>)
        }));
        (ids, dropped)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn surface_clients_get_each_frame_through_their_port_and_hold_it_until_they_release_it() {
        let rig = Rig::new("surf");
        let (ids, dropped) = with_surfaces(&rig);
        let inbox = sim_coresim::SurfaceInbox::check_in(std::process::id() as i32).expect("check in");
        let mut d = Display::connect(&rig.dir).expect("connect");
        d.request_shared().expect("shm");
        d.send(&Input::Surfaces(inbox.name().to_owned())).expect("surfaces");
        d.send(&Input::Hello { max_width: u32::MAX }).expect("hello");
        let r = d.try_clone().expect("clone");
        r.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut frames = Frames::new(r);
        let mut buf = Vec::new();
        rig.store.posted(64, 32);
        let sim_android::display::Next::Surface { header, seq } = frames.newest_into(&mut buf, || true).expect("record") else {
            panic!("a surface record");
        };
        assert_eq!((header.width, header.height, header.frame), (64, 32, 1));
        let (got_seq, got) = inbox.recv(Duration::from_secs(1)).expect("recv").expect("the surface, before its record");
        assert_eq!((got_seq, got.id()), (seq, ids.lock().expect("ids")[0]), "the store's surface itself");
        assert_eq!(rig.reads.load(Ordering::Relaxed), 0, "nothing read back");
        // The next frame: the store lets the first go, the client still holds it.
        rig.store.posted(64, 32);
        let next = frames.newest_into(&mut buf, || true).expect("record");
        assert!(matches!(next, sim_android::display::Next::Surface { header, .. } if header.frame == 2), "{next:?}");
        assert!(inbox.recv(Duration::from_secs(1)).expect("recv").is_some());
        assert_eq!(dropped.load(Ordering::Relaxed), 0, "leased to the client");
        d.send(&Input::Release(seq)).expect("release");
        wait_until("the released surface to go", || dropped.load(Ordering::Relaxed) == 1);
        // A downscaled stream gets pixels: surfaces are full size only.
        d.send(&Input::Hello { max_width: 32 }).expect("hello");
        pixels_at(&rig, &mut frames, &mut d, &inbox, 32);
        // `surfaces -` (a client that can't use them) turns them off: full size comes as pixels.
        d.send(&Input::Surfaces(sim_core::stream::NO_SURFACES.into())).expect("off");
        d.send(&Input::Hello { max_width: u32::MAX }).expect("hello");
        pixels_at(&rig, &mut frames, &mut d, &inbox, 64);
    }

    /// Posts 64x32 frames until one comes as `width`-wide pixels (lines sent before take effect from
    /// some frame on), giving back the surfaces that come before it.
    #[cfg(target_os = "macos")]
    fn pixels_at(rig: &Rig, frames: &mut Frames<UnixStream>, d: &mut Display, inbox: &sim_coresim::SurfaceInbox, width: u32) {
        let mut buf = Vec::new();
        for _ in 0..100 {
            rig.store.posted(64, 32);
            match frames.newest_into(&mut buf, || true).expect("record") {
                sim_android::display::Next::Pixels(h) if h.width == width => return,
                sim_android::display::Next::Pixels(_) => {}
                sim_android::display::Next::Surface { seq, .. } => {
                    let _ = inbox.recv(Duration::from_secs(1)).expect("recv");
                    d.send(&Input::Release(seq)).expect("release");
                }
            }
        }
        panic!("no {width}-wide frame as pixels");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_surface_client_whose_port_is_gone_gets_ring_frames() {
        let rig = Rig::new("nosurf");
        let _ = with_surfaces(&rig);
        let mut d = Display::connect(&rig.dir).expect("connect");
        d.request_shared().expect("shm");
        d.send(&Input::Surfaces("dev.simee.surfaces.nobody".into())).expect("surfaces");
        d.send(&Input::Hello { max_width: u32::MAX }).expect("hello");
        let r = d.try_clone().expect("clone");
        r.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut frames = Frames::new(r);
        rig.store.posted(8, 4);
        let mut buf = Vec::new();
        let next = frames.newest_into(&mut buf, || true).expect("frame");
        assert!(matches!(next, sim_android::display::Next::Pixels(h) if (h.width, h.frame) == (8, 1)), "{next:?}");
        assert_eq!(buf, pixels(8, 4, 1), "read back as before");
    }

    #[test]
    fn socket_clients_get_the_same_frames_and_share_one_readback() {
        let rig = Rig::new("sock");
        // An old client: frames on the socket, read with the header-only decoder.
        let (_old, mut old) = rig.connect(false, 720);
        let (_new, shared) = rig.connect(true, 720);
        let mut shared = Frames::new(shared);
        std::thread::sleep(Duration::from_millis(50));
        rig.store.posted(1440, 200);
        let (h_old, px_old) = read_frame(&mut old).expect("socket frame");
        let (h_new, px_new) = shared.next_frame().expect("ring frame");
        assert_eq!((h_old, &px_old), (h_new, &px_new));
        // A screenshot of the same frame reads nothing back either.
        assert_eq!(rig.store.latest(|c| c.frame), Some(1));
        assert_eq!(rig.reads.load(Ordering::Relaxed), 1, "two streams and a capture, one readback");
        assert!(rig.rings().len() == 1, "only the shared client has a ring");
    }

    #[test]
    fn a_ring_is_announced_with_the_first_frame_sent_from_it() {
        // The first readback fails after the ring is made: the announcement waits for a frame.
        let tries = Arc::new(AtomicUsize::new(0));
        let t = tries.clone();
        let rig = Rig::with(
            "announce",
            None,
            Some(Box::new(move |w, h, buf| {
                if t.fetch_add(1, Ordering::Relaxed) == 0 {
                    return false;
                }
                buf.copy_from_slice(&pixels(w, h, 9));
                true
            })),
        );
        let (_d, reader) = rig.connect(true, 4096);
        let mut frames = Frames::new(reader);
        std::thread::sleep(Duration::from_millis(50));
        rig.store.posted(5, 3);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, rgba), (5, 3, pixels(5, 3, 9)));
        assert!(tries.load(Ordering::Relaxed) >= 2, "the first try failed");
        assert_eq!(frames.torn, 0);
    }

    #[test]
    fn rings_skip_taken_names_and_the_server_removes_its_own() {
        let rig = Rig::new("names");
        let dir = rig.dir.join("simee");
        // Names a previous process with this PID left behind.
        let next = RING_IDS.load(Ordering::Relaxed);
        for id in next..next + 3 {
            std::fs::write(dir.join(format!("{}{id}.ring", ring_prefix())), b"old").expect("write");
        }
        let ring = Ring::create(&dir, 100).expect("ring");
        assert!(std::fs::read(&ring.path).expect("ring").len() as u64 == ring.layout.file_len(), "written out in full");
        let other = dir.join("frames-1-0.ring");
        std::fs::write(&other, b"").expect("write");
        std::mem::forget(ring); // a stream still running at exit never drops its ring
        let mut rig = rig;
        drop(rig.server.take());
        assert!(rig.rings() == ["frames-1-0.ring"], "{:?}", rig.rings());
    }

    #[test]
    fn a_client_can_go_back_to_socket_frames() {
        let rig = Rig::new("back");
        // Lines are handled in order, so the stream starts after `socket` took effect.
        let mut d = Display::connect(&rig.dir).expect("connect");
        for input in [Input::Shared, Input::Socket, Input::Hello { max_width: 4096 }] {
            d.send(&input).expect("send");
        }
        let mut reader = d.try_clone().expect("clone");
        reader.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        rig.store.posted(3, 2);
        // The header-only decoder: an EMUR or EMUS here would be an error.
        let (h, rgba) = read_frame(&mut reader).expect("socket frame");
        assert_eq!((h.width, rgba), (3, pixels(3, 2, 1)));
        assert!(rig.rings().is_empty());
    }

    #[test]
    fn without_a_ring_shared_clients_get_frames_on_the_socket() {
        let rig = Rig::new("noring");
        let (_d, reader) = rig.connect(true, 4096);
        let mut frames = Frames::new(reader);
        std::thread::sleep(Duration::from_millis(50));
        // Nowhere to make a ring: the stream says so and falls back to the socket.
        std::fs::remove_dir_all(rig.dir.join("simee")).expect("rm");
        rig.store.posted(3, 2);
        let (h, rgba) = frames.next_frame().expect("frame");
        assert_eq!((h.width, h.height, h.frame, rgba), (3, 2, 1, pixels(3, 2, 1)));
        assert!(rig.rings().is_empty());
    }

    #[test]
    fn input_lines_reach_the_devices() {
        let (touch, th) = emu_vmm::virtio::input::Input::new(Kind::Touch { width: 720, height: 1280, screen: true }, "touch");
        let (keyboard, kh) = emu_vmm::virtio::input::Input::new(Kind::Keyboard, "keyboard");
        let (rotary, rh) = emu_vmm::virtio::input::Input::new(Kind::Rotary, "rotary");
        let inputs = emu_vmm::Inputs { screen: (720, 1280), touch: Some(th), keyboard: kh, rotary: rh, mouse: None, touchpad: None };
        let rig = Rig::with("input", Some(inputs), None);
        let mut d = Display::connect(&rig.dir).expect("connect");
        for input in [
            Input::Touch { x: 1.0, y: 2.0, phase: Phase::Up }, // no finger down: dropped
            Input::Touch { x: 10.0, y: 20.0, phase: Phase::Down },
            Input::Touch { x: 11.0, y: 21.0, phase: Phase::Move },
            Input::Touch { x: 12.0, y: 22.0, phase: Phase::Up },
            Input::Key("HOME".into()),
            Input::Key("NOT_A_KEY".into()),
            Input::Wheel(2),
        ] {
            d.send(&input).expect("send");
        }
        // Lines are handled in order: once the wheel is in, everything before it is too.
        let start = Instant::now();
        let wheel = loop {
            let got = rotary.take_queued();
            if !got.is_empty() {
                break got;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "no wheel");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(wheel, [vec![InputEvent::new(EV_REL, REL_WHEEL, 30), InputEvent::syn()]], "2 clicks, 15 units each");
        assert_eq!(touch.take_queued().len(), 3, "down, move, up; the stray up dropped");
        let home = sim_core::goldfish::keycode("HOME").expect("HOME");
        let keys: Vec<InputEvent> = keyboard.take_queued().into_iter().flatten().collect();
        assert!(keys.iter().any(|e| e.typ == EV_KEY && e.code == home), "{keys:?}");
        assert!(keys.iter().all(|e| e.typ != EV_KEY || e.code == home), "the unknown key sent nothing: {keys:?}");
        // No frames without hello.
        let mut r = d.try_clone().expect("clone");
        r.set_read_timeout(Some(Duration::from_millis(100))).expect("timeout");
        rig.store.posted(4, 4);
        assert!(std::io::Read::read(&mut r, &mut [0u8; 1]).is_err());
        assert_eq!(rig.reads.load(Ordering::Relaxed), 0);
    }

    /// `pixels(w, h, n)` turned clockwise by `r`.
    fn turned(w: u32, h: u32, n: u64, r: Rotation) -> Vec<u8> {
        let src = pixels(w, h, n);
        let mut out = vec![0; src.len()];
        sim_core::rotation::rotate_rgba_into(&src, w, h, r, &mut out);
        out
    }

    /// The records a raw socket reader gets up to and including the next frame, its pixels
    /// skipped.
    fn records_to_frame(r: &mut UnixStream) -> Vec<Record> {
        let mut got = Vec::new();
        loop {
            let mut hdr = [0u8; sim_core::stream::HEADER_LEN];
            std::io::Read::read_exact(r, &mut hdr).expect("record");
            let rec = Record::decode(&hdr).expect("decode");
            got.push(rec);
            if let Record::Frame(h) = rec {
                let mut px = vec![0; h.len as usize];
                std::io::Read::read_exact(r, &mut px).expect("pixels");
                return got;
            }
        }
    }

    #[test]
    fn a_turned_device_streams_upright_frames_to_clients_that_dont_turn_them() {
        let rig = Rig::new("upright");
        // A landscape panel (a tablet), turned a quarter clockwise: portrait frames.
        let (w, h) = (12u32, 8u32);
        rig.store.posted(w, h);
        rig.store.set_rotation(Rotation::Deg90);
        let n = rig.store.frames();
        let (_old, mut old) = rig.connect(false, 4096);
        let (h_old, px_old) = read_frame(&mut old).expect("socket frame");
        assert_eq!((h_old.width, h_old.height, h_old.device_width, h_old.device_height), (8, 12, 8, 12));
        assert_eq!(px_old, turned(w, h, n, Rotation::Deg90), "the header-only decoder sees an upright frame");
        // Through the ring, full size and downscaled.
        let (_full, full) = rig.connect(true, 4096);
        let (hf, pf) = Frames::new(full).next_frame().expect("ring frame");
        assert_eq!(((hf.width, hf.height), pf), ((8, 12), turned(w, h, n, Rotation::Deg90)));
        let (_half, half) = rig.connect(true, 4);
        let (hh, ph) = Frames::new(half).next_frame().expect("ring frame");
        assert_eq!((hh.width, hh.height, hh.device_width, hh.device_height), (4, 6, 8, 12));
        assert_eq!(ph, downscale(8, 12, &turned(w, h, n, Rotation::Deg90), 2).2);
        assert_eq!(rig.reads.load(Ordering::Relaxed), 1, "three streams, one readback and one turn");
    }

    #[test]
    fn clients_that_turn_frames_get_them_as_the_panel_lies_with_the_turn() {
        let rig = Rig::new("rotates");
        let (w, h) = (6u32, 10u32);
        rig.store.posted(w, h);
        // Raw records on the socket: the turn goes before the first frame and when it changes.
        let mut d = Display::connect(&rig.dir).expect("connect");
        d.request_rotation().expect("rotates");
        d.send(&Input::Hello { max_width: 4096 }).expect("hello");
        let mut r = d.try_clone().expect("clone");
        r.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let header = |recs: &[Record]| match recs.last() {
            Some(Record::Frame(h)) => (h.width, h.height),
            other => panic!("{other:?}"),
        };
        let recs = records_to_frame(&mut r);
        assert_eq!((recs[0], header(&recs)), (Record::Rotation(Rotation::Deg0), (6, 10)), "{recs:?}");
        rig.store.set_rotation(Rotation::Deg270);
        let recs = records_to_frame(&mut r);
        assert_eq!((recs.len(), recs[0], header(&recs)), (2, Record::Rotation(Rotation::Deg270), (6, 10)), "the panel's frame");
        rig.store.posted(w, h);
        assert_eq!(records_to_frame(&mut r).len(), 1, "no turn again while it holds");
        // Through the ring, the client reads the turn with the frame it applies to.
        let mut d = Display::connect(&rig.dir).expect("connect");
        d.request_rotation().expect("rotates");
        d.request_shared().expect("shm");
        d.send(&Input::Hello { max_width: 4096 }).expect("hello");
        let reader = d.try_clone().expect("clone");
        reader.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let mut frames = Frames::new(reader);
        let (hd, px) = frames.next_frame().expect("frame");
        assert_eq!(((hd.width, hd.height), frames.rotation), ((6, 10), Rotation::Deg270));
        assert_eq!(px, pixels(w, h, rig.store.frames()), "as the panel lies: nothing turned it");
        rig.store.set_rotation(Rotation::Deg180);
        frames.next_frame().expect("frame");
        assert_eq!(frames.rotation, Rotation::Deg180);
        assert_eq!(turned_copy_bytes(&rig.store), 0, "no stream asked for turned frames");
    }

    /// Bytes the store holds beyond the capture of a posted frame (whose pixels the renderer
    /// keeps): the turned copy, when anything made one.
    fn turned_copy_bytes(store: &FrameStore) -> usize {
        store.buffered_bytes() - store.latest(|c| c.rgba.capacity()).unwrap_or(0)
    }

    #[test]
    fn touches_on_a_turned_screen_land_where_they_would_unturned() {
        // Phone, landscape tablet, and a folded Pixel Fold's outer screen on its scanout.
        let cases: [((u32, u32), Option<emu_vmm::display::View>); 3] =
            [((720, 1280), None), ((1280, 800), None), ((2208, 1840), Some((629, 0, 949, 1840)))];
        for ((sw, sh), view) in cases {
            let (touch, th) = emu_vmm::virtio::input::Input::new(Kind::Touch { width: sw, height: sh, screen: true }, "touch");
            let (_k, kh) = emu_vmm::virtio::input::Input::new(Kind::Keyboard, "keyboard");
            let (rotary, rh) = emu_vmm::virtio::input::Input::new(Kind::Rotary, "rotary");
            let inputs = emu_vmm::Inputs { screen: (sw, sh), touch: Some(th), keyboard: kh, rotary: rh, mouse: None, touchpad: None };
            let rig = Rig::with(&format!("turn-touch-{sw}"), Some(inputs), None);
            rig.store.posted(sw, sh);
            rig.store.set_view(view);
            let (_, _, vw, vh) = view.unwrap_or((0, 0, sw, sh));
            let mut d = Display::connect(&rig.dir).expect("connect");
            // A tap at view pixel `p`, sent where the turned screen shows it.
            let tap = |d: &mut Display, r: Rotation, (x, y): (u32, u32)| {
                let (sx, sy) = r.panel_to_screen((x, y), (vw, vh));
                for phase in [Phase::Down, Phase::Up] {
                    d.send(&Input::Touch { x: sx as f32, y: sy as f32, phase }).expect("send");
                }
                // A marker after the touches: once it's in, they are.
                d.send(&Input::Wheel(1)).expect("wheel");
                let start = Instant::now();
                while rotary.take_queued().is_empty() {
                    assert!(start.elapsed() < Duration::from_secs(5), "no marker");
                    std::thread::sleep(Duration::from_millis(5));
                }
                touch.take_queued()
            };
            for p in [(0, 0), (vw - 1, vh - 1), (vw / 3, vh / 7)] {
                rig.store.set_rotation(Rotation::Deg0);
                let want = tap(&mut d, Rotation::Deg0, p);
                assert_eq!(want.len(), 2, "down and up");
                for r in [Rotation::Deg90, Rotation::Deg180, Rotation::Deg270] {
                    rig.store.set_rotation(r);
                    assert_eq!(tap(&mut d, r, p), want, "{r:?} {sw}x{sh} {view:?} at {p:?}");
                }
            }
        }
    }

    /// A client that asks for another width of a screen that isn't drawing gets the newest frame
    /// again at that size, at once: the new `hello` wakes the stream out of its wait for a frame
    /// (which times out after a second), so the answer comes well before that.
    #[test]
    fn a_new_hello_width_on_a_static_screen_resends_the_newest_frame_at_that_size_without_waiting() {
        let rig = Rig::new("rehello");
        let (w, h) = (8u32, 4u32);
        rig.store.posted(w, h);
        let (mut client, mut frames_in) = rig.connect(false, 4096);
        let (full, _) = read_frame(&mut frames_in).expect("frame");
        assert_eq!((full.width, full.height), (w, h));
        // The guest draws nothing more, and the stream is waiting for a frame that won't come.
        std::thread::sleep(Duration::from_millis(100));
        let asked = Instant::now();
        client.send(&Input::Hello { max_width: 4 }).expect("hello");
        let (half, rgba) = read_frame(&mut frames_in).expect("the same frame, resized");
        let took = asked.elapsed();
        assert_eq!((half.frame, half.width, half.height), (full.frame, 4, 2));
        assert_eq!(rgba, downscale(w, h, &pixels(w, h, rig.store.frames()), 2).2);
        assert!(took < Duration::from_millis(900), "took {took:?}: it waited out the frame timeout");
    }

    /// `hello 0` pauses a stream (the desktop's switched-away screen). While the guest keeps
    /// drawing, the paused stream waits for the client, not for frames: it neither spins nor
    /// counts as a reader, so the store frees its buffers; `hello <w>` sends the newest at once.
    #[test]
    fn a_paused_stream_waits_for_the_client_and_resumes_with_the_newest_frame() {
        let store = Arc::new(FrameStore::with_idle(Duration::from_millis(50)));
        let (w, h) = (8u32, 4u32);
        let present = |n: u64| store.present(w, h, w * 4, sim_core::pixels::PixelFormat::RGBA_VIRTIO, &pixels(w, h, n));
        let wants = Arc::new(Wants::default());
        wants.set_max_width(720);
        let stop = Arc::new(AtomicBool::new(false));
        let dir = scratch("pause");
        let (out, mut client) = UnixStream::pair().expect("pair");
        client.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let stream = {
            let (store, wants, stop, dir) = (store.clone(), wants.clone(), stop.clone(), dir.join("simee"));
            std::thread::spawn(move || frames(out, &store, &wants, &stop, &dir))
        };
        present(1);
        let (hd, rgba) = read_frame(&mut client).expect("frame");
        assert_eq!((hd.frame, rgba), (1, pixels(w, h, 1)));
        let with_capture = store.buffered_bytes();
        // Pause, then the guest draws on.
        wants.set_max_width(0);
        std::thread::sleep(Duration::from_millis(30));
        present(2);
        let passes = wants.passes.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        let spun = wants.passes.load(Ordering::Relaxed) - passes;
        assert!(spun <= 1, "a paused stream made {spun} passes in 300 ms");
        // Unread for longer than the store's idle time: the next present frees the capture.
        present(3);
        assert!(store.buffered_bytes() < with_capture, "a paused stream kept the store from idling");
        // Resume: the newest frame, without waiting for another.
        let resumed = Instant::now();
        wants.set_max_width(720);
        let (hd, rgba) = read_frame(&mut client).expect("frame after resume");
        assert_eq!((hd.frame, rgba), (3, pixels(w, h, 3)));
        assert!(resumed.elapsed() < Duration::from_millis(200), "took {:?}", resumed.elapsed());
        // Closing while paused ends the stream at once, too.
        wants.set_max_width(0);
        std::thread::sleep(Duration::from_millis(30));
        let closed = Instant::now();
        // As `client` does when the connection ends.
        wants.finish();
        store.wake();
        stream.join().expect("stream");
        assert!(closed.elapsed() < Duration::from_millis(500), "took {:?}", closed.elapsed());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frame_stats_report_every_hundred_frames() {
        let mut stats = Stats::default();
        let header = FrameHeader::new(2, 1, 4, 2, 1).expect("size");
        for i in 1..Stats::EVERY {
            assert_eq!(stats.add(&Out::Socket(header), Duration::from_millis(i as u64), Duration::ZERO), None);
        }
        let line = stats.add(&Out::Shared { header, slot: 0, announce: None }, Duration::from_millis(100), Duration::ZERO).expect("report");
        assert_eq!(
            line,
            "100 frames of 2x1 (device 4x2) by ring: capture+scale median 51.00 ms, max 100.00 ms; send median 0.00 ms, max 0.00 ms"
        );
        assert!(stats.capture.is_empty() && stats.send.is_empty(), "starts over");
    }

    #[test]
    fn stale_rings_go() {
        let dir = scratch("stale");
        // PIDs never reach this (Linux caps them at 2^22, macOS at 99999); our own ring stays.
        let mine = format!("frames-{}-0.ring", std::process::id());
        for name in ["frames-2147483646-0.ring", mine.as_str(), "frames-x-1.ring", "frames-2147483646-0.png"] {
            std::fs::write(dir.join("simee").join(name), b"").expect("write");
        }
        remove_stale_rings(&dir.join("simee"));
        let mut left: Vec<String> =
            std::fs::read_dir(dir.join("simee")).expect("dir").flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        left.sort();
        // Sorted the same way: where our own ring falls depends on the digits of our pid.
        let mut want = vec!["frames-2147483646-0.png".to_owned(), mine, "frames-x-1.ring".to_owned()];
        want.sort();
        assert_eq!(left, want);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    /// Moves `runs` frames of each size from this side to a client, on the socket (as before)
    /// and through a ring in each of `roots`, `pause` apart; one line per root and size.
    fn transport_bench(roots: &[PathBuf], sizes: &[(u32, u32)], runs: usize, pause: Duration) -> Vec<String> {
        let mut lines = Vec::new();
        for root in roots {
            let dir = root.join(format!("emu-transport-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            for &(w, h) in sizes {
                let px = pixels(w, h, 1);
                let header = FrameHeader::new(w, h, w, h, 1).expect("size");
                // Socket: header and rows written, read into a fresh Vec.
                let (mut tx, mut rx) = UnixStream::pair().expect("pair");
                let reader = std::thread::spawn(move || {
                    (0..runs).map(|_| (read_frame(&mut rx).expect("frame").1.len(), Instant::now())).collect::<Vec<_>>()
                });
                let (mut write, mut starts) = (Vec::new(), Vec::new());
                for _ in 0..runs {
                    std::thread::sleep(pause);
                    let t = Instant::now();
                    starts.push(t);
                    tx.write_all(&header.encode()).and_then(|()| tx.write_all(&px)).expect("write");
                    write.push(ms(t.elapsed()));
                }
                let got = reader.join().expect("reader");
                assert!(got.iter().all(|(n, _)| *n == px.len()));
                let socket = median(got.iter().zip(&starts).map(|((_, d), s)| ms(d.duration_since(*s))).collect());
                // Ring: rows copied into a slot (what a full-size readback writes there; a
                // downscale writes its output there), a 32-byte notice, the client's pread.
                let mut ring = Ring::create(&dir, u64::from(header.len)).expect("ring");
                let (mut tx, rx) = UnixStream::pair().expect("pair");
                let path = ring.path.as_os_str().as_encoded_bytes().to_vec();
                tx.write_all(&[Record::Ring { path_len: path.len() as u32 }.encode().as_slice(), &path].concat()).expect("announce");
                let reader = std::thread::spawn(move || {
                    let mut frames = Frames::new(rx);
                    (0..runs).map(|_| (frames.next_frame().expect("frame").1.len(), Instant::now())).collect::<Vec<_>>()
                });
                let (mut publish, mut starts) = (Vec::new(), Vec::new());
                for n in 0..runs {
                    std::thread::sleep(pause);
                    let t = Instant::now();
                    starts.push(t);
                    let header = FrameHeader { frame: n as u64 + 1, ..header };
                    let slot = ring.publish(&header, |dst| dst.copy_from_slice(&px));
                    tx.write_all(&Record::Shared { header, slot }.encode()).expect("notice");
                    publish.push(ms(t.elapsed()));
                }
                let got = reader.join().expect("reader");
                assert!(got.iter().all(|(n, _)| *n == px.len()));
                let ring_total = median(got.iter().zip(&starts).map(|((_, d), s)| ms(d.duration_since(*s))).collect());
                lines.push(format!(
                    "ring in {}: {w}x{h} ({:.1} MB): socket write {:.2} ms, write->read {socket:.2} ms | ring copy+notice {:.2} ms, ->client has it {ring_total:.2} ms",
                    root.display(),
                    f64::from(header.len) / 1e6,
                    median(write),
                    median(publish),
                ));
            }
            std::fs::remove_dir_all(dir).ok();
        }
        lines
    }

    /// Post to client-has-the-pixels through the display server, `runs` frames `pause` apart, for
    /// each (frame width, frame height, max width, shared) case. The readback is a memcpy of the
    /// frame (the last step of the renderer's own; its GPU copy isn't modelled). One line each,
    /// with every sample when `samples`.
    fn latency_bench(cases: &[(u32, u32, u32, bool)], runs: usize, pause: Duration, samples: bool) -> Vec<String> {
        let mut lines = Vec::new();
        for &(w, h, max_width, shared) in cases {
            let src = pixels(w, h, 0);
            let rig = Rig::with(
                &format!("lat{w}{max_width}{shared}"),
                None,
                Some(Box::new(move |_, _, buf| {
                    buf.copy_from_slice(&src);
                    true
                })),
            );
            let (_d, reader) = rig.connect(shared, max_width);
            let mut frames = Frames::new(reader);
            std::thread::sleep(Duration::from_millis(50));
            let mut lat = Vec::new();
            for _ in 0..runs {
                std::thread::sleep(pause);
                let t = Instant::now();
                rig.store.posted(w, h);
                std::hint::black_box(frames.next_frame().expect("frame"));
                lat.push(ms(t.elapsed()));
            }
            let all = if samples { format!(" {lat:.1?}") } else { String::new() };
            let way = if shared { "ring  " } else { "socket" };
            lines.push(format!("{w}x{h} hello {max_width} {way}: post -> client has pixels {:.2} ms (median of {runs}){all}", median(lat)));
        }
        lines
    }

    /// The guest posting steadily every `cadence` (an animation, a scroll), `runs` frames, a client
    /// reading as fast as it can: post -> client has the pixels for each frame it got, and how
    /// many of the posted frames reached it. `before` lines go to the server ahead of `hello`.
    fn cadence_bench(cases: &[(u32, u32, u32, bool)], runs: usize, cadence: Duration, before: &[&str]) -> Vec<String> {
        let mut lines = Vec::new();
        for &(w, h, max_width, shared) in cases {
            let src = pixels(w, h, 0);
            let rig = Rig::with(
                &format!("cad{w}{max_width}{shared}"),
                None,
                Some(Box::new(move |_, _, buf| {
                    buf.copy_from_slice(&src);
                    true
                })),
            );
            let mut d = Display::connect(&rig.dir).expect("connect");
            let mut raw = d.try_clone().expect("clone");
            for line in before {
                raw.write_all(format!("{line}\n").as_bytes()).expect("line");
            }
            if shared {
                d.request_shared().expect("shm");
            }
            d.send(&Input::Hello { max_width }).expect("hello");
            let r = d.try_clone().expect("clone");
            r.set_read_timeout(Some(Duration::from_secs(1))).expect("timeout");
            std::thread::sleep(Duration::from_millis(50));
            let posts: Arc<Mutex<Vec<Instant>>> = Arc::default();
            let (p, store) = (posts.clone(), rig.store.clone());
            let poster = std::thread::spawn(move || {
                let start = Instant::now();
                for n in 1..=runs as u32 {
                    // Steady posts, as a guest's vsync would: on the grid, not drifting.
                    if let Some(wait) = (start + cadence * n).checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    p.lock().expect("posts").push(Instant::now());
                    store.posted(w, h);
                }
            });
            let mut frames = Frames::new(r);
            let mut lat = Vec::new();
            while let Ok((header, px)) = frames.next_frame() {
                let at = Instant::now();
                std::hint::black_box(px);
                if let Some(t) = posts.lock().expect("posts").get(header.frame as usize - 1) {
                    lat.push(ms(at.duration_since(*t)));
                }
                if header.frame as usize >= runs {
                    break;
                }
            }
            poster.join().expect("poster");
            let way = if shared { "ring  " } else { "socket" };
            let got = lat.len();
            let all = if std::env::var_os("SAMPLES").is_some() { format!(" {lat:.1?}") } else { String::new() };
            lines.push(format!(
                "{w}x{h} hello {max_width} {way} posts every {} ms: {got}/{runs} frames delivered, post -> client has pixels median {:.2} ms, max {:.2} ms{all}",
                cadence.as_millis(),
                median(lat.clone()),
                lat.iter().copied().fold(0.0, f64::max),
            ));
        }
        lines
    }

    #[test]
    fn clients_can_set_their_frame_rate() {
        assert_eq!(parse_fps("fps 120"), Some(120));
        assert_eq!(parse_fps(" fps  30 "), Some(30));
        for bad in ["fps 0", "fps 241", "fps", "fps 60 1", "fps x", "hello 60"] {
            assert_eq!(parse_fps(bad), None, "{bad}");
        }
        let w = Wants::default();
        assert_eq!(w.interval(), DEFAULT_INTERVAL);
        w.fps.store(120, Ordering::Relaxed);
        assert_eq!(w.interval(), Duration::from_secs(1) / 120);
        // A client asking for 10 fps of 60 Hz content gets about every sixth frame, the newest
        // each time; the first goes out at once. The line isn't `sim_core::stream`'s, so a
        // stream that doesn't send it is unchanged.
        let lines = cadence_bench(&[(16, 8, 8, true)], 30, Duration::from_millis(16), &["fps 10"]);
        let got: usize = lines[0].split('/').next().and_then(|s| s.rsplit(' ').next()).and_then(|n| n.parse().ok()).expect("count");
        assert!((2..=8).contains(&got), "{lines:?}");
    }

    #[test]
    fn the_benchmarks_run() {
        let lines = transport_bench(&[scratch("bench")], &[(8, 4)], 3, Duration::ZERO);
        assert!(lines.len() == 1 && lines[0].contains("8x4"), "{lines:?}");
        let lines = latency_bench(&[(16, 8, 8, true), (16, 8, 8, false)], 3, DEFAULT_INTERVAL, true);
        assert!(lines.len() == 2 && lines.iter().all(|l| l.contains("median of 3")), "{lines:?}");
        let lines = cadence_bench(&[(16, 8, 8, true)], 3, Duration::from_millis(16), &[]);
        assert!(lines.len() == 1 && lines[0].contains("/3 frames delivered"), "{lines:?}");
    }

    /// Moving one frame's RGBA rows from the emulator to a client, on the socket and through the
    /// ring, on disk and (where there's one) on a tmpfs.
    ///     cargo test --release -p simee-emu transport_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn transport_cost() {
        let roots: Vec<PathBuf> = [std::env::temp_dir(), PathBuf::from("/dev/shm")].into_iter().filter(|d| d.is_dir()).collect();
        for l in transport_bench(&roots, &[(2480, 1740), (1080, 2400), (826, 580), (360, 800)], 11, Duration::from_millis(20)) {
            println!("{l}");
        }
    }

    /// Post to client-has-the-pixels for the desktop's request (`hello 1080`) and a full-size
    /// one, on the socket and through the ring. `CASE=<w>,<max_width>,<shared>` runs one case
    /// alone (a long run of many rigs drifts); `SAMPLES=1` prints every sample (each ring's first
    /// three frames include its slots' first page faults).
    ///     cargo test --release -p simee-emu frame_path_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn frame_path_latency() {
        let case = std::env::var("CASE").ok();
        let mut cases = Vec::new();
        for (w, h) in [(2480u32, 1740u32), (1080, 2400)] {
            for max_width in [1080, 4096] {
                for shared in [false, true] {
                    if case.as_ref().is_none_or(|c| *c == format!("{w},{max_width},{shared}")) {
                        cases.push((w, h, max_width, shared));
                    }
                }
            }
        }
        for l in latency_bench(&cases, 11, Duration::from_millis(40), std::env::var_os("SAMPLES").is_some()) {
            println!("{l}");
        }
        // A steady 16 ms post cadence (60 Hz content): latency and frames delivered.
        for l in cadence_bench(&cases, 120, Duration::from_millis(16), &[]) {
            println!("{l}");
        }
    }
}
