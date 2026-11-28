//! The guest display as the host sees it: finished scanout frames from the GPU.
//!
//! Frames reach the host two ways. Host composition posts them to the renderer, which keeps them:
//! the post only records the frame's size ([`DisplaySink::posted`]), and the pixels are read back
//! on demand through the renderer ([`DisplaySink::set_readback`]), once per posted frame however
//! many streams and screenshots want it ([`FrameStore::latest`]). Guest-composed scanouts
//! (virtio-gpu `RESOURCE_FLUSH`) arrive as pixels ([`DisplaySink::present`]), read straight into
//! the store ([`DisplaySink::present_with`]).
//!
//! While frames are being read, every buffer is reused: no allocation per frame. A store nobody
//! has read for [`IDLE`] gives its frame-sized buffers back (about 10 MB each at 1080x2400): the
//! capture, the spare buffer, and the kept pixels of a posted frame (which never needs them). A
//! stream waiting in [`FrameStore::wait_newer`] counts as a reader, so a connected stream on a
//! static screen keeps its buffers. The check runs on the next post, so it needs no thread or
//! timer of its own. Unread guest-composed frames are then read in place into the kept buffer,
//! one buffer instead of three.
//!
//! A guest that rotated its display ([`FrameStore::set_rotation`]) still draws into the panel as
//! it lies: readers that turn frames as they draw (the desktop, on the GPU) take them that way
//! ([`FrameStore::latest`]), and the rest get them turned upright ([`FrameStore::latest_upright`]),
//! turned once per frame into a buffer of their own that every such reader shares. An unturned
//! display never touches that buffer.
//!
//! Under `-gpu metal` a posted frame can also be taken as a shared surface the renderer's GPU
//! copied it into ([`FrameStore::latest_surface`], [`DisplaySink::set_surface_readback`]): once
//! per frame however many streams ask, held while anyone holds it, and none for a presented
//! frame or a view, which are read as pixels as before.
//!
//! A renderer without GL can't report its posts (gfxstream's post callback reads every post back
//! through GL); [`PostTicker`] stands in, driven by the renderer's fences.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use sim_core::pixels::{PixelFormat, to_rgba_into};
use sim_core::rotation::{Rotation, rotate_rgba_into};

/// Reads the last posted frame back as tightly packed RGBA, top row first, `width` x `height`,
/// into the buffer (exactly `width * height * 4` bytes). False when there's nothing to read.
pub type Readback = Box<dyn Fn(u32, u32, &mut [u8]) -> bool + Send + Sync>;

/// A posted frame the renderer's GPU copied into a shared surface (an IOSurface, `-gpu metal`):
/// BGRA, the whole scanout as the panel lies. The surface isn't written again while any handle
/// to it lives.
pub trait SharedSurface: Send + Sync {
    fn size(&self) -> (u32, u32);
    /// A new Mach send right for the surface, owned by the caller; `None` if none could be made.
    fn mach_port(&self) -> Option<u32>;
}

/// Copies the last posted frame (`width` x `height`) into a shared surface; `None` when it can't
/// (no free surface, the path is off): readers then take its pixels from the [`Readback`].
pub type SurfaceReadback = Box<dyn Fn(u32, u32) -> Option<Arc<dyn SharedSurface>> + Send + Sync>;

/// Receives finished scanout frames (BGRA/RGBA rows, `stride` bytes apart).
pub trait DisplaySink: Send + Sync {
    fn present(&self, width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]);

    /// [`DisplaySink::present`] for a frame whose rows arrive bottom row first (GL readback),
    /// tightly packed.
    fn present_bottom_up(&self, width: u32, height: u32, format: u32, pixels: &[u8]) {
        let row = width as usize * 4;
        let flipped: Vec<u8> = pixels.chunks_exact(row.max(1)).rev().flatten().copied().collect();
        self.present(width, height, width * 4, format, &flipped);
    }

    /// [`DisplaySink::present`] for a frame `fill` writes into the buffer it's given
    /// (`stride * height` bytes): a readback lands in the sink's own buffer with no second copy.
    /// Nothing is presented when `fill` returns false.
    fn present_with(&self, width: u32, height: u32, stride: u32, format: u32, fill: &mut dyn FnMut(&mut [u8]) -> bool) {
        let mut buf = vec![0; stride as usize * height as usize];
        if fill(&mut buf) {
            self.present(width, height, stride, format, &buf);
        }
    }

    /// A `width` x `height` frame was posted whose pixels the [`Readback`] produces on demand:
    /// only its size and number are recorded, nothing is copied.
    fn posted(&self, width: u32, height: u32) {
        let _ = (width, height);
    }

    /// How posted frames are read back (see [`DisplaySink::posted`]).
    fn set_readback(&self, readback: Readback) {
        let _ = readback;
    }

    /// How posted frames are copied into shared surfaces (`-gpu metal`), besides being read back.
    fn set_surface_readback(&self, readback: SurfaceReadback) {
        let _ = readback;
    }

    /// The guest reset: the screen goes black until the next boot shows a frame, rather than
    /// keep the last boot's final frame (or a posted frame the renderer has since freed).
    fn blank(&self) {}
}

/// How long a store goes unread before it frees its frame-sized buffers.
pub const IDLE: Duration = Duration::from_secs(5);

/// Longest a reader waits for an unread frame being filled in place (see [`FrameStore`]).
const FILL_WAIT: Duration = Duration::from_secs(1);

/// Milliseconds since the first call in this process, plus one (0 means "never").
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Keeps the latest frame (for screenshots and the desktop app) and counts frames.
pub struct FrameStore {
    inner: Mutex<FrameState>,
    /// Signalled on every new frame, so streams wake per frame rather than poll.
    posted: Condvar,
    readback: OnceLock<Readback>,
    /// The last frame read back or converted, shared by every reader.
    capture: Mutex<Capture>,
    /// The buffer the next guest-composed frame is read into while the last one stays
    /// readable (double buffering for [`DisplaySink::present_with`]).
    spare: Mutex<Vec<u8>>,
    /// When a reader last asked for pixels ([`now_ms`]; 0: never).
    last_read: AtomicU64,
    /// How long unread before the buffers go ([`IDLE`]; shorter in tests).
    idle_ms: u64,
    /// The part of the scanout readers see (x, y, width, height): a folded foldable's outer
    /// screen. None: all of it.
    view: Mutex<Option<View>>,
    /// How far the guest rotated its display: the view turned by this much is its layout upright.
    rotation: Mutex<Rotation>,
    /// The capture turned upright, for readers that don't turn frames themselves, and the turn
    /// it was made with; filled only while the display is turned.
    upright: Mutex<(Capture, Rotation)>,
    surface_readback: OnceLock<SurfaceReadback>,
    /// The frame number the last shared surface was taken for, and the surface (None: it couldn't
    /// be had for that frame).
    surface: Mutex<(u64, Option<Arc<dyn SharedSurface>>)>,
}

/// A rectangle of the scanout: x, y, width, height.
pub type View = (u32, u32, u32, u32);

/// `view` clipped to a `width` x `height` frame; None when it covers all of it or nothing.
fn clip_view(view: Option<View>, width: u32, height: u32) -> Option<View> {
    let (x, y, w, h) = view?;
    let (x, y) = (x.min(width), y.min(height));
    let (w, h) = (w.min(width - x), h.min(height - y));
    (w > 0 && h > 0 && (x, y, w, h) != (0, 0, width, height)).then_some((x, y, w, h))
}

impl Default for FrameStore {
    fn default() -> Self {
        FrameStore::with_idle(IDLE)
    }
}

#[derive(Default, Clone)]
pub struct FrameState {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// virtio-gpu format of `pixels`.
    pub format: u32,
    /// The presented frame's pixels; empty for a posted frame, whose pixels the renderer keeps.
    pub pixels: Vec<u8>,
    pub frames: u64,
    /// The latest frame was posted to the renderer (its pixels come from the [`Readback`]), not
    /// presented as pixels.
    pub posted: bool,
    /// Bumped by [`FrameStore::wake`]: ends every [`FrameStore::wait_newer`] in progress.
    pub wakes: u64,
    /// An unread frame is being read into `pixels` in place (taken out meanwhile); readers wait.
    pub filling: bool,
}

/// The latest frame as tightly packed RGBA, top row first.
#[derive(Default)]
pub struct Capture {
    pub width: u32,
    pub height: u32,
    /// The frame number it was captured for.
    pub frame: u64,
    pub rgba: Vec<u8>,
}

/// Cuts `view` out of the RGBA frame `rgba` (`width` pixels a row) in place: every row moves to an
/// offset no later than where it was, so a forward copy never overwrites a row still to come.
fn crop_in_place(rgba: &mut Vec<u8>, width: u32, (x, y, w, h): View) {
    let (row, src_row, x) = (w as usize * 4, width as usize * 4, x as usize * 4);
    for r in 0..h as usize {
        let from = (y as usize + r) * src_row + x;
        rgba.copy_within(from..from + row, r * row);
    }
    rgba.truncate(row * h as usize);
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl FrameStore {
    /// A store that frees its buffers after `idle` without a reader.
    pub fn with_idle(idle: Duration) -> FrameStore {
        FrameStore {
            inner: Mutex::default(),
            posted: Condvar::new(),
            readback: OnceLock::new(),
            capture: Mutex::default(),
            spare: Mutex::default(),
            last_read: AtomicU64::new(0),
            idle_ms: idle.as_millis() as u64,
            view: Mutex::new(None),
            rotation: Mutex::new(Rotation::Deg0),
            upright: Mutex::default(),
            surface_readback: OnceLock::new(),
            surface: Mutex::new((0, None)),
        }
    }

    pub fn snapshot(&self) -> FrameState {
        lock(&self.inner).clone()
    }

    /// Shows readers only `view` of the scanout (None: all of it), from the latest frame on: the
    /// frame is counted again, so streams on a static screen send it at the new size.
    pub fn set_view(&self, view: Option<View>) {
        let mut v = lock(&self.view);
        if *v == view {
            return;
        }
        *v = view;
        drop(v);
        self.recount();
    }

    /// Where readers' (0, 0) is on the scanout: the view's origin, clipped to the latest frame.
    pub fn view_origin(&self) -> (u32, u32) {
        let (w, h, _) = self.full_geometry();
        clip_view(*lock(&self.view), w, h).map_or((0, 0), |(x, y, ..)| (x, y))
    }

    /// The guest rotated its display by `rotation` (as its log reports it, not the
    /// accelerometer: the guest may not follow that), from the latest frame on: the frame is
    /// counted again, so streams on a static screen send it turned.
    pub fn set_rotation(&self, rotation: Rotation) {
        let mut r = lock(&self.rotation);
        if *r == rotation {
            return;
        }
        *r = rotation;
        drop(r);
        self.recount();
    }

    pub fn rotation(&self) -> Rotation {
        *lock(&self.rotation)
    }

    /// The latest frame counted again (a new view or turn): waiting streams send it.
    fn recount(&self) {
        let mut s = lock(&self.inner);
        if s.frames > 0 {
            s.frames += 1;
        }
        drop(s);
        self.posted.notify_all();
    }

    /// The scanout pixel under point `(x, y)` of the screen readers are shown upright (the view,
    /// turned as the guest's display is, which is uiautomator's): what a touch there presses. The
    /// view's size is the latest frame's; before one, points are taken as scanout pixels.
    pub fn to_scanout(&self, x: f64, y: f64) -> (i32, i32) {
        let (w, h, frames) = self.geometry();
        let (ox, oy) = self.view_origin();
        if frames == 0 || w == 0 || h == 0 {
            return (x as i32 + ox as i32, y as i32 + oy as i32);
        }
        let (px, py) = self.rotation().screen_to_panel((x, y), (w, h));
        (px as i32 + ox as i32, py as i32 + oy as i32)
    }

    /// [`FrameStore::geometry`] as the frames of [`FrameStore::latest_upright`] are: turned.
    pub fn upright_geometry(&self) -> (u32, u32, u64) {
        let (w, h, n) = self.geometry();
        let (w, h) = self.rotation().turned((w, h));
        (w, h, n)
    }

    /// The latest frame's width, height and number as readers see it (the view, if one is set),
    /// without copying its pixels.
    pub fn geometry(&self) -> (u32, u32, u64) {
        let (w, h, n) = self.full_geometry();
        match clip_view(*lock(&self.view), w, h) {
            Some((_, _, vw, vh)) => (vw, vh, n),
            None => (w, h, n),
        }
    }

    fn full_geometry(&self) -> (u32, u32, u64) {
        let s = lock(&self.inner);
        (s.width, s.height, s.frames)
    }

    /// Blocks until a frame newer than number `seen` is presented, [`FrameStore::wake`] is
    /// called, or `timeout` passes; returns the latest frame number either way. A waiter counts
    /// as a reader: the store keeps its buffers for the frame it's waiting for.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> u64 {
        self.mark_read();
        let s = lock(&self.inner);
        let wakes = s.wakes;
        let (s, _) =
            self.posted.wait_timeout_while(s, timeout, |s| s.frames <= seen && s.wakes == wakes).unwrap_or_else(|e| e.into_inner());
        // A long wait: still reading when it ends.
        self.mark_read();
        s.frames
    }

    /// Ends every [`FrameStore::wait_newer`] in progress now (a stream's client went away, or the
    /// server is stopping), whatever frame it waits for.
    pub fn wake(&self) {
        lock(&self.inner).wakes += 1;
        self.posted.notify_all();
    }

    fn mark_read(&self) {
        self.last_read.store(now_ms(), Ordering::Relaxed);
    }

    /// Nobody has read a frame for `idle_ms` (or ever).
    fn idle(&self) -> bool {
        let last = self.last_read.load(Ordering::Relaxed);
        last == 0 || now_ms().saturating_sub(last) >= self.idle_ms
    }

    /// Frees the capture and the spare buffer if the store has gone unread. Never waits: a
    /// buffer someone holds is in use, so it stays.
    fn release_if_idle(&self) {
        if !self.idle() {
            return;
        }
        if let Ok(mut cap) = self.capture.try_lock()
            && cap.rgba.capacity() > 0
        {
            cap.rgba = Vec::new();
            cap.frame = 0;
        }
        if let Ok(mut spare) = self.spare.try_lock()
            && spare.capacity() > 0
        {
            *spare = Vec::new();
        }
        if let Ok(mut up) = self.upright.try_lock()
            && up.0.rgba.capacity() > 0
        {
            up.0.rgba = Vec::new();
            up.0.frame = 0;
        }
        // The pool's surface goes back to it.
        if let Ok(mut s) = self.surface.try_lock() {
            *s = (0, None);
        }
    }

    /// Whether posted frames can be taken as shared surfaces ([`FrameStore::latest_surface`]).
    pub fn has_surfaces(&self) -> bool {
        self.surface_readback.get().is_some()
    }

    /// The latest frame as a shared surface the renderer's GPU copied it into, with its number:
    /// copied once per frame however many readers ask, and kept for that frame while anyone holds
    /// it. `None` for a presented frame, a view (a folded foldable's outer screen), a store without
    /// [`DisplaySink::set_surface_readback`], or when no surface could be had for this frame:
    /// readers then take its pixels ([`FrameStore::latest`], [`FrameStore::read_into`]).
    pub fn latest_surface(&self) -> Option<(u64, Arc<dyn SharedSurface>)> {
        let copy = self.surface_readback.get()?;
        self.mark_read();
        let (width, height, frame, posted) = self.state();
        if !posted || frame == 0 || width == 0 || height == 0 || clip_view(*lock(&self.view), width, height).is_some() {
            return None;
        }
        let mut kept = lock(&self.surface);
        if kept.0 != frame {
            // The last frame's surface goes back first: a pool with one free surface still serves.
            *kept = (frame, None);
            kept.1 = copy(width, height).filter(|s| s.size() == (width, height));
        }
        kept.1.clone().map(|s| (frame, s))
    }

    /// Bytes of frame buffers the store holds (kept pixels, capture, spare), for tests and
    /// diagnostics.
    pub fn buffered_bytes(&self) -> usize {
        let pixels = lock(&self.inner).pixels.capacity();
        pixels + lock(&self.capture).rgba.capacity() + lock(&self.spare).capacity() + lock(&self.upright).0.rgba.capacity()
    }

    pub fn frames(&self) -> u64 {
        lock(&self.inner).frames
    }

    /// Runs `f` on the latest frame as RGBA. The frame is read back (or converted from presented
    /// pixels) only when it is newer than the last capture, so streams and screenshots of the
    /// same frame share one readback. Readers of the same frame wait for each other while `f`
    /// runs; the guest's posts never wait. `None` before the first frame, or when the frame can't
    /// be read.
    pub fn latest<R>(&self, f: impl FnOnce(&Capture) -> R) -> Option<R> {
        self.mark_read();
        let (width, height, frame, posted) = self.state();
        if frame == 0 || width == 0 || height == 0 {
            return None;
        }
        let view = clip_view(*lock(&self.view), width, height);
        let (seen_w, seen_h) = view.map_or((width, height), |(_, _, w, h)| (w, h));
        let mut cap = lock(&self.capture);
        if (cap.frame, cap.width, cap.height) != (frame, seen_w, seen_h) {
            let len = width as usize * height as usize * 4;
            // A posted frame is the renderer's; a presented one is only in the store (the renderer
            // would answer with whatever it posted last, maybe before a guest reboot).
            let read = if posted {
                self.readback.get().is_some_and(|r| {
                    cap.rgba.resize(len, 0);
                    r(width, height, &mut cap.rgba)
                })
            } else {
                self.presented_rgba(width, height, &mut cap.rgba)
            };
            if !read {
                cap.frame = 0;
                return None;
            }
            if let Some(view) = view {
                crop_in_place(&mut cap.rgba, width, view);
            }
            (cap.width, cap.height, cap.frame) = (seen_w, seen_h, frame);
        }
        Some(f(&cap))
    }

    /// [`FrameStore::latest`] turned upright, as a viewer of the turned device sees it (a screenshot,
    /// a stream to a client that doesn't turn frames itself). Unturned, it is the capture itself;
    /// turned, the capture is turned once per frame into a buffer every such reader shares.
    pub fn latest_upright<R>(&self, f: impl FnOnce(&Capture) -> R) -> Option<R> {
        self.latest(|cap| {
            // Read after the frame's number: a turn counts the frame again after it's set, so a
            // frame numbered after a turn is never turned the old way.
            let rotation = self.rotation();
            if rotation == Rotation::Deg0 {
                return f(cap);
            }
            let (w, h) = rotation.turned((cap.width, cap.height));
            let mut up = lock(&self.upright);
            // Turned for this frame, this way (a square view keeps its size across a turn).
            if (up.0.frame, up.0.width, up.0.height, up.1) != (cap.frame, w, h, rotation) {
                let (turned, _) = &mut *up;
                turned.rgba.resize(cap.rgba.len(), 0);
                rotate_rgba_into(&cap.rgba, cap.width, cap.height, rotation, &mut turned.rgba);
                (turned.width, turned.height, turned.frame) = (w, h, cap.frame);
                up.1 = rotation;
            }
            f(&up.0)
        })
    }

    /// Frame `frame` (`width` x `height`, the latest [`FrameStore::geometry`] gave) as RGBA into
    /// `dst` (`width * height * 4` bytes), for a consumer that wants the whole frame and nothing
    /// else, like a full-size stream writing into shared memory: copied from the capture if it
    /// holds that frame, else read back from the renderer straight into `dst`, skipping the
    /// capture (a later [`FrameStore::latest`] reads the frame back again). False when the frame
    /// can't be read.
    pub fn read_into(&self, width: u32, height: u32, frame: u64, dst: &mut [u8]) -> bool {
        if dst.len() != width as usize * height as usize * 4 || frame == 0 {
            return false;
        }
        self.mark_read();
        {
            let cap = lock(&self.capture);
            if (cap.frame, cap.width, cap.height) == (frame, width, height) {
                dst.copy_from_slice(&cap.rgba);
                return true;
            }
        }
        let (full_w, full_h, _, posted) = self.state();
        // A view is cut from the whole frame, which the capture reads.
        let viewed = clip_view(*lock(&self.view), full_w, full_h).is_some();
        if posted && !viewed {
            return self.readback.get().is_some_and(|r| r(width, height, dst));
        }
        let copied = self.latest(|c| {
            let same = (c.width, c.height) == (width, height);
            if same {
                dst.copy_from_slice(&c.rgba);
            }
            same
        });
        copied == Some(true)
    }

    fn state(&self) -> (u32, u32, u64, bool) {
        let s = lock(&self.inner);
        (s.width, s.height, s.frames, s.posted)
    }

    /// The presented pixels of a `width` x `height` frame, as RGBA.
    fn presented_rgba(&self, width: u32, height: u32, out: &mut Vec<u8>) -> bool {
        let s = lock(&self.inner);
        let (s, _) = self.posted.wait_timeout_while(s, FILL_WAIT, |s| s.filling).unwrap_or_else(|e| e.into_inner());
        if (s.width, s.height) != (width, height) || s.pixels.is_empty() {
            return false;
        }
        let format = PixelFormat::from_virtio_gpu(s.format).unwrap_or(PixelFormat::Rgba);
        to_rgba_into(&s.pixels, width as usize, height as usize, s.stride as usize, format, out)
    }

    fn update(&self, width: u32, height: u32, stride: u32, format: u32, posted: bool, fill: impl FnOnce(&mut Vec<u8>) -> bool) {
        let mut s = lock(&self.inner);
        if !fill(&mut s.pixels) {
            return;
        }
        (s.width, s.height, s.stride, s.format, s.posted) = (width, height, stride, format, posted);
        s.frames += 1;
        drop(s);
        self.posted.notify_all();
        self.release_if_idle();
    }

    /// [`DisplaySink::present_with`] for a store nobody reads: the frame is read straight into
    /// the kept buffer (taken out of the store meanwhile, so the fill still runs unlocked), and
    /// no spare is kept. A reader that arrives during the fill waits for it. A failed fill
    /// leaves no readable pixels, since it may have overwritten them.
    fn present_in_place(&self, width: u32, height: u32, stride: u32, format: u32, fill: &mut dyn FnMut(&mut [u8]) -> bool) {
        let mut buf = {
            let mut s = lock(&self.inner);
            s.filling = true;
            std::mem::take(&mut s.pixels)
        };
        buf.resize(stride as usize * height as usize, 0);
        let ok = fill(&mut buf);
        let mut s = lock(&self.inner);
        s.filling = false;
        if ok {
            s.pixels = buf;
            (s.width, s.height, s.stride, s.format, s.posted) = (width, height, stride, format, false);
            s.frames += 1;
        } else if !s.posted {
            // Torn by the failed fill: nothing to show until the next frame. (A post that landed
            // during the fill doesn't need the pixels.)
            buf.clear();
            s.pixels = buf;
        }
        drop(s);
        self.posted.notify_all();
        self.release_if_idle();
    }
}

impl DisplaySink for FrameStore {
    fn present(&self, width: u32, height: u32, stride: u32, format: u32, pixels: &[u8]) {
        self.update(width, height, stride, format, false, |kept| {
            kept.clear();
            kept.extend_from_slice(pixels);
            true
        });
    }

    /// Row by row, reversed, straight into the kept buffer: one copy, each row a `memcpy`.
    fn present_bottom_up(&self, width: u32, height: u32, format: u32, pixels: &[u8]) {
        let row = width as usize * 4;
        self.update(width, height, width * 4, format, false, |kept| {
            kept.clear();
            if row > 0 {
                for r in pixels.chunks_exact(row).rev() {
                    kept.extend_from_slice(r);
                }
            }
            true
        });
    }

    /// Into a spare buffer that then trades places with the kept one: one copy, and the last
    /// frame stays readable meanwhile. The fill (a renderer readback) runs with the store
    /// unlocked: the renderer may take its FrameBuffer lock, which a render thread holds while it
    /// posts here. A failed fill presents nothing.
    fn present_with(&self, width: u32, height: u32, stride: u32, format: u32, fill: &mut dyn FnMut(&mut [u8]) -> bool) {
        if self.idle() {
            return self.present_in_place(width, height, stride, format, fill);
        }
        let mut spare = lock(&self.spare);
        spare.resize(stride as usize * height as usize, 0);
        if !fill(&mut spare) {
            return;
        }
        self.update(width, height, stride, format, false, |kept| {
            std::mem::swap(kept, &mut spare);
            true
        });
    }

    /// Records the size only; kept pixels from an earlier presented frame are freed, since a
    /// posted frame's pixels come from the renderer.
    fn posted(&self, width: u32, height: u32) {
        self.update(width, height, width * 4, PixelFormat::RGBA_VIRTIO, true, |kept| {
            if kept.capacity() > 0 {
                *kept = Vec::new();
            }
            true
        });
    }

    fn set_readback(&self, readback: Readback) {
        let _ = self.readback.set(readback);
    }

    fn set_surface_readback(&self, readback: SurfaceReadback) {
        let _ = self.surface_readback.set(readback);
    }

    /// An opaque black frame of the current size, presented as pixels (nothing before the first
    /// frame).
    fn blank(&self) {
        let (width, height, frames) = self.full_geometry();
        if frames == 0 || width == 0 || height == 0 {
            return;
        }
        self.update(width, height, width * 4, PixelFormat::RGBA_VIRTIO, false, |kept| {
            kept.clear();
            kept.extend(std::iter::repeat_n([0, 0, 0, 0xff], width as usize * height as usize).flatten());
            true
        });
    }
}

/// Post notices for a renderer that can't make them: a Vulkan-only renderer, whose post callback
/// would read every post back through the GL backend it doesn't have. Each renderer fence
/// [`PostTicker::poke`]s it; while pokes keep coming it records a posted frame
/// ([`DisplaySink::posted`]) at most once per `interval`, and goes on for `tail` after the last
/// one. Host composition and its post run on the renderer's post worker after the fence that
/// ends the composer's commands, so the tail is what catches the last frame of an animation.
/// A quiet guest costs nothing: the thread sleeps until the next fence. A notice with no new
/// frame behind it costs a reader one readback of an unchanged frame, and only while one reads.
///
/// With a [`PostProbe`] ([`PostTicker::start_probed`]) the notices follow the posts instead:
/// while pokes are recent the thread asks the probe every `poll` and gives a notice only when the
/// last post changed. A fence comes before its frame is posted, so a fence-paced notice reads
/// the old frame back and the new one waits out the interval; a probed one comes within `poll`
/// of the post, once per post. While the probe can't answer, notices are fence-paced as above.
/// Once a burst's first post is noticed it polls more coarsely, and when its tail runs out it
/// gives one more notice for a post the probe couldn't tell apart ([`Pacer`]).
pub struct PostTicker {
    state: Mutex<TickState>,
    wake: Condvar,
    /// Signalled when a notice given outside the lock ends ([`TickState::noticing`]).
    noticed: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// See [`PostTicker::hold_until`].
pub type Hold = Arc<dyn Fn() -> bool + Send + Sync>;

/// What tells a [`PostTicker`] a new post from none: an identity of the renderer's last post that
/// changes with each post (the posted color buffer's handle), or `None` while it can't be read.
/// Called on the ticker's thread, unlocked, every poll while fences come: it has to be cheap.
pub type PostProbe = Arc<dyn Fn() -> Option<u32> + Send + Sync>;

#[derive(Default)]
struct TickState {
    /// The last poke not yet followed by a full tail of notices.
    last: Option<Instant>,
    /// The thread is waiting for a poke (a poke then has to wake it).
    idle: bool,
    stop: bool,
    hold: Option<Hold>,
    /// The thread is giving a notice ([`DisplaySink::posted`], outside the lock).
    noticing: bool,
}

impl PostTicker {
    /// Starts the notice thread for a `width` x `height` display, its notices paced by fences.
    pub fn start(sink: Arc<dyn DisplaySink>, width: u32, height: u32, interval: Duration, tail: Duration) -> Arc<PostTicker> {
        Self::start_probed(sink, width, height, interval, tail, None)
    }

    /// [`PostTicker::start`], its notices following the posts `probe` tells apart, asked every
    /// `poll` while fences come (see [`PostTicker`]).
    pub fn start_probed(
        sink: Arc<dyn DisplaySink>,
        width: u32,
        height: u32,
        interval: Duration,
        tail: Duration,
        probe: Option<(PostProbe, Duration)>,
    ) -> Arc<PostTicker> {
        let ticker = Arc::new(PostTicker {
            state: Mutex::new(TickState::default()),
            wake: Condvar::new(),
            noticed: Condvar::new(),
            thread: Mutex::new(None),
        });
        let t = ticker.clone();
        let pace = Pace { interval, tail, probe };
        let spawned = std::thread::Builder::new().name("post-ticker".into()).spawn(move || t.run(&*sink, width, height, &pace));
        match spawned {
            Ok(h) => *ticker.thread.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(h),
            Err(e) => eprintln!("emu: post-ticker: {e}; host frames won't update"),
        }
        ticker
    }

    /// Holds notices back until `until` returns true (asked before each one, until it does).
    /// After a guest reset the renderer's last post is the old boot's, maybe freed: a notice then
    /// would have readers read it back, until the new boot posts. Returns once no notice is
    /// being given, so none reaches the sink after this returns until `until` is met (not from
    /// the sink's `posted`, which would wait for itself).
    pub fn hold_until(&self, until: Hold) {
        let mut s = self.lock();
        s.hold = Some(until);
        while s.noticing {
            s = self.noticed.wait(s).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Whether notices are held ([`PostTicker::hold_until`]), for the machine's tests.
    #[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
    pub(crate) fn holding(&self) -> bool {
        self.lock().hold.is_some()
    }

    fn lock(&self) -> MutexGuard<'_, TickState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The renderer finished some work (a fence signalled). Cheap: one uncontended lock, and a
    /// wakeup only when the thread was idle.
    pub fn poke(&self) {
        let mut s = self.lock();
        s.last = Some(Instant::now());
        if s.idle {
            s.idle = false;
            self.wake.notify_one();
        }
    }

    /// Ends the thread after its current notice and waits for it (not from the thread itself).
    pub fn stop(&self) {
        self.lock().stop = true;
        self.wake.notify_one();
        let thread = self.thread.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        if let Some(h) = thread {
            let _ = h.join();
        }
    }

    fn run(&self, sink: &dyn DisplaySink, width: u32, height: u32, pace: &Pace) {
        let mut pacer = Pacer::new(pace.interval, pace.tail, pace.probe.as_ref().map(|(_, poll)| *poll));
        loop {
            let (hold, last) = {
                let mut s = self.lock();
                loop {
                    if s.stop {
                        return;
                    }
                    if let Some(t) = s.last {
                        break (s.hold.clone(), t);
                    }
                    s.idle = true;
                    s = self.wake.wait(s).unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            // The hold and the probe are asked unlocked: they call into the renderer, whose
            // fences poke.
            let met = hold.as_ref().is_none_or(|h| h());
            let post = pace.probe.as_ref().and_then(|(p, _)| p());
            let now = Instant::now();
            let step = pacer.step(now, last, post, hold.is_some());
            let notice = {
                let mut s = self.lock();
                // Checked again under the lock: a hold set since it was read holds this notice
                // too, or the notice could land after the reset's black frame.
                let notice = step.notice
                    && met
                    && match (&s.hold, &hold) {
                        (None, _) => true,
                        (Some(cur), Some(h)) => Arc::ptr_eq(cur, h),
                        (Some(_), None) => false,
                    };
                // A met hold ends with the notice it lets through.
                if notice {
                    s.hold = None;
                }
                s.noticing = notice;
                notice
            };
            if notice {
                pacer.noticed(now, post);
                // Ends the notice even if the sink panics, so a hold never waits for it forever.
                struct Noticed<'a>(&'a PostTicker);
                impl Drop for Noticed<'_> {
                    fn drop(&mut self) {
                        self.0.lock().noticing = false;
                        self.0.noticed.notify_all();
                    }
                }
                let _end = Noticed(self);
                sink.posted(width, height);
            }
            match step.next {
                Some(d) => std::thread::sleep(d),
                None => {
                    // The burst is over, unless a fence came since it was read.
                    let mut s = self.lock();
                    if s.last == Some(last) {
                        s.last = None;
                        pacer.quiet();
                    }
                }
            }
        }
    }
}

/// How a [`PostTicker`] paces its notices (see [`PostTicker::start_probed`]).
struct Pace {
    interval: Duration,
    tail: Duration,
    probe: Option<(PostProbe, Duration)>,
}

/// How often a probed ticker asks for a new post once a burst's first post was noticed: the
/// rest of an animation's posts come a display frame apart, so a coarser poll still notices each
/// within it, for a quarter of the wakeups (see [`Pacer`]).
const SETTLED_POLL: Duration = Duration::from_millis(4);

/// A [`PostTicker`]'s decisions, apart from its thread, locks and clock, so tests can drive them
/// on a fake clock. A burst is the ticker's time awake: from the fence that wakes it to a tail
/// with none.
///
/// Probed, a burst polls every `poll` until its first new post is noticed (a tap's frame on a
/// quiet screen comes within `poll`), then every [`SETTLED_POLL`] for the rest of it: a steady
/// 60 Hz animation wakes the ticker about 250 times a second instead of 1000. The handle can't
/// tell every post: a compositor with a single target keeps it, three posts between two polls
/// cycle back to it, reallocated targets reuse it, and it may be set before its frame is
/// composed. So when the tail runs out, one more notice is given if a fence came since the last
/// one: at most one readback of an unchanged frame per burst, after the changed one.
struct Pacer {
    interval: Duration,
    tail: Duration,
    /// How often the probe is asked, with one.
    poll: Option<Duration>,
    /// The post the last notice was for (probed), so the same post isn't noticed twice.
    seen: Option<u32>,
    /// When the last notice was given.
    noticed: Option<Instant>,
    /// This burst's first new post was noticed: poll every [`SETTLED_POLL`].
    settled: bool,
}

/// What the ticker does on one wakeup: give a notice (unless held), then sleep for `next`, or go
/// idle when it's `None`.
#[derive(Debug, PartialEq)]
struct Step {
    notice: bool,
    next: Option<Duration>,
}

impl Pacer {
    fn new(interval: Duration, tail: Duration, poll: Option<Duration>) -> Pacer {
        Pacer { interval, tail, poll, seen: None, noticed: None, settled: false }
    }

    /// The step at `now`, the last fence having come at `last` and the probe reading `post`.
    /// `held`: a hold is set (a met one notices whatever is posted, see
    /// `machine::blank_until_next_post`: the next boot's first post may reuse the handle of the
    /// last boot's last).
    fn step(&self, now: Instant, last: Instant, post: Option<u32>, held: bool) -> Step {
        if now.saturating_duration_since(last) > self.tail {
            // The tail's notice: for a post the handle couldn't tell from the last.
            let notice = self.noticed.is_none_or(|n| last > n);
            return Step { notice, next: None };
        }
        let notice = post.is_none() || post != self.seen || held;
        let next = match (self.poll, post) {
            (Some(poll), Some(_)) if self.settled => poll.max(SETTLED_POLL),
            (Some(poll), Some(_)) => poll,
            _ => self.interval,
        };
        Step { notice, next: Some(next) }
    }

    /// A notice was given at `now` for `post`.
    fn noticed(&mut self, now: Instant, post: Option<u32>) {
        self.seen = post;
        self.noticed = Some(now);
        self.settled |= post.is_some();
    }

    /// The burst ended (the ticker went idle): the next one polls every `poll` again.
    fn quiet(&mut self) {
        self.settled = false;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts posted notices.
    #[derive(Default)]
    struct Posts(AtomicUsize);

    impl DisplaySink for Posts {
        fn present(&self, _: u32, _: u32, _: u32, _: u32, _: &[u8]) {}
        fn posted(&self, w: u32, h: u32) {
            assert_eq!((w, h), (2560, 1600));
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Polls `done` until it holds. The deadline is only a timeout (a hang fails instead of
    /// stalling the run): how long the wait takes says nothing, so a loaded host passes.
    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn post_ticker_notices_while_fences_come_and_for_a_tail_after() {
        let posts = Arc::new(Posts::default());
        // The tail is wall-clock from the last fence, so a ticker thread that isn't scheduled for
        // longer than the tail gives no tail notice at all, however long a test waits. A tail of
        // 300 ms makes that a stall of hundreds of milliseconds, not tens.
        let (interval, tail) = (Duration::from_millis(5), Duration::from_millis(300));
        let ticker = PostTicker::start(posts.clone(), 2560, 1600, interval, tail);
        std::thread::sleep(Duration::from_millis(30));
        // (Can only false-pass: a ticker that never ran also reads 0.)
        assert_eq!(posts.0.load(Ordering::Relaxed), 0, "no fence, no notice");

        // A steady stream of fences for 100 ms: notices keep coming, at most one per interval.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(100) {
            ticker.poke();
            std::thread::sleep(Duration::from_millis(1));
        }
        let during = posts.0.load(Ordering::Relaxed);
        // Sleeps run long on a loaded CI runner, so bound the count by the time that really
        // passed: more than one notice, never more than one per interval.
        let most = (start.elapsed().as_millis() / interval.as_millis()) as usize + 1;
        assert!((2..=most).contains(&during), "{during} notices in {:?} (at most {most})", start.elapsed());

        // The tail: notices go on for the tail after the last fence, then stop.
        wait_for("the frame posted after the last fence to be noticed", || posts.0.load(Ordering::Relaxed) > during);
        // Wait the tail out: the count has not moved for half a tail. This wait can only end
        // early if the thread stalls, and then the check below can only false-pass.
        let quiet = Duration::from_millis(150);
        let (mut after, mut since) = (posts.0.load(Ordering::Relaxed), Instant::now());
        while since.elapsed() < quiet {
            assert!(start.elapsed() < Duration::from_secs(10), "the tail never ended");
            std::thread::sleep(Duration::from_millis(5));
            let now = posts.0.load(Ordering::Relaxed);
            if now != after {
                (after, since) = (now, Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(posts.0.load(Ordering::Relaxed), after, "a quiet guest gets no notices");

        // One fence after a quiet spell wakes it again.
        ticker.poke();
        wait_for("a fence after a quiet spell to wake the ticker", || posts.0.load(Ordering::Relaxed) > after);
        ticker.stop();
    }

    /// A renderer's last post as the probe sees it: host composition cycling through its targets.
    struct FakeRenderer {
        posts: AtomicUsize,
        /// How many targets composition posts in turn (three on API 36, one for a compositor
        /// with a single target buffer).
        targets: usize,
        /// The probe can't read the renderer (a missing or uninitialized `FrameBuffer`).
        unreadable: std::sync::atomic::AtomicBool,
    }

    impl FakeRenderer {
        fn new(targets: usize) -> Arc<FakeRenderer> {
            Arc::new(FakeRenderer { posts: AtomicUsize::new(0), targets, unreadable: Default::default() })
        }

        fn post(&self) {
            self.posts.fetch_add(1, Ordering::SeqCst);
        }

        fn probe(self: &Arc<Self>) -> PostProbe {
            let r = self.clone();
            Arc::new(move || (!r.unreadable.load(Ordering::SeqCst)).then(|| (r.posts.load(Ordering::SeqCst) % r.targets) as u32 + 1))
        }
    }

    /// Counts notices, and the renderer's posts as each one saw them.
    struct Seen {
        renderer: Arc<FakeRenderer>,
        notices: AtomicUsize,
        /// The renderer's post count when the last notice was given.
        posts: AtomicUsize,
    }

    impl Seen {
        fn new(renderer: &Arc<FakeRenderer>) -> Arc<Seen> {
            Arc::new(Seen { renderer: renderer.clone(), notices: AtomicUsize::new(0), posts: AtomicUsize::new(usize::MAX) })
        }

        fn notices(&self) -> usize {
            self.notices.load(Ordering::SeqCst)
        }
    }

    impl DisplaySink for Seen {
        fn present(&self, _: u32, _: u32, _: u32, _: u32, _: &[u8]) {}
        fn posted(&self, _: u32, _: u32) {
            self.posts.store(self.renderer.posts.load(Ordering::SeqCst), Ordering::SeqCst);
            self.notices.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A probed ticker as the machine starts one (a 50 ms tail, polled every 1 ms), but with an
    /// interval of a minute: a fence-paced notice would take a minute.
    fn probed(sink: Arc<dyn DisplaySink>, renderer: &Arc<FakeRenderer>) -> Arc<PostTicker> {
        let probe = Some((renderer.probe(), Duration::from_millis(1)));
        PostTicker::start_probed(sink, 2560, 1600, Duration::from_secs(60), Duration::from_millis(50), probe)
    }

    /// Waits until the ticker has given no notice for three tails, so its burst is over.
    fn quiet(seen: &Seen) -> usize {
        let start = Instant::now();
        let (mut n, mut since) = (seen.notices(), Instant::now());
        while since.elapsed() < Duration::from_millis(150) {
            assert!(start.elapsed() < Duration::from_secs(10), "the notices never stopped");
            std::thread::sleep(Duration::from_millis(5));
            if seen.notices() != n {
                (n, since) = (seen.notices(), Instant::now());
            }
        }
        n
    }

    #[test]
    fn a_single_target_compositors_last_frame_is_noticed_when_its_fences_stop() {
        // Every post lands on the one target: the handle never changes after the first.
        let renderer = FakeRenderer::new(1);
        let seen = Seen::new(&renderer);
        let ticker = probed(seen.clone(), &renderer);
        ticker.poke();
        wait_for("the first post to be noticed", || seen.notices() >= 1);
        let first = quiet(&seen);
        for _ in 0..3 {
            fences(&ticker, 5);
            renderer.post();
            fences(&ticker, 5);
        }
        wait_for("the tail's notice of the last post", || seen.notices() > first);
        assert_eq!(seen.posts.load(Ordering::SeqCst), 3, "the last notice came after the last post");
        // (Can only false-pass: a stalled ticker also gives none.)
        let n = quiet(&seen);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(seen.notices(), n, "no fence, no notice");
        ticker.stop();
    }

    #[test]
    fn a_post_back_on_the_noticed_handle_is_noticed_when_its_fences_stop() {
        // Three posts between two polls (a late ticker on a loaded host): the 1-2-3 cycle lands
        // on the handle the last notice was for.
        let renderer = FakeRenderer::new(3);
        let seen = Seen::new(&renderer);
        let ticker = probed(seen.clone(), &renderer);
        ticker.poke();
        wait_for("the first post to be noticed", || seen.notices() >= 1);
        quiet(&seen);
        fences(&ticker, 5);
        for _ in 0..3 {
            renderer.post();
        }
        fences(&ticker, 5);
        wait_for("a notice after the last post", || seen.posts.load(Ordering::SeqCst) == 3);
        ticker.stop();
    }

    #[test]
    fn fences_with_no_post_behind_them_give_one_notice_when_they_stop() {
        // The handle may be set before its frame is composed: a notice of it can read the old
        // pixels, and the next post may keep the handle. The tail's notice covers both.
        let renderer = FakeRenderer::new(3);
        let seen = Seen::new(&renderer);
        let ticker = probed(seen.clone(), &renderer);
        ticker.poke();
        wait_for("the first post to be noticed", || seen.notices() >= 1);
        let first = quiet(&seen);
        fences(&ticker, 20);
        wait_for("the tail's notice", || seen.notices() > first);
        let n = quiet(&seen);
        // A ticker stalled for longer than the tail while the fences came ends the burst early
        // and starts another, so the count is bounded by bursts, not exact (the fake clock tests
        // below count exactly).
        assert!(n - first <= 2, "{} notices for one burst of fences", n - first);
        ticker.stop();
    }

    /// Drives a [`Pacer`] on a fake clock the way the ticker's thread does: idle until a fence,
    /// then awake on the pacer's sleeps until it goes idle. `fences` are times from the start in
    /// ms (sorted), `posts` the times the handle changes to a value (sorted), `start` the handle
    /// before the first. Returns the wakeups and the notices' times.
    fn simulate(pacer: &mut Pacer, fences: &[f64], posts: &[(f64, u32)], start: u32) -> (usize, Vec<f64>) {
        let t0 = Instant::now();
        let at = |ms: f64| t0 + Duration::from_secs_f64(ms / 1000.0);
        let since = |t: Instant| t.duration_since(t0).as_secs_f64() * 1000.0;
        let (mut now, mut awake, mut wakeups, mut notices) = (t0, false, 0, Vec::new());
        loop {
            if !awake {
                match fences.iter().find(|&&f| at(f) > now || (wakeups == 0 && at(f) >= now)) {
                    Some(&f) => (now, awake) = (at(f), true),
                    None => return (wakeups, notices),
                }
            }
            let last = fences.iter().rev().map(|&f| at(f)).find(|&f| f <= now).expect("woken by a fence");
            let post = posts.iter().rev().find(|&&(t, _)| at(t) <= now).map_or(start, |&(_, h)| h);
            wakeups += 1;
            let step = pacer.step(now, last, Some(post), false);
            if step.notice {
                pacer.noticed(now, Some(post));
                notices.push(since(now));
            }
            match step.next {
                Some(d) => now += d,
                None => {
                    pacer.quiet();
                    awake = false;
                }
            }
        }
    }

    /// The machine's pacer (16 ms interval, 50 ms tail, 1 ms poll).
    fn machine_pacer() -> Pacer {
        Pacer::new(Duration::from_millis(16), Duration::from_millis(50), Some(Duration::from_millis(1)))
    }

    #[test]
    fn a_tap_on_a_quiet_screen_is_noticed_within_a_poll_of_its_post() {
        let mut pacer = machine_pacer();
        // The current frame, noticed and gone quiet; then a tap: the app's and the composer's
        // fences for 6 ms, its post at 5.5 ms, and the post's own fence at 7 ms.
        let mut fences = vec![0.0];
        fences.extend((0..=6).map(|i| 200.0 + i as f64));
        fences.push(207.0);
        let (_, notices) = simulate(&mut pacer, &fences, &[(205.5, 2)], 1);
        let tap: Vec<f64> = notices.iter().copied().filter(|&t| t >= 200.0).collect();
        assert!(tap[0] >= 205.5 && tap[0] <= 206.5, "the tap's frame noticed at {:.1} ms, posted at 205.5", tap[0]);
        // Then at most the tail's notice, after it.
        assert!(tap.len() <= 2, "notices of the tap: {tap:?}");
    }

    /// 60 Hz for `secs`: each frame's fences at 0, 1 and 2 ms into it, its post at 4 ms, on
    /// `targets` targets in turn. Returns the fences and the posts.
    fn animation(secs: f64, targets: u32) -> (Vec<f64>, Vec<(f64, u32)>) {
        let frame = 1000.0 / 60.0;
        let frames = (secs * 60.0) as u32;
        let fences = (0..frames).flat_map(|k| [0.0, 1.0, 2.0].map(|d| k as f64 * frame + d)).collect();
        let posts = (0..frames).map(|k| (k as f64 * frame + 4.0, (k + 1) % targets + 1)).collect();
        (fences, posts)
    }

    #[test]
    fn a_steady_animation_is_polled_coarsely_after_its_first_post() {
        let mut pacer = machine_pacer();
        let (fences, posts) = animation(1.0, 3);
        let (wakeups, notices) = simulate(&mut pacer, &fences, &posts, 1);
        // A 1 ms poll for the whole second would wake 1000 times (plus the tail).
        assert!(wakeups <= 300, "{wakeups} wakeups for a second of 60 Hz");
        // The frame on screen when it starts, every post once, and the tail's notice after the
        // last.
        assert!((61..=62).contains(&notices.len()), "{} notices of 60 posts", notices.len());
        // The coarse poll still follows each post within a poll of its own.
        let settled = SETTLED_POLL.as_secs_f64() * 1000.0 + 1e-6;
        for (i, &(t, _)) in posts.iter().enumerate() {
            let n = notices[i + 1];
            assert!(n >= t && n - t <= settled, "post {i} at {t:.1} noticed at {n:.1}");
        }
    }

    #[test]
    fn the_next_burst_polls_finely_again() {
        let mut pacer = machine_pacer();
        // A second of animation, a quiet spell, then a tap: its frame within 1 ms of its post.
        let (mut fences, posts) = animation(1.0, 3);
        let mut posts = posts;
        fences.extend((0..=6).map(|i| 2000.0 + i as f64));
        let next = posts.last().map_or(1, |&(_, h)| h % 3 + 1);
        posts.push((2005.5, next));
        let (_, notices) = simulate(&mut pacer, &fences, &posts, 1);
        let tap = notices.iter().copied().find(|&t| t >= 2000.0).expect("the tap noticed");
        assert!(tap - 2005.5 <= 1.0, "the tap's frame noticed at {tap:.1} ms, posted at 2005.5");
    }

    #[test]
    fn the_tail_notices_once_what_the_handle_cannot_tell() {
        // A single target: the posts after the first never change the handle.
        let mut pacer = machine_pacer();
        let (fences, posts) = animation(0.2, 1);
        let (_, notices) = simulate(&mut pacer, &fences, &posts, 1);
        let last = *fences.last().expect("fences");
        assert_eq!(notices.len(), 2, "the first post and the tail's notice: {notices:?}");
        let end = last + 50.0 + SETTLED_POLL.as_secs_f64() * 1000.0;
        assert!(notices[1] > last + 50.0 && notices[1] <= end, "the tail's notice at {:.1}, last fence {last:.1}", notices[1]);

        // Three posts between two polls: the cycle lands on the noticed handle.
        let mut pacer = machine_pacer();
        let fences: Vec<f64> = (0..10).map(f64::from).collect();
        let (_, notices) = simulate(&mut pacer, &fences, &[(5.1, 2), (5.2, 3), (5.3, 1)], 1);
        assert_eq!(notices.len(), 2, "the first frame and the tail's notice: {notices:?}");

        // Fences after the last notice, the handle never changing: one notice at the tail's end.
        let mut pacer = machine_pacer();
        let fences: Vec<f64> = (0..20).map(f64::from).collect();
        let (_, notices) = simulate(&mut pacer, &fences, &[], 1);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[1] > 69.0, "the tail's notice at {:.1}", notices[1]);

        // The pin: no fence after the last notice, no tail notice.
        let mut pacer = machine_pacer();
        let (_, notices) = simulate(&mut pacer, &[0.0], &[], 1);
        assert_eq!(notices.len(), 1, "{notices:?}");
    }

    /// Pokes for `ms` (fences with no post behind them).
    fn fences(ticker: &PostTicker, ms: u64) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            ticker.poke();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_probed_ticker_notices_each_post_once_and_soon() {
        let (posts, renderer) = (Arc::new(Posts::default()), FakeRenderer::new(3));
        // An interval and a tail of a minute: a notice paced by fences after the first would take
        // a minute, so any later notice within the waits' 5 s timeout came from the probe.
        let minute = Duration::from_secs(60);
        let probe = Some((renderer.probe(), Duration::from_millis(1)));
        let ticker = PostTicker::start_probed(posts.clone(), 2560, 1600, minute, minute, probe);
        ticker.poke();
        wait_for("the current post to be noticed", || posts.0.load(Ordering::Relaxed) == 1);
        for n in 2..=6 {
            // Fences before the post (the composer's commands): no notice of an unchanged frame.
            fences(&ticker, 5);
            assert_eq!(posts.0.load(Ordering::Relaxed), n - 1, "a fence with no post behind it");
            renderer.post();
            wait_for("a post to be noticed within the poll, not the interval", || posts.0.load(Ordering::Relaxed) >= n);
        }
        // (Can only false-pass: a stalled ticker also reads 6.)
        fences(&ticker, 20);
        assert_eq!(posts.0.load(Ordering::Relaxed), 6, "one notice per post");
        ticker.stop();
    }

    #[test]
    fn a_probed_ticker_sleeps_through_a_quiet_guest_and_follows_fences_while_it_cannot_probe() {
        let (posts, renderer) = (Arc::new(Posts::default()), FakeRenderer::new(3));
        let probe = Some((renderer.probe(), Duration::from_millis(1)));
        let ticker = PostTicker::start_probed(posts.clone(), 2560, 1600, Duration::from_millis(5), Duration::from_millis(20), probe);
        // No fence: a post isn't looked for (the probe is asked only while fences come).
        renderer.post();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(posts.0.load(Ordering::Relaxed), 0, "no fence, no notice");
        // The renderer can't be probed: notices come with the fences, as without a probe.
        renderer.unreadable.store(true, Ordering::SeqCst);
        fences(&ticker, 60);
        wait_for("fence-paced notices", || posts.0.load(Ordering::Relaxed) >= 2);
        ticker.stop();
    }

    #[test]
    fn a_met_hold_notices_the_next_boots_post_even_on_the_last_boots_handle() {
        let (posts, renderer) = (Arc::new(Posts::default()), FakeRenderer::new(3));
        let minute = Duration::from_secs(60);
        let probe = Some((renderer.probe(), Duration::from_millis(1)));
        let ticker = PostTicker::start_probed(posts.clone(), 2560, 1600, minute, minute, probe);
        ticker.poke();
        wait_for("the last boot's post to be noticed", || posts.0.load(Ordering::Relaxed) == 1);
        // A reset, and the next boot's first post on the same handle: the probe reads no change.
        let posted_new = Arc::new(std::sync::atomic::AtomicBool::new(false));
        ticker.hold_until({
            let posted_new = posted_new.clone();
            Arc::new(move || posted_new.load(Ordering::SeqCst))
        });
        fences(&ticker, 10);
        posted_new.store(true, Ordering::SeqCst);
        wait_for("the next boot's post to be noticed", || posts.0.load(Ordering::Relaxed) == 2);
        fences(&ticker, 20);
        assert_eq!(posts.0.load(Ordering::Relaxed), 2, "noticed once");
        ticker.stop();
    }

    #[test]
    fn a_held_ticker_gives_no_notice_until_its_hold_is_met() {
        let posts = Arc::new(Posts::default());
        let ticker = PostTicker::start(posts.clone(), 2560, 1600, Duration::from_millis(2), Duration::from_millis(20));
        // After a reset: fences of the new boot, but no new post yet.
        let posted_new = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let asked = Arc::new(AtomicUsize::new(0));
        ticker.hold_until({
            let (posted_new, asked) = (posted_new.clone(), asked.clone());
            Arc::new(move || {
                asked.fetch_add(1, Ordering::Relaxed);
                posted_new.load(Ordering::Relaxed)
            })
        });
        let fences = |ms| {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(ms) {
                ticker.poke();
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        fences(40);
        assert_eq!(posts.0.load(Ordering::Relaxed), 0, "held: no notice");
        assert!(asked.load(Ordering::Relaxed) > 0, "the hold was asked");
        // The new boot posted: notices resume, and the hold is dropped (asked no more).
        posted_new.store(true, Ordering::Relaxed);
        fences(40);
        assert!(posts.0.load(Ordering::Relaxed) > 0, "released");
        let asked_then = asked.load(Ordering::Relaxed);
        posted_new.store(false, Ordering::Relaxed);
        fences(20);
        assert_eq!(asked.load(Ordering::Relaxed), asked_then, "a met hold is gone");
        ticker.stop();
    }

    /// A sink whose notices wait at a gate until it opens.
    #[derive(Default)]
    struct Gated {
        posts: AtomicUsize,
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl DisplaySink for Gated {
        fn present(&self, _: u32, _: u32, _: u32, _: u32, _: &[u8]) {}
        fn posted(&self, _: u32, _: u32) {
            self.posts.fetch_add(1, Ordering::SeqCst);
            let mut open = self.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*open {
                open = self.opened.wait(open).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    #[test]
    fn a_hold_waits_out_a_notice_already_under_way() {
        let sink = Arc::new(Gated::default());
        let ticker = PostTicker::start(sink.clone(), 2560, 1600, Duration::from_millis(1), Duration::from_secs(60));
        ticker.poke();
        let start = Instant::now();
        while sink.posts.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "no notice");
            std::thread::sleep(Duration::from_millis(1));
        }
        // A reset holds notices while one is being given (it read no hold).
        let returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holder = std::thread::spawn({
            let (ticker, returned) = (ticker.clone(), returned.clone());
            move || {
                ticker.hold_until(Arc::new(|| false));
                returned.store(true, Ordering::SeqCst);
            }
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!returned.load(Ordering::SeqCst), "the hold returned while a notice was under way");
        *sink.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        sink.opened.notify_all();
        holder.join().expect("holder");
        let n = sink.posts.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(sink.posts.load(Ordering::SeqCst), n, "a notice after the hold returned");
        ticker.stop();
    }

    #[test]
    fn stopping_a_ticker_ends_its_thread() {
        let posts = Arc::new(Posts::default());
        let ticker = PostTicker::start(posts.clone(), 2560, 1600, Duration::from_millis(1), Duration::from_secs(60));
        ticker.poke(); // busy: noticing every millisecond for a minute
        wait_for("the first notice", || posts.0.load(Ordering::Relaxed) > 0);
        let t = Instant::now();
        ticker.stop();
        // A timeout, not a measurement: the tail alone is a minute, so a thread that ignored
        // the stop would take far longer than this.
        assert!(t.elapsed() < Duration::from_secs(5), "stop took {:?}", t.elapsed());
        // stop() joined the thread: nothing can post after it returned, so read the count now.
        let n = posts.0.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(10)); // can only false-pass
        assert_eq!(posts.0.load(Ordering::Relaxed), n, "no notice after stop returned");
        ticker.stop(); // twice is fine
    }

    #[test]
    fn blanking_replaces_the_last_boots_frame_with_black() {
        let store = FrameStore::default();
        store.blank();
        assert_eq!(store.frames(), 0, "nothing shown yet: nothing to blank");
        // The last boot's final frame was posted (its pixels the renderer's, maybe freed by now).
        store.set_readback(Box::new(|_, _, buf| {
            buf.fill(0x7f);
            true
        }));
        store.posted(4, 2);
        assert_eq!(store.latest(|c| c.rgba[0]), Some(0x7f));
        store.blank();
        let s = store.snapshot();
        assert!(!s.posted && (s.width, s.height, s.frames) == (4, 2, 2));
        let rgba = store.latest(|c| c.rgba.clone()).expect("a frame");
        assert_eq!(rgba, [0, 0, 0, 0xff].repeat(8), "opaque black, not the readback");
    }

    #[test]
    fn a_view_shows_readers_part_of_the_scanout() {
        let store = FrameStore::default();
        // 4x2, each pixel's red byte its index.
        let pixels: Vec<u8> = (0..8u8).flat_map(|i| [i, 0, 0, 0xff]).collect();
        store.present(4, 2, 16, PixelFormat::RGBA_VIRTIO, &pixels);
        let n = store.frames();
        store.set_view(Some((1, 0, 2, 2)));
        assert_eq!(store.geometry(), (2, 2, n + 1), "counted again at the new size");
        assert_eq!(store.view_origin(), (1, 0));
        let reds = |store: &FrameStore| store.latest(|c| (c.width, c.height, c.rgba.chunks(4).map(|p| p[0]).collect::<Vec<_>>()));
        assert_eq!(reds(&store), Some((2, 2, vec![1, 2, 5, 6])));
        let mut dst = vec![0; 2 * 2 * 4];
        assert!(store.read_into(2, 2, n + 1, &mut dst));
        assert_eq!(dst.chunks(4).map(|p| p[0]).collect::<Vec<_>>(), [1, 2, 5, 6]);
        // Posted frames are cut from the readback too.
        store.set_readback(Box::new(|w, _, buf| {
            buf.chunks_mut(4).enumerate().for_each(|(i, p)| p.copy_from_slice(&[(i as u32 % w) as u8 + 10, 0, 0, 0xff]));
            true
        }));
        store.posted(4, 2);
        let (_, _, m) = store.geometry();
        assert!(store.read_into(2, 2, m, &mut dst));
        assert_eq!(dst.chunks(4).map(|p| p[0]).collect::<Vec<_>>(), [11, 12, 11, 12]);
        // A view as large as the frame, or none, shows all of it.
        store.set_view(Some((0, 0, 9, 9)));
        assert_eq!((store.geometry().0, store.geometry().1, store.view_origin()), (4, 2, (0, 0)));
        store.set_view(None);
        assert_eq!(reds(&store).map(|(w, h, _)| (w, h)), Some((4, 2)));
    }

    /// A `w` x `h` frame whose pixels hold their own index (little-endian, all four bytes).
    fn indexed(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| i.to_le_bytes()).collect()
    }

    fn index_at(c: &Capture, (x, y): (u32, u32)) -> u32 {
        let i = (y * c.width + x) as usize * 4;
        u32::from_le_bytes(c.rgba[i..i + 4].try_into().expect("pixel"))
    }

    #[test]
    fn an_unturned_device_reads_its_frames_unchanged_and_keeps_no_turned_copy() {
        let store = FrameStore::default();
        store.present(6, 4, 24, PixelFormat::RGBA_VIRTIO, &indexed(6, 4));
        assert_eq!(store.rotation(), Rotation::Deg0);
        let plain = store.latest(|c| c.rgba.clone()).expect("frame");
        let bytes = store.buffered_bytes();
        // The capture itself, not a copy: the same buffer.
        let same = store.latest(|c| c.rgba.as_ptr()) == store.latest_upright(|c| c.rgba.as_ptr());
        assert!(same, "upright is the capture when unturned");
        assert_eq!(store.latest_upright(|c| c.rgba.clone()), Some(plain));
        assert_eq!(store.buffered_bytes(), bytes, "no turned buffer");
        assert_eq!(store.upright_geometry(), store.geometry());
        assert_eq!(store.to_scanout(5.0, 3.0), (5, 3));
        // Setting the same turn again isn't a new frame.
        let n = store.frames();
        store.set_rotation(Rotation::Deg0);
        assert_eq!(store.frames(), n);
    }

    #[test]
    fn a_turned_device_shows_readers_its_frames_upright_turned_once_per_frame() {
        // A phone (portrait panel) and a tablet (landscape panel), scaled down.
        for (w, h) in [(6u32, 10u32), (10, 6)] {
            let store = FrameStore::default();
            store.present(w, h, w * 4, PixelFormat::RGBA_VIRTIO, &indexed(w, h));
            for r in [Rotation::Deg90, Rotation::Deg180, Rotation::Deg270, Rotation::Deg0] {
                let n = store.frames();
                store.set_rotation(r);
                assert_eq!(store.frames(), n + 1, "{r:?}: counted again, so streams send it turned");
                let (tw, th) = r.turned((w, h));
                assert_eq!(store.upright_geometry(), (tw, th, n + 1));
                assert_eq!(store.geometry(), (w, h, n + 1), "readers that turn frames themselves see the panel");
                // Every panel pixel shows where the turn puts it.
                store
                    .latest_upright(|c| {
                        assert_eq!((c.width, c.height, c.frame), (tw, th, n + 1));
                        for y in 0..h {
                            for x in 0..w {
                                assert_eq!(index_at(c, r.panel_to_screen((x, y), (w, h))), y * w + x, "{r:?} ({x}, {y})");
                            }
                        }
                    })
                    .expect("frame");
                // The panel's frame is untouched, and a second reader shares the turned copy.
                assert_eq!(store.latest(|c| index_at(c, (1, 0))), Some(1));
                if r != Rotation::Deg0 {
                    let first = store.latest_upright(|c| c.rgba.as_ptr());
                    assert_eq!(store.latest_upright(|c| c.rgba.as_ptr()), first, "one turned copy per frame");
                }
            }
        }
    }

    #[test]
    fn touches_on_the_upright_screen_land_on_the_scanout_pixel_shown_there() {
        // Phone 1080x2400, a landscape tablet 2560x1600, and a folded Pixel Fold (its outer
        // screen, 949x1840 at x 629 of the 2208x1840 scanout).
        let cases: [((u32, u32), Option<View>); 3] =
            [((1080, 2400), None), ((2560, 1600), None), ((2208, 1840), Some((629, 0, 949, 1840)))];
        for ((sw, sh), view) in cases {
            let store = FrameStore::default();
            store.posted(sw, sh);
            store.set_view(view);
            let (ox, oy, vw, vh) = view.unwrap_or((0, 0, sw, sh));
            for r in Rotation::ALL {
                store.set_rotation(r);
                let (tw, th, _) = store.upright_geometry();
                assert_eq!((tw, th), r.turned((vw, vh)));
                // Each corner of the screen, and a point inside, on the view's pixel there.
                for (vx, vy) in [(0, 0), (vw - 1, 0), (0, vh - 1), (vw - 1, vh - 1), (vw / 3, vh / 5)] {
                    let (x, y) = r.panel_to_screen((vx, vy), (vw, vh));
                    let want = ((vx + ox) as i32, (vy + oy) as i32);
                    assert_eq!(store.to_scanout(f64::from(x) + 0.5, f64::from(y) + 0.5), want, "{r:?} {sw}x{sh} {view:?}");
                }
            }
        }
    }

    #[test]
    fn turned_buffers_go_with_the_rest_when_unread() {
        let store = FrameStore::with_idle(Duration::from_millis(30));
        store.present(8, 8, 32, PixelFormat::RGBA_VIRTIO, &indexed(8, 8));
        store.set_rotation(Rotation::Deg90);
        store.latest_upright(|_| ()).expect("frame");
        assert!(lock(&store.upright).0.rgba.capacity() > 0);
        std::thread::sleep(Duration::from_millis(40));
        store.present(8, 8, 32, PixelFormat::RGBA_VIRTIO, &indexed(8, 8));
        assert_eq!(lock(&store.upright).0.rgba.capacity(), 0, "freed with the capture");
        assert_eq!(store.latest_upright(|c| index_at(c, (7, 0))), Some(0), "turned again when read");
    }

    #[test]
    fn wake_ends_a_wait_without_a_frame() {
        let store = Arc::new(FrameStore::default());
        let waker = store.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let woke = std::time::Instant::now();
            waker.wake();
            woke
        });
        assert_eq!(store.wait_newer(0, Duration::from_secs(10)), 0, "no frame, still the latest number");
        let returned = std::time::Instant::now();
        let woke = t.join().expect("waker");
        assert!(returned.duration_since(woke) < Duration::from_millis(100), "returned {:?} after wake", returned.duration_since(woke));
        // A wake ends only waits in progress, not the next one.
        let start = std::time::Instant::now();
        assert_eq!(store.wait_newer(0, Duration::from_millis(30)), 0);
        assert!(start.elapsed() >= Duration::from_millis(30), "a later wait still times out");
    }

    const IDLE_TEST: Duration = Duration::from_millis(60);

    #[test]
    fn unread_buffers_are_freed_after_the_idle_period() {
        // Guest composition: a reader makes the store keep pixels, capture and spare.
        let store = FrameStore::with_idle(IDLE_TEST);
        let frame = |store: &FrameStore, v: u8| {
            store.present_with(4, 4, 16, 2, &mut |b| {
                b.fill(v);
                true
            })
        };
        store.mark_read();
        frame(&store, 1);
        assert_eq!(store.latest(|c| c.rgba.len()), Some(64));
        frame(&store, 2);
        assert_eq!(store.buffered_bytes(), 3 * 64, "pixels, capture, spare");
        std::thread::sleep(IDLE_TEST + Duration::from_millis(20));
        // The next post frees the capture and the spare, and reads the frame in place.
        frame(&store, 3);
        assert_eq!(store.buffered_bytes(), 64, "only the kept frame");
        assert_eq!(store.latest(|c| c.rgba[0]), Some(3), "still readable");
        // Host composition: an unread store keeps nothing at all.
        let posted = FrameStore::with_idle(IDLE_TEST);
        posted.set_readback(Box::new(|_, _, b| {
            b.fill(7);
            true
        }));
        posted.present(4, 4, 16, 2, &[1; 64]);
        posted.posted(4, 4);
        assert_eq!(posted.latest(|c| c.rgba[0]), Some(7));
        assert_eq!(posted.buffered_bytes(), 64, "the capture; posted frames keep no pixels");
        std::thread::sleep(IDLE_TEST + Duration::from_millis(20));
        posted.posted(4, 4);
        assert_eq!(posted.buffered_bytes(), 0, "freed on the next post");
        assert_eq!(posted.latest(|c| c.rgba[0]), Some(7), "read back again on demand");
    }

    #[test]
    fn a_waiting_stream_keeps_the_buffers_on_a_static_screen() {
        let store = FrameStore::with_idle(IDLE_TEST);
        store.set_readback(Box::new(|_, _, b| {
            b.fill(7);
            true
        }));
        store.posted(4, 4);
        assert_eq!(store.latest(|c| c.rgba[0]), Some(7));
        // A stream waits out more than the idle period with nothing new on screen.
        assert_eq!(store.wait_newer(1, IDLE_TEST + Duration::from_millis(20)), 1);
        store.posted(4, 4);
        assert_eq!(store.buffered_bytes(), 64, "still a reader: the capture stays");
    }

    #[test]
    fn steady_reading_reuses_every_buffer() {
        // 50 guest-composed frames, each read: the buffers trade places but none is allocated.
        let store = FrameStore::with_idle(IDLE_TEST);
        store.mark_read();
        let mut seen = std::collections::HashSet::new();
        for i in 0..50u8 {
            store.present_with(8, 8, 32, 2, &mut |b| {
                seen.insert(b.as_ptr() as usize);
                b.fill(i);
                true
            });
            let (v, capture) = store.latest(|c| (c.rgba[0], c.rgba.as_ptr() as usize)).expect("frame");
            assert_eq!(v, i);
            seen.insert(capture);
        }
        assert!(seen.len() <= 3, "{} distinct buffers for 50 frames", seen.len());
        assert_eq!(store.buffered_bytes(), 3 * 256);
    }

    #[test]
    fn a_reader_waits_for_an_unread_frame_filled_in_place() {
        let store = Arc::new(FrameStore::with_idle(IDLE_TEST));
        store.present_with(1, 1, 4, 67, &mut |b| {
            b.copy_from_slice(&[1, 2, 3, 4]);
            true
        });
        std::thread::sleep(IDLE_TEST + Duration::from_millis(20));
        let reader = store.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        store.present_with(1, 1, 4, 67, &mut |b| {
            let reader = reader.clone();
            let tx = tx.clone();
            std::thread::spawn(move || tx.send(reader.latest(|c| c.rgba.clone())).expect("send"));
            std::thread::sleep(Duration::from_millis(30));
            b.copy_from_slice(&[5, 6, 7, 8]);
            true
        });
        assert_eq!(rx.recv().expect("reader"), Some(vec![5, 6, 7, 8]), "the new frame, after the fill");
        // A failed in-place fill shows nothing rather than a torn frame.
        std::thread::sleep(IDLE_TEST + Duration::from_millis(20));
        store.present_with(1, 1, 4, 67, &mut |b| {
            b[0] = 9;
            false
        });
        assert_eq!(store.frames(), 2);
        assert_eq!(store.latest(|_| ()), None);
    }

    #[test]
    fn waiters_wake_on_the_next_frame() {
        let store = std::sync::Arc::new(FrameStore::default());
        assert_eq!(store.wait_newer(0, Duration::from_millis(10)), 0, "times out with nothing new");
        let poster = store.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            poster.present(1, 1, 4, 1, &[0; 4]);
        });
        let start = std::time::Instant::now();
        assert_eq!(store.wait_newer(0, Duration::from_secs(5)), 1);
        assert!(start.elapsed() < Duration::from_secs(2), "woken by the frame, not the timeout");
        t.join().expect("poster");
        assert_eq!(store.wait_newer(0, Duration::ZERO), 1, "already newer: no wait");
    }

    /// A frame posted bottom-up, as host composition posts them (BGRA), `n` times at each size:
    /// flipped and copied (the first version), copied row by row (the last), and posted (size
    /// only, now). One line per size.
    fn present_bench(sizes: &[(u32, u32)], n: u32) -> Vec<String> {
        let mut lines = Vec::new();
        for &(w, h) in sizes {
            let px: Vec<u8> = (0..w * h * 4).map(|i| i as u8).collect();
            let store = FrameStore::default();
            let time = |f: &mut dyn FnMut()| {
                let t = std::time::Instant::now();
                for _ in 0..n {
                    f();
                }
                t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            };
            let row = w as usize * 4;
            let old = time(&mut || {
                let flipped: Vec<u8> = px.chunks(row).rev().flatten().copied().collect();
                store.present(w, h, w * 4, 1, &flipped);
            });
            let copied = time(&mut || store.present_bottom_up(w, h, 1, &px));
            let posted = time(&mut || store.posted(w, h));
            assert_eq!(store.frames(), 3 * u64::from(n));
            lines.push(format!(
                "{w}x{h} frame into the store: {old:.2} ms (flip, then copy) -> {copied:.2} ms (copy) -> {posted:.4} ms (posted: size only)"
            ));
        }
        lines
    }

    #[test]
    fn the_benchmark_runs() {
        assert_eq!(present_bench(&[(4, 3)], 2).len(), 1);
    }

    /// Posts at a tablet's and a phone's size.
    ///     cargo test --release -p simee-emu-vmm present_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn present_cost() {
        for l in present_bench(&[(2480, 1740), (1080, 2400)], 30) {
            println!("{l}");
        }
    }

    #[test]
    fn keeps_the_latest_frame_and_counts_them() {
        let store = FrameStore::default();
        assert_eq!((store.frames(), store.snapshot().pixels.len()), (0, 0));
        store.present(2, 1, 8, 1, &[1; 8]);
        store.present(1, 1, 4, 2, &[7, 7, 7, 7]);
        let f = store.snapshot();
        assert_eq!((f.width, f.height, f.stride, f.format, f.frames), (1, 1, 4, 2, 2));
        assert_eq!(f.pixels, [7, 7, 7, 7], "replaced, not appended");
        assert_eq!(store.geometry(), (1, 1, 2));
        // Bottom-up rows come out top-down, whether the store or the default flips them.
        let rows = [1u8, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
        store.present_bottom_up(1, 3, 1, &rows);
        assert_eq!(store.snapshot().pixels, [3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1]);
        #[derive(Default)]
        struct Plain(Mutex<Vec<u8>>);
        impl DisplaySink for Plain {
            fn present(&self, _: u32, _: u32, _: u32, _: u32, pixels: &[u8]) {
                *self.0.lock().expect("plain") = pixels.to_vec();
            }
        }
        let plain = Plain::default();
        plain.present_bottom_up(1, 3, 1, &rows);
        assert_eq!(*plain.0.lock().expect("plain"), store.snapshot().pixels);
        assert_eq!(store.frames(), 3);
        // The trait's defaults: present_with fills a temporary; posted and set_readback are no-ops.
        plain.present_with(1, 1, 4, 1, &mut |b| {
            b.copy_from_slice(&[4, 5, 6, 7]);
            true
        });
        plain.present_with(1, 1, 4, 1, &mut |_| false);
        assert_eq!(*plain.0.lock().expect("plain"), [4, 5, 6, 7], "a failed fill presents nothing");
        plain.posted(1, 1);
        plain.set_readback(Box::new(|_, _, _| true));
    }

    #[test]
    fn presents_through_a_spare_buffer_and_keeps_the_last_frame_on_failure() {
        let store = FrameStore::default();
        store.present_with(1, 2, 8, 2, &mut |b| {
            assert_eq!(b.len(), 16, "stride x height");
            b.copy_from_slice(&[1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]);
            true
        });
        let f = store.snapshot();
        assert_eq!((f.width, f.height, f.stride, f.format, f.frames), (1, 2, 8, 2, 1));
        assert_eq!(f.pixels, [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]);
        // Captures convert presented pixels (BGRX here) to tightly packed RGBA.
        assert_eq!(store.latest(|c| (c.width, c.height, c.frame, c.rgba.clone())), Some((1, 2, 1, vec![3, 2, 1, 255, 7, 6, 5, 255])));
        store.present_with(1, 2, 8, 2, &mut |b| {
            b.fill(9);
            false
        });
        let f = store.snapshot();
        assert_eq!((f.frames, f.pixels), (1, vec![1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]), "the last frame, untouched");
        assert_eq!(store.latest(|c| c.rgba[0]), Some(3));
        // The fill runs with the store unlocked: a renderer readback may wait on a render thread
        // that is posting here (this would deadlock on the store's own lock otherwise).
        store.present_with(1, 1, 4, 67, &mut |b| {
            store.posted(1, 1);
            b.copy_from_slice(&[5, 6, 7, 8]);
            store.geometry().2 == 2
        });
        let f = store.snapshot();
        assert_eq!((f.frames, f.pixels, f.posted), (3, vec![5, 6, 7, 8], false));
        // The frame being filled doesn't hide the last one from readers.
        store.present_with(1, 1, 4, 67, &mut |b| {
            assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![5, 6, 7, 8]));
            b.fill(1);
            true
        });
        assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![1; 4]));
    }

    #[test]
    fn presented_frames_never_come_from_the_renderer() {
        // After a guest reboot, or with guest composition, the renderer still holds an old post.
        let store = FrameStore::default();
        store.set_readback(Box::new(|_, _, buf| {
            buf.fill(0xEE);
            true
        }));
        store.present(1, 1, 4, 67, &[1, 2, 3, 4]);
        assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![1, 2, 3, 4]));
        let mut dst = [0; 4];
        store.present(1, 1, 4, 67, &[5, 6, 7, 8]);
        assert!(store.read_into(1, 1, 2, &mut dst));
        assert_eq!(dst, [5, 6, 7, 8]);
        store.posted(1, 1);
        assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![0xEE; 4]), "posted: the renderer's");
    }

    #[test]
    fn posted_frames_are_read_back_once_however_many_readers() {
        let store = FrameStore::default();
        assert_eq!(store.latest(|_| ()), None, "nothing posted yet");
        let reads = Arc::new(AtomicUsize::new(0));
        let counter = reads.clone();
        store.set_readback(Box::new(move |w, h, buf| {
            let n = counter.fetch_add(1, Ordering::Relaxed) as u8;
            assert_eq!(buf.len(), w as usize * h as usize * 4);
            buf.fill(n + 1);
            true
        }));
        store.posted(2, 1);
        assert_eq!(store.snapshot().pixels.len(), 0, "a post copies no pixels");
        assert_eq!(store.geometry(), (2, 1, 1));
        let first = store.latest(|c| (c.frame, c.rgba.clone()));
        let again = store.latest(|c| (c.frame, c.rgba.clone()));
        assert_eq!(first, Some((1, vec![1; 8])));
        assert_eq!(again, first, "the same frame: no second readback");
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        store.posted(2, 1);
        assert_eq!(store.latest(|c| c.rgba[0]), Some(2), "a new frame is read back");
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        // A whole-frame consumer copies a captured frame, and reads a new one straight in.
        let mut dst = [0u8; 8];
        assert!(store.read_into(2, 1, 2, &mut dst));
        assert_eq!((dst, reads.load(Ordering::Relaxed)), ([2; 8], 2), "copied from the capture");
        store.posted(2, 1);
        assert!(store.read_into(2, 1, 3, &mut dst));
        assert_eq!((dst, reads.load(Ordering::Relaxed)), ([3; 8], 3), "read back into dst");
        assert_eq!(store.latest(|c| c.rgba[0]), Some(4), "the capture wasn't updated: read again");
        assert!(!store.read_into(2, 1, 3, &mut [0; 7]), "wrong size");
        assert!(!store.read_into(2, 1, 0, &mut dst), "no frame");
    }

    #[test]
    fn a_failed_readback_falls_back_to_presented_pixels_or_nothing() {
        let store = FrameStore::default();
        store.set_readback(Box::new(|_, _, _| false));
        store.posted(1, 1);
        assert_eq!(store.latest(|_| ()), None, "no readback, no pixels");
        assert!(!store.read_into(1, 1, 1, &mut [0; 4]));
        // Guest composition presents pixels; the renderer has nothing posted to read.
        store.present(1, 1, 4, 67, &[9, 8, 7, 6]);
        assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![9, 8, 7, 6]));
        let mut dst = [0; 4];
        store.present(1, 1, 4, 67, &[1, 2, 3, 4]);
        assert!(store.read_into(1, 1, 3, &mut dst), "converted presented pixels");
        assert_eq!(dst, [1, 2, 3, 4]);
    }

    /// A shared surface from a pool of `free` (a count shared with the fake renderer).
    struct FakeSurface {
        size: (u32, u32),
        free: Arc<AtomicUsize>,
    }

    impl SharedSurface for FakeSurface {
        fn size(&self) -> (u32, u32) {
            self.size
        }
        fn mach_port(&self) -> Option<u32> {
            None
        }
    }

    impl Drop for FakeSurface {
        fn drop(&mut self) {
            self.free.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A store whose renderer copies posts into a pool of `pool` surfaces; the copies made, and
    /// the pool's free count.
    fn surface_store(pool: usize) -> (FrameStore, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let store = FrameStore::default();
        let (copies, free) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(pool)));
        let (c, f) = (copies.clone(), free.clone());
        store.set_surface_readback(Box::new(move |w, h| {
            // Take one from the pool, if one is free.
            f.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).ok()?;
            c.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(FakeSurface { size: (w, h), free: f.clone() }) as Arc<dyn SharedSurface>)
        }));
        store.set_readback(Box::new(|_, _, buf| {
            buf.fill(0xAB);
            true
        }));
        (store, copies, free)
    }

    #[test]
    fn posted_frames_become_one_shared_surface_each_however_many_readers() {
        let (store, copies, free) = surface_store(3);
        assert!(store.has_surfaces() && store.latest_surface().is_none(), "nothing posted yet");
        store.posted(4, 2);
        let (n, a) = store.latest_surface().expect("a surface");
        let (m, b) = store.latest_surface().expect("the same");
        assert_eq!((n, m, copies.load(Ordering::Relaxed)), (1, 1, 1), "one copy for both readers");
        assert!(Arc::ptr_eq(&a, &b) && a.size() == (4, 2));
        // The next frame is copied again; the last one's surface stays with whoever holds it.
        store.posted(4, 2);
        let (n, c) = store.latest_surface().expect("a surface");
        assert_eq!((n, copies.load(Ordering::Relaxed), free.load(Ordering::Relaxed)), (2, 2, 1));
        drop((a, b));
        assert_eq!(free.load(Ordering::Relaxed), 2, "released by its last holder");
        drop(c);
        assert_eq!(free.load(Ordering::Relaxed), 2, "the store keeps the latest frame's");
    }

    #[test]
    fn frames_without_a_surface_are_read_as_pixels() {
        let (store, copies, free) = surface_store(1);
        store.posted(2, 2);
        let held = store.latest_surface().expect("the only surface");
        // The pool is empty while a client holds it: the next frame has no surface, once, and
        // its pixels come from the readback as before.
        store.posted(2, 2);
        assert!(store.latest_surface().is_none());
        assert!(store.latest_surface().is_none(), "tried once per frame");
        assert_eq!(copies.load(Ordering::Relaxed), 1);
        assert_eq!(store.latest(|c| c.rgba.clone()), Some(vec![0xAB; 16]));
        drop(held);
        store.posted(2, 2);
        assert!(store.latest_surface().is_some(), "free again: the next frame is a surface");
        // Presented pixels (guest composition, a blank screen) are never a surface.
        store.present(2, 2, 8, 67, &[1; 16]);
        assert!(store.latest_surface().is_none());
        // Nor is a view of the scanout.
        store.posted(2, 2);
        store.set_view(Some((0, 0, 1, 2)));
        assert!(store.latest_surface().is_none());
        assert_eq!(store.latest(|c| (c.width, c.height)), Some((1, 2)));
        // A store nobody gave a surface readback has none.
        let plain = FrameStore::default();
        plain.posted(2, 2);
        assert!(!plain.has_surfaces() && plain.latest_surface().is_none());
        assert_eq!(free.load(Ordering::Relaxed), 0, "the store holds the last surface it had");
    }

    #[test]
    fn an_unread_stores_surface_goes_back_to_the_pool() {
        let store = FrameStore::with_idle(Duration::from_millis(20));
        let free = Arc::new(AtomicUsize::new(1));
        let f = free.clone();
        store.set_surface_readback(Box::new(move |w, h| {
            f.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).ok()?;
            Some(Arc::new(FakeSurface { size: (w, h), free: f.clone() }) as Arc<dyn SharedSurface>)
        }));
        store.posted(2, 2);
        drop(store.latest_surface().expect("a surface"));
        assert_eq!(free.load(Ordering::Relaxed), 0, "kept for its frame");
        std::thread::sleep(Duration::from_millis(40));
        store.posted(2, 2);
        assert_eq!(free.load(Ordering::Relaxed), 1, "unread past the idle period: released");
    }
}
