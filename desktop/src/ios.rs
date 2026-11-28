//! A live iOS device: a capture thread that presents its framebuffer, and the HID session input goes
//! to. Everything except the capture thread lives on the UI thread. macOS only.

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
use sim_coresim::{Appearance, Device, Display, Frame, Hid, Surface};

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
    /// screen while it is unfolded).
    pub fn touch(&self, x: f32, y: f32, phase: Phase) {
        let (Some(hid), Some(screen)) = (&self.hid, self.screen()) else { return };
        let at = screen.normalize(Point::new(f64::from(x), f64::from(y)));
        let sent = match self.shared.screen_id.load(Ordering::Relaxed) {
            0 => hid.touch(at, phase),
            id => hid.screen_touch(id, at, phase, None),
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
fn spawn_capture(udid: String, native: NativeSurface, shared: Arc<Shared>, kicked: Arc<AtomicBool>) -> Result<JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("simee-capture".into())
        .spawn(move || {
            let (main, inner) = match Context::new().and_then(|c| c.device(&udid)).and_then(|d| d.builtin_displays()) {
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
            let watch = watch_frames(&main);
            // An iPhone Duo's inner screen announces its frames too; without, the safety look finds them.
            let _inner_watch = inner.as_ref().and_then(|d| watch_frames(d).ok());
            let mut shown = Screen::Outer;
            let mut pacer = Pacer::new(Pacing::FRAMEBUFFER, Instant::now());
            match &watch {
                Ok(_) => pacer = pacer.announcing(),
                Err(e) => eprintln!("capture: no frame callback ({e}); polling"),
            }
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
                    sim_coresim::autoreleasepool(|| {
                        let display = match &inner {
                            Some(inner) => {
                                let next = follow_fold(&main, inner, shown, &shared);
                                if next != shown {
                                    shown = next;
                                    current = None;
                                }
                                if shown == Screen::Inner { inner } else { &main }
                            }
                            None => &main,
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
                    if Some(seed) != last_seed {
                        changed = last_seed.is_some();
                        last_seed = Some(seed);
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
            drop(watch);
            native.clear();
        })
        .map_err(|e| e.to_string())
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
