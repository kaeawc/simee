//! `device:` camera sources (#325): another device's live screen, read here in the simee server
//! and never in emu. `sim_camera::device` has the id, the crop and the source over a `Screen`; this
//! module has what posts to that screen, which links the device's platform stack:
//!
//! - **Android** (an AVD name): emu's display socket and shared frame ring
//!   (`sim_android::display`), asked for only what the camera needs: `fps <camera rate>`, and
//!   `hello <width>` with the smallest whole downscale of the screen that keeps the region at the
//!   camera's resolution ([`sim_camera::device::needed_width`]), asked again when the device turns
//!   or a larger (or faster) camera joins the capture: one capture serves the largest subscriber,
//!   scaled down for the others.
//! - **iOS** (a simulator's UDID or name, macOS only): the framebuffer `IOSurface`'s frame
//!   callback, then one copy out of the locked surface (`sim_coresim::Display::capture`) when its
//!   seed says the contents changed, at most at the camera's rate.
//!
//! [`DeviceOpener`] is the server's camera opener: `device:` here, everything else through
//! `sim_camera::HostOpener`. An iOS simulator's feed reads it directly; an Android device's camera
//! reads it through a feed the server serves its emu (`feed:<socket>`, `crate::camera`). emu links
//! `sim-camera` only, so it never links this.
//!
//! [`describe`] resolves a `device:` name for `camera_link` (`booted` included, so a self-link
//! can't hide behind it) and reads the source's screen size now, so a region off the screen is
//! refused when it is linked.

use std::sync::Arc;

use sim_camera::device::{DeviceSource, DeviceSpec, Screen};
use sim_camera::{Error, FrameSpec, HostOpener, Opener, Result, Source, SourceSpec};

/// The simee server's camera opener: `device:` sources read here, the rest as `HostOpener` does.
#[derive(Default)]
pub(crate) struct DeviceOpener(HostOpener);

impl Opener for DeviceOpener {
    fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
        self.0.canonical(spec)
    }

    fn open(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Box<dyn Source>> {
        match source {
            // A device's screen needs no camera permission.
            SourceSpec::Device(device) => open(device, spec),
            other => self.0.open(other, spec),
        }
    }
}

/// Opens `device:` `device` for a capture that starts at `spec`: an AVD of that name, else (macOS)
/// a simulator by UDID, name or `booted`.
fn open(device: &DeviceSpec, spec: FrameSpec) -> Result<Box<dyn Source>> {
    let screen = Arc::new(Screen::default());
    let reader = start(device, screen.clone(), spec)?;
    Ok(Box::new(DeviceSource::new(&device.device, device.region, screen).reading(reader)))
}

#[cfg(unix)]
fn start(device: &DeviceSpec, screen: Arc<Screen>, spec: FrameSpec) -> Result<sim_camera::device::Reader> {
    let name = &device.device;
    let env = sim_android::Env::from_env();
    if env.has_avd(name) {
        let avd = env.load(name).map_err(|e| Error::Io(format!("device:{name}: {e}")))?;
        let lcd = lcd(|key| avd.get(key));
        return Ok(Box::new(android::Reader::start(&avd.dir, name, screen, android::Want { region: device.region, spec, lcd })?));
    }
    ios(name, screen, spec)
}

#[cfg(not(unix))]
fn start(device: &DeviceSpec, _screen: Arc<Screen>, _spec: FrameSpec) -> Result<sim_camera::device::Reader> {
    Err(Error::Unsupported(format!("device:{}: a device's screen as a camera source needs macOS or Linux", device.device)))
}

/// The AVD's screen size as configured (`hw.lcd.width` x `hw.lcd.height`), the first guess at what
/// to ask its display for.
#[cfg(unix)]
fn lcd<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<(u32, u32)> {
    let side = |key: &str| get(key).and_then(|v| v.trim().parse::<u32>().ok()).filter(|&v| v > 0);
    Some((side("hw.lcd.width")?, side("hw.lcd.height")?))
}

#[cfg(target_os = "macos")]
fn ios(name: &str, screen: Arc<Screen>, spec: FrameSpec) -> Result<sim_camera::device::Reader> {
    Ok(Box::new(ios::Reader::start(name, screen, spec)?))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn ios(name: &str, _screen: Arc<Screen>, _spec: FrameSpec) -> Result<sim_camera::device::Reader> {
    Err(Error::Unsupported(format!(
        "device:{name}: no AVD is named {name:?}, and an iOS simulator's screen as a camera source needs macOS"
    )))
}

/// A `device:` source's device as `camera_link` checks it: the names it goes by (an AVD's name; a
/// simulator's UDID and name, which `booted` resolves to) and its screen size now, when it is
/// running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceDevice {
    pub names: Vec<String>,
    pub screen: Option<(u32, u32)>,
}

/// Finds the device `query` names (`None`: no device of that name now, which the link saves for
/// when there is one). `booted` must resolve: it is the booted simulator, and an error otherwise.
pub(crate) fn describe(query: &str) -> std::result::Result<Option<SourceDevice>, String> {
    let booted = query.eq_ignore_ascii_case("booted");
    #[cfg(unix)]
    if !booted {
        let env = sim_android::Env::from_env();
        if env.has_avd(query) {
            let screen = env.load(query).ok().and_then(|avd| android::screen_size(&avd.dir));
            return Ok(Some(SourceDevice { names: vec![query.to_owned()], screen }));
        }
    }
    let found = ios_describe(query)?;
    if booted && found.is_none() {
        return Err("device:booted names the booted iOS simulator, and none is booted: name the device (its UDID or name)".into());
    }
    Ok(found)
}

#[cfg(target_os = "macos")]
fn ios_describe(query: &str) -> std::result::Result<Option<SourceDevice>, String> {
    use sim_core::device::DeviceState;
    sim_coresim::autoreleasepool(|| {
        let Ok(set) = crate::params::device_set(&serde_json::Value::Null) else { return Ok(None) };
        let Ok(context) = sim_coresim::Context::open(set.as_deref()) else { return Ok(None) };
        let Ok(device) = context.device(query) else { return Ok(None) };
        let info = device.selection_info();
        let screen = (info.state == DeviceState::Booted).then(|| device.display().and_then(|d| d.size()).ok()).flatten();
        Ok(Some(SourceDevice { names: vec![info.udid, info.name], screen }))
    })
}

#[cfg(not(target_os = "macos"))]
fn ios_describe(_query: &str) -> std::result::Result<Option<SourceDevice>, String> {
    Ok(None)
}

/// Android: emu's display socket, frames through its shared ring.
#[cfg(unix)]
mod android {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use sim_android::display::{Display, Frames};
    use sim_camera::device::{Region, Screen, needed_width};
    use sim_camera::{Error, FrameSpec, Result};
    use sim_core::stream::Input;

    /// What the camera needs of the source device's display.
    pub(super) struct Want {
        pub region: Option<Region>,
        pub spec: FrameSpec,
        /// The screen size to plan the first request by (the AVD's configured one).
        pub lcd: Option<(u32, u32)>,
    }

    /// What the display has been asked for, shared by the reader thread (which learns the device's
    /// size from its frames) and the screen's asker (which learns what the capture needs as
    /// subscribers join): either one asks again when what it knows changes the answer.
    struct Asking {
        control: UnixStream,
        region: Option<Region>,
        /// The device's screen size: the configured one until a frame says.
        device: Option<(u32, u32)>,
        /// The capture's subscriber specs: the first one's, then every one the screen says.
        specs: Vec<FrameSpec>,
        width: u32,
        fps: u32,
    }

    impl Asking {
        /// The `hello` width the largest need takes (full size while the device's size is unknown).
        fn needed(&self) -> u32 {
            let Some(device) = self.device else { return u32::MAX };
            self.specs.iter().map(|s| needed_width(self.region, device, *s)).max().unwrap_or(u32::MAX)
        }

        /// Asks the display again for what changed: a higher rate (`fps`), another width (`hello`).
        fn ask(&mut self) {
            let fps = self.specs.iter().map(|s| s.fps).max().unwrap_or(self.fps);
            if fps > self.fps && writeln!(self.control, "fps {fps}").is_ok() {
                self.fps = fps;
            }
            let needed = self.needed();
            if needed != self.width && writeln!(self.control, "{}", Input::Hello { max_width: needed }.to_line()).is_ok() {
                self.width = needed;
            }
        }
    }

    /// The reader thread of one display connection.
    pub(super) struct Reader {
        link: UnixStream,
        thread: Option<JoinHandle<()>>,
    }

    impl Reader {
        /// Connects to the AVD in `avd_dir`, asks for frames at the camera's rate and the size it
        /// needs, and posts them to `screen`. A larger or faster subscriber joining the capture
        /// later ([`Screen::want`]) asks again for its size and rate, at once: one capture serves
        /// the largest, scaled down for the others.
        pub(super) fn start(avd_dir: &Path, name: &str, screen: Arc<Screen>, want: Want) -> Result<Reader> {
            let io = |e: String| Error::Io(format!("device:{name}: {e}"));
            let mut display = Display::connect(avd_dir).map_err(|e| Error::Missing(format!("{name} isn't running under emu: {e}")))?;
            display.request_shared().map_err(io)?;
            let mut control = display.try_clone().map_err(io)?;
            // The display server's rate cap: without it, a 60 Hz stream whatever the camera takes.
            writeln!(control, "fps {}", want.spec.fps).map_err(|e| io(e.to_string()))?;
            let mut asking =
                Asking { control, region: want.region, device: want.lcd, specs: vec![want.spec], width: 0, fps: want.spec.fps };
            asking.width = asking.needed();
            display.send(&Input::Hello { max_width: asking.width }).map_err(io)?;
            let asking = Arc::new(Mutex::new(asking));
            let (first, on_want) = (want.spec, asking.clone());
            screen.on_want(move |specs| {
                let mut asking = on_want.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                asking.specs = specs.to_vec();
                if !asking.specs.iter().any(|s| (s.width, s.height) == (first.width, first.height)) {
                    asking.specs.push(first);
                }
                asking.ask();
            });
            let mut frames = Frames::new(display.try_clone().map_err(io)?).with_fallback(display.try_clone().map_err(io)?);
            let link = display.try_clone().map_err(io)?;
            let name = name.to_string();
            let thread = std::thread::Builder::new()
                .name("camera device android".into())
                .spawn(move || {
                    let mut buf = Vec::new();
                    loop {
                        match frames.newest_frame_into(&mut buf, || true) {
                            Ok(h) => {
                                let device = (h.device_width, h.device_height);
                                // A turned device (or a first guess that was off): ask again.
                                {
                                    let mut asking = asking.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    if asking.device != Some(device) {
                                        asking.device = Some(device);
                                        asking.ask();
                                    }
                                }
                                buf = screen.post(h.width, h.height, device, std::mem::take(&mut buf));
                            }
                            Err(e) => return screen.close(format!("{name}'s display closed (the device shut down?): {e}")),
                        }
                    }
                })
                .map_err(|e| Error::Capture(format!("can't start the device reader: {e}")))?;
            Ok(Reader { link, thread: Some(thread) })
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            // Closing both directions wakes the reader out of its blocking read.
            let _ = self.link.shutdown(std::net::Shutdown::Both);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// The running AVD in `avd_dir`'s screen size now: one small frame's header off its display
    /// (a still screen sends its first frame at once). `None` when it isn't running or doesn't say
    /// within a second.
    pub(super) fn screen_size(avd_dir: &Path) -> Option<(u32, u32)> {
        let mut display = Display::connect(avd_dir).ok()?;
        let mut reader = display.try_clone().ok()?;
        reader.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
        display.send(&Input::Hello { max_width: 32 }).ok()?;
        let (h, _) = sim_android::display::read_frame(&mut reader).ok()?;
        let _ = reader.shutdown(std::net::Shutdown::Both);
        Some((h.device_width, h.device_height)).filter(|&(w, h)| w > 0 && h > 0)
    }
}

/// iOS: the framebuffer surface's frame callback. The service's objects aren't `Send`, so one
/// thread makes the context, finds the device and reads its display for the capture's life.
#[cfg(target_os = "macos")]
mod ios {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use sim_camera::device::Screen;
    use sim_camera::{Error, FrameSpec, Result};
    use sim_core::device::DeviceState;
    use sim_coresim::{Context, Device, Display, FrameWatch};

    /// How often an idle reader checks that the device is still booted (and whether to stop).
    const IDLE: Duration = Duration::from_millis(250);

    /// "The device presented a frame", from the service's callback thread.
    #[derive(Default)]
    struct Presented {
        flag: Mutex<bool>,
        cv: Condvar,
    }

    impl Presented {
        fn post(&self) {
            *self.flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.cv.notify_all();
        }

        /// Waits up to `timeout`: whether a frame was presented.
        fn wait(&self, timeout: Duration) -> bool {
            let flag = self.flag.lock().unwrap_or_else(|p| p.into_inner());
            let (mut flag, _) = self.cv.wait_timeout_while(flag, timeout, |f| !*f).unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *flag)
        }
    }

    pub(super) struct Reader {
        stop: Arc<AtomicBool>,
        presented: Arc<Presented>,
        thread: Option<JoinHandle<()>>,
    }

    impl Reader {
        /// Finds simulator `name` (a UDID, a name or `booted`), checks it is booted, and posts its
        /// main display's frames to `screen`, at most at `spec`'s rate; fails here when it can't.
        pub(super) fn start(name: &str, screen: Arc<Screen>, spec: FrameSpec) -> Result<Reader> {
            let (stop, presented) = (Arc::new(AtomicBool::new(false)), Arc::new(Presented::default()));
            let (ready, opened) = mpsc::channel();
            let (n, s, p) = (name.to_string(), stop.clone(), presented.clone());
            let thread = std::thread::Builder::new()
                .name("camera device ios".into())
                .spawn(move || run(&n, &screen, spec.interval(), &s, &p, &ready))
                .map_err(|e| Error::Capture(format!("can't start the device reader: {e}")))?;
            match opened.recv() {
                Ok(Ok(())) => Ok(Reader { stop, presented, thread: Some(thread) }),
                Ok(Err(e)) => {
                    let _ = thread.join();
                    Err(e)
                }
                Err(_) => {
                    let _ = thread.join();
                    Err(Error::Capture(format!("device:{name}: the device reader stopped before it started")))
                }
            }
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.presented.post();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn open(name: &str, presented: &Arc<Presented>) -> Result<(Context, Device, Display, FrameWatch)> {
        // A device a camera follows is in the environment's device set (#409), as the link outlives the call.
        let set = crate::params::device_set(&serde_json::Value::Null).map_err(|e| Error::Invalid(format!("device:{name}: {e}")))?;
        let context =
            Context::open(set.as_deref()).map_err(|e| Error::Unavailable(format!("device:{name}: the simulator service: {e}")))?;
        let device =
            context.device(name).map_err(|e| Error::Invalid(format!("device:{name}: no AVD or simulator is named {name:?} ({e})")))?;
        if device.state() != DeviceState::Booted {
            return Err(Error::Missing(format!("{name} isn't booted; boot it first (`simee boot {name}`)")));
        }
        let display = device.display().map_err(|e| Error::Missing(format!("{name} has no display yet: {e}")))?;
        let p = presented.clone();
        let watch =
            display.watch_frames(move || p.post()).map_err(|e| Error::Unavailable(format!("device:{name}: frame callback: {e}")))?;
        Ok((context, device, display, watch))
    }

    fn run(
        name: &str,
        screen: &Screen,
        interval: Duration,
        stop: &AtomicBool,
        presented: &Arc<Presented>,
        ready: &mpsc::Sender<Result<()>>,
    ) {
        let (_context, device, display, _watch) = match sim_coresim::autoreleasepool(|| open(name, presented)) {
            Ok(opened) => opened,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        // The surface already on screen counts as the first frame.
        let mut frame = true;
        let mut seed = None;
        let mut last: Option<Instant> = None;
        while !stop.load(Ordering::Acquire) {
            if frame {
                // No faster than the camera takes frames: a 60 Hz screen is read at its rate.
                if let Some(wait) = last.map(|t| interval.saturating_sub(t.elapsed())).filter(|w| !w.is_zero()) {
                    std::thread::sleep(wait);
                }
                let now = display.seed();
                if seed != Some(now)
                    && let Ok(f) = sim_coresim::autoreleasepool(|| display.capture())
                {
                    seed = Some(now);
                    last = Some(Instant::now());
                    screen.post(f.width, f.height, (f.width, f.height), f.rgba);
                }
            } else if sim_coresim::autoreleasepool(|| device.state()) != DeviceState::Booted {
                return screen.close(format!("{name} shut down"));
            }
            frame = presented.wait(IDLE);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use sim_camera::device::{Region, Screen};
    use sim_camera::{Error, FrameSpec};
    use sim_core::stream::FrameHeader;

    use super::android;

    /// An AVD directory with a fake display socket, under /tmp (a socket path must fit 104 bytes).
    struct FakeDisplay {
        dir: PathBuf,
        listener: UnixListener,
    }

    impl FakeDisplay {
        fn new(tag: &str) -> FakeDisplay {
            let dir = PathBuf::from("/tmp").join(format!("sa-camdev-{tag}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(dir.join("simee")).expect("state dir");
            let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
            FakeDisplay { dir, listener }
        }
    }

    impl Drop for FakeDisplay {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn frame(conn: &mut impl Write, width: u32, height: u32, device: (u32, u32), frame: u64) {
        let len = width * height * 4;
        let h = FrameHeader { width, height, device_width: device.0, device_height: device.1, frame, len };
        conn.write_all(&h.encode()).and_then(|()| conn.write_all(&vec![7u8; len as usize])).expect("frame");
    }

    /// The reader asks a source emu for the camera's rate and only the size the camera needs (not
    /// `hello <full size>` at 60 Hz), asks again when the device turns, posts its frames, and
    /// reports its display closing as the device gone.
    #[test]
    fn the_android_reader_asks_for_the_cameras_size_and_rate_and_again_when_the_device_turns() {
        let fake = FakeDisplay::new("ask");
        let listener = fake.listener.try_clone().expect("listener");
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            conn.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let mut got: Vec<String> = (0..3).filter_map(|_| lines.next()?.ok()).collect();
            // A portrait 1080x2400 device, sent at a sixth.
            frame(&mut conn, 180, 400, (1080, 2400), 1);
            // It turns: landscape.
            frame(&mut conn, 400, 180, (2400, 1080), 2);
            got.extend(lines.next().and_then(Result::ok));
            thread::sleep(Duration::from_millis(100));
            got
        });
        let screen = Arc::new(Screen::default());
        let want = android::Want { region: None, spec: FrameSpec::new(176, 144, 15).expect("spec"), lcd: Some((1080, 2400)) };
        let reader = android::Reader::start(&fake.dir, "A", screen.clone(), want).expect("start");
        let got = server.join().expect("server");
        // 176x144 from a 1080-wide portrait screen: a sixth (180 >= 176); turned, 2400 wide, the
        // visible part is 1320 of it, so a seventh (343) keeps 176.
        assert_eq!(got, ["shm", "fps 15", "hello 180", "hello 343"]);
        drop(reader);

        // No display socket: the device isn't running.
        let err = android::Reader::start(
            std::path::Path::new("/nonexistent-avd"),
            "B",
            Arc::new(Screen::default()),
            android::Want { region: Some(Region { x: 0, y: 0, width: 10, height: 10 }), spec: FrameSpec::default(), lcd: None },
        )
        .err();
        assert!(matches!(&err, Some(Error::Missing(m)) if m.contains("B isn't running")), "{err:?}");
    }

    /// Two cameras on one capture of A at different sizes: the reader started for the first (small)
    /// one asks again, for the larger one's size and rate, as soon as the larger one subscribes,
    /// so its frames are drawn from A's screen at its own resolution and not upscaled.
    #[test]
    fn the_android_reader_asks_for_the_largest_subscribers_size_when_one_joins() {
        let fake = FakeDisplay::new("grow");
        let listener = fake.listener.try_clone().expect("listener");
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            conn.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let mut got: Vec<String> = (0..3).filter_map(|_| lines.next()?.ok()).collect();
            // A portrait 1080x2400 device, sent at a sixth for the small camera.
            frame(&mut conn, 180, 400, (1080, 2400), 1);
            // The larger camera joins: asked for again (its rate, then its size), without a frame.
            got.extend((0..2).filter_map(|_| lines.next()?.ok()));
            // The display stays up until the test is done with it.
            let _ = finished.recv_timeout(Duration::from_secs(5));
            got
        });
        let screen = Arc::new(Screen::default());
        let small = FrameSpec::new(320, 240, 15).expect("small");
        let want = android::Want { region: None, spec: small, lcd: Some((1080, 2400)) };
        let reader = android::Reader::start(&fake.dir, "A", screen.clone(), want).expect("start");
        let mut source = sim_camera::device::DeviceSource::new("A", None, screen.clone());
        let tick = |seq| sim_camera::Tick { seq, ..sim_camera::Tick::start() };
        assert!(sim_camera::Source::frame(&mut source, small, tick(0)).is_ok(), "the small camera's frame");
        let large = FrameSpec::new(1280, 720, 30).expect("large");
        assert!(sim_camera::Source::frame(&mut source, large, tick(1)).is_ok(), "the large camera's frame");
        done.send(()).expect("done");
        // 320x240 from 1080 wide: a third (360); 1280x720: the whole width.
        assert_eq!(server.join().expect("server"), ["shm", "fps 15", "hello 360", "fps 30", "hello 1080"]);
        drop(reader);
    }

    #[test]
    fn the_android_reader_posts_frames_and_reports_the_display_closing() {
        let fake = FakeDisplay::new("post");
        let listener = fake.listener.try_clone().expect("listener");
        let (close, closing) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            conn.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let got: Vec<String> = (0..3).filter_map(|_| lines.next()?.ok()).collect();
            frame(&mut conn, 2, 1, (4, 2), 1);
            // The device shuts down when the test says.
            let _ = closing.recv_timeout(Duration::from_secs(5));
            got
        });
        let screen = Arc::new(Screen::default());
        // No configured size: full size until the first frame says.
        let spec = FrameSpec::new(32, 32, 30).expect("spec");
        let reader =
            android::Reader::start(&fake.dir, "A", screen.clone(), android::Want { region: None, spec, lcd: None }).expect("start");
        let mut source = sim_camera::device::DeviceSource::new("A", None, screen.clone());
        let tick = |seq| sim_camera::Tick { seq, ..sim_camera::Tick::start() };
        let f = sim_camera::Source::frame(&mut source, spec, tick(0));
        assert_eq!(f.map(|f| f.pixel(0, 0)), Ok([7, 7, 7, 255]));
        close.send(()).expect("close");
        assert_eq!(server.join().expect("server"), ["shm", "fps 30", "hello 4294967295"]);
        let gone = (1..50).map(|seq| sim_camera::Source::frame(&mut source, spec, tick(seq))).find(Result::is_err);
        assert!(matches!(&gone, Some(Err(Error::Missing(m))) if m.contains("A's display closed")), "{gone:?}");
        drop(reader);
    }

    #[test]
    fn a_running_devices_screen_size_is_read_off_one_small_frame() {
        let fake = FakeDisplay::new("size");
        let listener = fake.listener.try_clone().expect("listener");
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let hello = lines.next().and_then(Result::ok);
            frame(&mut conn, 32, 71, (1080, 2400), 1);
            hello
        });
        assert_eq!(android::screen_size(&fake.dir), Some((1080, 2400)));
        assert_eq!(server.join().expect("server").as_deref(), Some("hello 32"), "a small frame, not the screen");
        assert_eq!(android::screen_size(std::path::Path::new("/nonexistent-avd")), None, "not running");
    }
}
