//! A live iOS device: a capture thread that presents its framebuffer (an iPhone's CarPlay display
//! in its place while that shows), and the HID session input goes to. Everything except the
//! capture thread lives on the UI thread. macOS only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use std::time::Instant;

use rune::NativeSurface;
use rune::native_surface::macos::PixelBuffer;
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::duo::Screen;
use sim_core::geometry::{Point, Size};
use sim_core::gesture::Phase;
use sim_core::hid::Direction;
use sim_core::hid::{Button, CROWN_UNITS_PER_TICK, KEY_LEFT_SHIFT, KeyStroke, Orientation};
use sim_core::pacing::{Pacer, Pacing};
pub use sim_coresim::Context;
use sim_coresim::{Appearance, Device, Display, Frame, FrameWatch, Hid, Surface};

use crate::device::Shared;

/// A connected, booted device.
pub struct Live {
    pub udid: String,
    device: Device,
    hid: Option<Hid>,
    pub shared: Arc<Shared>,
    capture: Option<JoinHandle<()>>,
    /// Set when input goes out, so the capture thread looks for its response at once.
    kicked: Arc<AtomicBool>,
    pub dark: bool,
    orientation: Orientation,
}

impl Live {
    /// Connects to a booted device and starts presenting its screen in `native`.
    pub fn connect(ctx: &Context, udid: &str, native: NativeSurface, waker: rune::Waker) -> Result<Self, String> {
        let device = ctx.device(udid).map_err(|e| e.to_string())?;
        let hid = device.hid().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared::default());
        shared.set_waker(waker);
        let kicked = Arc::new(AtomicBool::new(false));
        let capture = spawn_capture(udid.to_owned(), native, shared.clone(), kicked.clone())?;
        Ok(Self {
            udid: udid.to_owned(),
            device,
            hid: Some(hid),
            shared,
            capture: Some(capture),
            kicked,
            dark: false,
            orientation: Orientation::Portrait,
        })
    }

    fn screen(&self) -> Option<Size> {
        self.shared.size().map(|(w, h)| Size::new(f64::from(w), f64::from(h)))
    }

    /// Input went out: wake the capture thread to look for the response.
    fn kick(&self) {
        self.kicked.store(true, Ordering::Relaxed);
        if let Some(t) = &self.capture {
            t.thread().unpark();
        }
    }

    /// Sends a touch at a point given in device pixels, on the screen shown (an iPhone Duo's inner
    /// screen while it is unfolded, the CarPlay display while it shows, through its HID service).
    pub fn touch(&self, x: f32, y: f32, phase: Phase) {
        let (Some(hid), Some(screen)) = (&self.hid, self.screen()) else { return };
        let at = screen.normalize(Point::new(f64::from(x), f64::from(y)));
        let carplay = self.shared.showing_carplay.load(Ordering::Relaxed);
        let sent = match Target::of(carplay, self.shared.screen_id.load(Ordering::Relaxed)) {
            Target::CarPlay => hid.carplay_touch(at, phase),
            Target::Main => hid.touch(at, phase),
            Target::Screen(id) => hid.screen_touch(id, at, phase, None),
        };
        if let Err(e) = sent {
            eprintln!("touch: {e}");
        }
        self.kick();
    }

    /// Presses or releases a hardware button. Callers schedule the release so the UI never sleeps.
    pub fn button(&self, button: Button, direction: Direction) {
        let Some(hid) = &self.hid else { return };
        if let Err(e) = hid.button(button, direction) {
            eprintln!("button: {e}");
        }
        self.kick();
    }

    /// Turns an Apple Watch's Digital Crown by rotary ticks (negative turns back).
    pub fn crown(&self, ticks: i32) {
        let Some(hid) = &self.hid else { return };
        if let Err(e) = hid.crown(f64::from(ticks) * CROWN_UNITS_PER_TICK) {
            eprintln!("crown: {e}");
        }
        self.kick();
    }

    /// Rotates the device 90° clockwise from the orientation this window last set.
    pub fn rotate(&mut self) {
        let next = self.orientation.clockwise();
        match self.device.set_orientation(next) {
            Ok(()) => self.orientation = next,
            Err(e) => eprintln!("rotate: {e}"),
        }
        self.kick();
    }

    /// The device's data directory on the host.
    pub fn data_path(&self) -> String {
        self.device.data_path()
    }

    /// Copies the current frame of the screen shown (for saving screenshots).
    pub fn capture(&self) -> Result<Frame, String> {
        if self.shared.showing_carplay.load(Ordering::Relaxed) {
            return self.device.external_display().and_then(|d| d.capture()).map_err(|e| e.to_string());
        }
        let (main, inner) = self.device.builtin_displays().map_err(|e| e.to_string())?;
        let shown = self.shared.screen_id.load(Ordering::Relaxed);
        let display = inner.filter(|d| shown != 0 && d.screen_id() == Some(shown)).unwrap_or(main);
        display.capture().map_err(|e| e.to_string())
    }

    pub fn strokes(&self, strokes: &[KeyStroke]) {
        let Some(hid) = &self.hid else { return };
        for s in strokes {
            let r = (|| {
                if s.shift {
                    hid.key(KEY_LEFT_SHIFT, Direction::Down)?;
                }
                hid.key(s.usage, Direction::Down)?;
                hid.key(s.usage, Direction::Up)?;
                if s.shift {
                    hid.key(KEY_LEFT_SHIFT, Direction::Up)?;
                }
                Ok::<(), sim_coresim::Error>(())
            })();
            if let Err(e) = r {
                eprintln!("key: {e}");
            }
        }
        self.kick();
    }

    pub fn toggle_appearance(&mut self) {
        self.dark = !self.dark;
        let mode = if self.dark { Appearance::Dark } else { Appearance::Light };
        if let Err(e) = self.device.set_appearance(mode) {
            eprintln!("appearance: {e}");
        }
        self.kick();
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.device.shutdown().map_err(|e| e.to_string())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.hid = None;
        if let Some(t) = self.capture.take() {
            t.thread().unpark();
            t.join().ok();
        }
    }
}

/// Shows the device framebuffer without copying it: wraps its IOSurface once (again when the
/// device swaps surfaces, e.g. on rotation) and re-presents it whenever its seed changes. The
/// device announces each frame (the display's frame callback wakes this thread), so a still screen
/// costs a safety look every 100 ms; without announcements (or once they stop matching the seed)
/// it polls, fast while the screen changes and backing off to 32 ms when still, and input from
/// the window wakes it at once. A size change wakes the window the device is in now
/// ([`Shared::wake`]). The thread opens its own service connection: service objects stay on the
/// thread that made them.
///
/// While the window asks for an iPhone's CarPlay display ([`Shared::carplay`]) it presents that
/// display's surface instead, the same way and as soon as it has rendered, and says so
/// ([`Shared::showing_carplay`]); its frame callback replaces the iPhone's. Whether that TVOut
/// surface's seed counts its frames is unmeasured (#505), so until its seed is seen to move its
/// contents' hash (read in place, never copied) is compared at the 250 ms check, and if they change
/// under a still seed the hash tells its frames from then on ([`Change`]). Either way the view
/// samples the surface itself: no pixel is copied to show it.
fn spawn_capture(udid: String, native: NativeSurface, shared: Arc<Shared>, kicked: Arc<AtomicBool>) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("simee-capture".into())
        .spawn(move || {
            let opened = Context::new().and_then(|c| c.device(&udid)).and_then(|d| d.builtin_displays().map(|(m, i)| (d, m, i)));
            let (device, main, inner) = match opened {
                Ok(d) => d,
                Err(e) => return eprintln!("capture: {e}"),
            };
            let announced = Arc::new(AtomicBool::new(false));
            let watch_frames = |display: &Display| {
                let (announced, this) = (announced.clone(), std::thread::current());
                display.watch_frames(move || {
                    announced.store(true, Ordering::Release);
                    this.unpark();
                })
            };
            let mut watch = watch_frames(&main);
            // An iPhone Duo's inner screen announces its frames too; without, the safety look finds them.
            let mut inner_watch = inner.as_ref().and_then(|d| watch_frames(d).ok());
            let mut shown = Screen::Outer;
            let mut pacer = pacer_for(&watch, Instant::now());
            // The CarPlay display while it is presented, and how its frames (or the iPhone's) are told.
            let mut carplay: Option<Display> = None;
            let mut change = Change::Seed;
            let mut verify = false;
            let mut current: Option<(Surface, PixelBuffer)> = None;
            let mut last_seed = None;
            let mut frame_id = 0u64;
            let mut next_check = Instant::now();
            while !shared.stop.load(Ordering::Relaxed) {
                if kicked.swap(false, Ordering::Relaxed) {
                    pacer.kick(Instant::now());
                }
                let announced = announced.swap(false, Ordering::Acquire);
                if current.is_none() || Instant::now() >= next_check {
                    next_check = Instant::now() + Duration::from_millis(250);
                    verify = true;
                    sim_coresim::autoreleasepool(|| {
                        // CarPlay follows the window's switch: on once its display has rendered, off at once.
                        let switch = match (shared.carplay.load(Ordering::Relaxed), carplay.is_some()) {
                            (true, false) => device.external_display().ok().map(Some),
                            (false, true) => Some(None),
                            _ => None,
                        };
                        if let Some(next) = switch {
                            carplay = next;
                            current = None;
                            shared.showing_carplay.store(carplay.is_some(), Ordering::Relaxed);
                            // Only the screen presented wakes this thread with its frames.
                            watch = watch_frames(carplay.as_ref().unwrap_or(&main));
                            inner_watch = if carplay.is_some() { None } else { inner.as_ref().and_then(|d| watch_frames(d).ok()) };
                            pacer = pacer_for(&watch, Instant::now());
                            change = if carplay.is_some() { Change::Unverified(None) } else { Change::Seed };
                        }
                        let display = match (&carplay, &inner) {
                            (Some(carplay), _) => carplay,
                            (None, Some(inner)) => {
                                let next = follow_fold(&main, inner, shown, &shared);
                                if next != shown {
                                    shown = next;
                                    current = None;
                                }
                                if shown == Screen::Inner { inner } else { &main }
                            }
                            (None, None) => &main,
                        };
                        if let Some(surface) = display.surface_handle()
                            && current.as_ref().is_none_or(|(s, _)| s.as_raw() != surface.as_raw())
                        {
                            match crate::surface::wrap(&surface) {
                                Ok(buffer) => {
                                    let (w, h) = surface.size();
                                    let packed = (w << 16) | (h & 0xffff);
                                    if shared.size.swap(packed, Ordering::Relaxed) != packed {
                                        shared.wake();
                                    }
                                    current = Some((surface, buffer));
                                    last_seed = None;
                                }
                                Err(e) => eprintln!("capture: {e}"),
                            }
                        }
                    });
                }
                let mut changed = false;
                if let Some((surface, buffer)) = &current {
                    let seed = surface.seed();
                    let first = last_seed.is_none();
                    let moved = !first && last_seed != Some(seed);
                    last_seed = Some(seed);
                    let contents = || carplay.as_ref().and_then(|d| sim_coresim::autoreleasepool(|| d.content_hash().ok())).map(|(_, h)| h);
                    let before = change;
                    let fresh = change.look(moved, first || std::mem::take(&mut verify), contents);
                    if matches!((before, change), (Change::Unverified(_), Change::Contents(_))) {
                        eprintln!("capture: the CarPlay surface's seed doesn't follow its frames; comparing its contents");
                    }
                    if first || fresh {
                        changed = fresh;
                        frame_id += 1;
                        if let Err(e) = native.push(buffer.clone(), frame_id) {
                            eprintln!("present: {e}");
                        }
                        shared.frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let trusted = pacer.trusts_announcements();
                let wait = pacer.polled(changed, announced, Instant::now());
                if trusted && !pacer.trusts_announcements() {
                    eprintln!("capture: frames changed without their callback; polling");
                }
                std::thread::park_timeout(wait);
            }
            drop((watch, inner_watch));
            shared.showing_carplay.store(false, Ordering::Relaxed);
            native.clear();
        })
        .map_err(|e| e.to_string())
}

/// A pacer for a screen whose frame callback is `watch`: trusting its announcements when it
/// registered, polling when it didn't.
fn pacer_for(watch: &sim_coresim::Result<FrameWatch>, now: Instant) -> Pacer {
    let pacer = Pacer::new(Pacing::FRAMEBUFFER, now);
    match watch {
        Ok(_) => pacer.announcing(),
        Err(e) => {
            eprintln!("capture: no frame callback ({e}); polling");
            pacer
        }
    }
}

/// How the capture thread tells a new frame of the screen it presents. The main screen's surface
/// seed counts its frames ([`Change::Seed`]). CarPlay's TVOut surface starts unverified: a seed
/// that moves makes it seeded for good; contents that change under a still seed (their hash, read
/// in place) make it [`Change::Contents`], whose every look compares the hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Seed,
    /// The contents' hash at the last verification, if one was read.
    Unverified(Option<u64>),
    /// The contents' hash at the last look.
    Contents(u64),
}

impl Change {
    /// One look: whether the seed moved since the last, whether this look verifies (an unverified
    /// surface reads its contents only then) and the contents' hash, read only when needed (`None`
    /// when unreadable). Returns whether a new frame is there.
    fn look(&mut self, moved: bool, verify: bool, hash: impl FnOnce() -> Option<u64>) -> bool {
        match *self {
            Change::Seed => moved,
            Change::Unverified(_) if moved => {
                *self = Change::Seed;
                true
            }
            Change::Unverified(_) if !verify => false,
            Change::Unverified(before) => {
                let Some(now) = hash() else { return false };
                let changed = before.is_some_and(|b| b != now);
                *self = if changed { Change::Contents(now) } else { Change::Unverified(Some(now)) };
                changed
            }
            Change::Contents(before) => {
                let Some(now) = hash() else { return false };
                *self = Change::Contents(now);
                now != before
            }
        }
    }
}

/// Where a touch on the view goes: the CarPlay display's HID service while it shows, else the
/// device's main digitizer or the screen shown by its ID (an iPhone Duo's inner screen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    CarPlay,
    Main,
    Screen(u32),
}

impl Target {
    fn of(showing_carplay: bool, screen_id: u32) -> Target {
        match (showing_carplay, screen_id) {
            (true, _) => Target::CarPlay,
            (false, 0) => Target::Main,
            (false, id) => Target::Screen(id),
        }
    }
}

/// The screen an iPhone Duo's view shows next ([`sim_core::duo::follow`]: the one in use), with its
/// screen ID published for touches.
fn follow_fold(main: &Display, inner: &Display, shown: Screen, shared: &Shared) -> Screen {
    let next = sim_core::duo::follow(shown, main.lit(), inner.lit());
    let id = if next == Screen::Inner { inner.screen_id() } else { main.screen_id() };
    shared.screen_id.store(id.unwrap_or(0), Ordering::Relaxed);
    next
}

/// Lists devices for the picker. Errors become an empty list plus a message.
pub fn list(ctx: &Context) -> Result<Vec<DeviceInfo>, String> {
    let mut devices: Vec<DeviceInfo> = ctx.devices().map_err(|e| e.to_string())?.iter().map(Device::info).filter(|d| d.available).collect();
    devices.sort_by_key(|d| (d.state != DeviceState::Booted, d.runtime_id.clone(), d.name.clone()));
    Ok(devices)
}

/// Boots a device on a worker thread (booting blocks for seconds).
pub fn boot_async(udid: String, done: Arc<AtomicBool>, error: Arc<std::sync::Mutex<Option<String>>>, waker: rune::Waker) {
    std::thread::spawn(move || {
        let result = Context::new().and_then(|c| c.device(&udid)).and_then(|d| {
            if d.state() != DeviceState::Booted {
                d.boot()?;
            }
            for _ in 0..1200 {
                if d.boot_finished().unwrap_or(true) && d.display().is_ok_and(|x| x.surface().is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        });
        if let Err(e) = result
            && let Ok(mut slot) = error.lock()
        {
            *slot = Some(e.to_string());
        }
        done.store(true, Ordering::Relaxed);
        waker();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A look that must not read the contents.
    fn unread() -> Option<u64> {
        panic!("the contents were read")
    }

    /// The main screen's seed counts its frames: only the seed is looked at, never the pixels.
    #[test]
    fn a_seeded_screen_changes_with_its_seed_and_never_reads_its_contents() {
        let mut change = Change::Seed;
        assert!(change.look(true, true, unread));
        assert!(!change.look(false, true, unread));
        assert_eq!(change, Change::Seed);
    }

    /// CarPlay's TVOut surface (#505): a seed that moves makes it a seeded screen for good, with no
    /// more reads of its contents.
    #[test]
    fn an_unverified_screen_whose_seed_moves_is_seeded_from_then_on() {
        let mut change = Change::Unverified(None);
        assert!(!change.look(false, true, || Some(7)), "the first read is the baseline");
        assert!(!change.look(false, false, unread), "between verifications nothing is read");
        assert!(change.look(true, true, unread));
        assert_eq!(change, Change::Seed);
        assert!(!change.look(false, true, unread));
    }

    /// A surface whose contents change under a still seed has no usable seed: from then on each
    /// look compares the contents' hash (read in place; the view still samples the surface).
    #[test]
    fn contents_that_change_under_a_still_seed_are_followed_by_their_hash() {
        let mut change = Change::Unverified(None);
        assert!(!change.look(false, true, || Some(1)));
        assert!(!change.look(false, true, || Some(1)), "still contents: still unverified");
        assert_eq!(change, Change::Unverified(Some(1)));
        assert!(change.look(false, true, || Some(2)), "changed under a still seed");
        assert_eq!(change, Change::Contents(2));
        assert!(!change.look(false, false, || Some(2)), "every look reads now, and still is still");
        assert!(change.look(false, false, || Some(3)));
        assert!(!change.look(false, false, || None), "an unreadable surface is no change");
        assert_eq!(change, Change::Contents(3));
    }

    /// Touches go to CarPlay's HID service while its display shows, else to the screen shown
    /// (the main digitizer, or an iPhone Duo's inner screen by its ID).
    #[test]
    fn touches_go_to_the_screen_shown() {
        assert_eq!(Target::of(true, 0), Target::CarPlay);
        assert_eq!(Target::of(true, 3), Target::CarPlay);
        assert_eq!(Target::of(false, 0), Target::Main);
        assert_eq!(Target::of(false, 3), Target::Screen(3));
    }
}
