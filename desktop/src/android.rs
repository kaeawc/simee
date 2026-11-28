//! A live Android device: frames from its display socket (`<avd>/simee/display.sock`) stream into
//! a [`Pixels`], which wakes only the window showing it; touches and key taps go back as stream
//! lines. Frames come through the emulator's shared frame ring (only a 32-byte notice per frame
//! crosses the socket; the pixels are read from the ring straight into the buffer handed to
//! `Pixels`), or on the socket from an emulator without one. The emulator runs in its own process,
//! so a crashed guest can't take the window down.
//! While the window is hidden the frame stream pauses: the emulator is told to stop sending
//! (`hello 0`, so it stops scaling frames for us too) and the frame thread reads nothing; shown
//! again, it asks for frames and the emulator sends its newest at once.
//! Switched away from, a screen pauses the same way and keeps its last frame in its `Pixels`;
//! resumed, the window shows that frame at once and the emulator's newest replaces it.
//! A turned device's frames arrive as the panel lies, with how far to turn them (the stream's
//! `rotates`): the view turns them as it draws, on the GPU, so no frame is turned on the CPU,
//! here or in the emulator. Touches go back in the upright screen's pixels, which emu maps onto
//! the panel.
//! What the socket doesn't carry (typing with shift, rotation, the hinge, screenshots) goes over
//! the console through `sim-android`, on the screen's console worker: a console session can take
//! seconds on a busy emulator, and the window never waits for one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use rune::Pixels;
use sim_core::gesture::Phase;
use sim_core::hid::{KeyStroke, linux_keycode};
use sim_core::rotation::Rotation;
use sim_core::stream::Input;

use crate::work::{Outcome, Worker};

/// How the console worker finds its device: looked up again for every job (on the worker's
/// thread), so an AVD started or restarted elsewhere since the screen connected is the one the
/// job drives, never a copy that still says "not running".
pub type Lookup = Box<dyn Fn() -> Result<sim_android::Device, String> + Send>;

/// The lookup of AVD `udid`: an exact name loads just that AVD and the running-instance files.
pub fn lookup(backend: sim_android::Android, udid: String) -> Lookup {
    Box::new(move || backend.device(&udid))
}

/// Strokes queued for the console at most; more typed while a stalled emulator holds the queue
/// are dropped.
pub const STROKE_LIMIT: usize = 256;

/// The strokes of the newest console stroke job, until the worker starts it (`None` then).
type QueuedStrokes = Arc<Mutex<Option<Vec<KeyStroke>>>>;

pub struct Screen {
    pub udid: String,
    pub avd_dir: PathBuf,
    pub pixels: Pixels,
    link: Arc<Mutex<sim_android::display::Display>>,
    device: Arc<(AtomicU32, AtomicU32)>,
    /// Quarter turns clockwise that show the latest frame upright.
    turn: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
    /// None while paused: dropping the worker drops its queued jobs, and the one running posts
    /// nothing, so a device that isn't shown never reports.
    console: Option<Worker<Lookup>>,
    /// The last job queued on `console`, when it is a stroke job: strokes typed behind it join
    /// it (one console session) instead of queueing a session each.
    tail: Mutex<Option<QueuedStrokes>>,
    max_width: u32,
    shown: Arc<Shown>,
    /// Frames read so far.
    read: Arc<AtomicU64>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The display socket's `key` lines for `strokes`, when it can carry all of them: it taps one key
/// at a time, so a stroke with shift (or a key without a Linux keycode) needs the console.
pub fn display_keys(strokes: &[KeyStroke]) -> Option<Vec<String>> {
    strokes.iter().map(|s| if s.shift { None } else { linux_keycode(s.usage).map(|c| c.to_string()) }).collect()
}

/// Whether the window shows the screen; the frame thread waits on it while it doesn't.
struct Shown {
    visible: Mutex<bool>,
    changed: Condvar,
}

impl Shown {
    fn get(&self) -> bool {
        *lock(&self.visible)
    }

    /// Whether it changed.
    fn set(&self, visible: bool) -> bool {
        let mut v = lock(&self.visible);
        let changed = std::mem::replace(&mut *v, visible) != visible;
        self.changed.notify_all();
        changed
    }

    /// Waits while hidden; false once `alive` says the screen is gone.
    fn wait(&self, alive: &AtomicBool) -> bool {
        let mut v = lock(&self.visible);
        while !*v && alive.load(Ordering::Relaxed) {
            v = self.changed.wait(v).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        alive.load(Ordering::Relaxed)
    }
}

/// The device screen's height in the window, in points: the window is fitted to it.
pub const SCREEN_POINTS: f32 = 760.0;

/// Pixels the device screen's height covers on a Retina display (2 per point).
const SCREEN_PIXELS: u32 = 1520;

/// The widest frame to ask emu for, for a device of `size` pixels. emu shrinks frames by a whole
/// factor (`ceil(width / max_width)`), so this asks for the largest factor that still leaves at
/// least the pixels the view shows on the device's shorter side, whichever way it's turned, and
/// never shrinks a device that's smaller than that. A fixed 1080 shrank a 2560-wide tablet
/// threefold to 853 px, and the window stretched it back to about 2400.
pub fn stream_max_width((w, h): (u32, u32)) -> u32 {
    let factor = (w.min(h) / SCREEN_PIXELS).max(1);
    w.max(h).max(1).div_ceil(factor)
}

impl Screen {
    /// Connects and asks for frames at most `max_width` pixels wide. Console work for the
    /// device goes to `console`.
    pub fn connect(udid: &str, avd_dir: PathBuf, max_width: u32, console: Worker<Lookup>) -> Result<Screen, String> {
        let mut display = sim_android::display::Display::connect(&avd_dir)?;
        display.request_shared()?;
        display.request_rotation()?;
        display.send(&Input::Hello { max_width })?;
        let mut frames = sim_android::display::Frames::new(display.try_clone()?).with_fallback(display.try_clone()?);
        let (pixels, device, alive) = (Pixels::new(), Arc::new((AtomicU32::new(0), AtomicU32::new(0))), Arc::new(AtomicBool::new(true)));
        let shown = Arc::new(Shown { visible: Mutex::new(true), changed: Condvar::new() });
        let read = Arc::new(AtomicU64::new(0));
        let turn = Arc::new(AtomicU32::new(0));
        let (p, d, a, sh, r, t) = (pixels.clone(), device.clone(), alive.clone(), shown.clone(), read.clone(), turn.clone());
        std::thread::Builder::new()
            .name("simee-android-frames".into())
            .spawn(move || {
                // Only the newest announced frame (the window shows nothing older), and none
                // while the window is hidden: one announced just before it hid is passed over.
                // Each frame is read into the buffer the previous one came back in, so a steady
                // stream allocates nothing per frame (`spare` is empty only for the first frame,
                // or while Rune is still copying the previous one out).
                let mut spare = Vec::new();
                while sh.wait(&a)
                    && let Ok(header) = frames.newest_frame_into(&mut spare, || sh.get())
                {
                    r.fetch_add(1, Ordering::Relaxed);
                    d.0.store(header.device_width, Ordering::Relaxed);
                    d.1.store(header.device_height, Ordering::Relaxed);
                    t.store(frames.rotation.quarters(), Ordering::Relaxed);
                    spare = p.replace(header.width, header.height, std::mem::take(&mut spare));
                }
                a.store(false, Ordering::Relaxed);
            })
            .map_err(|e| e.to_string())?;
        let tail = Mutex::new(None);
        let link = Arc::new(Mutex::new(display));
        Ok(Screen {
            udid: udid.to_owned(),
            avd_dir,
            pixels,
            link,
            device,
            turn,
            alive,
            console: Some(console),
            tail,
            max_width,
            shown,
            read,
        })
    }

    /// Switched away from: stops the stream as a hidden window does and drops the console work
    /// still queued. The last frame stays in `pixels`, the connection and frame thread stay up.
    pub fn pause(&mut self) {
        self.set_visible(false);
        self.console = None;
        *lock(&self.tail) = None;
    }

    /// Shown again: console work goes to `console`, and while the window is `visible` the
    /// emulator sends its newest frame at once (the last one shows until it lands). Something is
    /// sent either way (`hello 0` while hidden keeps the stream paused): the write is what finds
    /// an emulator that went away while this screen was paused.
    pub fn resume(&mut self, console: Worker<Lookup>, visible: bool) {
        self.console = Some(console);
        self.shown.set(visible);
        self.send(&Input::Hello { max_width: if visible { self.max_width } else { 0 } });
    }

    /// Moved to another window: console work from now on goes to `console`, whose outbox is that
    /// window's. Work still queued on the old worker is dropped, as a pause drops it.
    pub fn set_console(&mut self, console: Worker<Lookup>) {
        self.console = Some(console);
        *lock(&self.tail) = None;
    }

    /// Pauses the frame stream while the window is hidden (minimised, or covered where the
    /// system says so) and resumes it with the newest frame when it shows again.
    pub fn set_visible(&self, visible: bool) {
        if self.shown.set(visible) {
            self.send(&Input::Hello { max_width: if visible { self.max_width } else { 0 } });
        }
    }

    /// Frames read from the emulator so far.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn frames_read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }

    /// Runs a console job for the device (looked up when the job starts), after the ones already
    /// queued. `false` when the queue is full (a stalled emulator) and the job was dropped.
    pub fn console(&self, job: impl FnOnce(&sim_android::Device) -> Outcome + Send + 'static) -> bool {
        let mut tail = lock(&self.tail);
        // Strokes typed after this job go behind it, not into a stroke job ahead of it.
        *tail = None;
        self.console.as_ref().is_some_and(|c| c.run(move |find: &Lookup| job(&find()?)))
    }

    /// Types key strokes: as display socket taps when it can carry them and no console work is
    /// queued ahead (which would otherwise land after them), else on the console worker, joined
    /// to the stroke job queued last if it hasn't started (key repeat on a stalled emulator is
    /// one queued session, not one per key).
    pub fn strokes(&self, strokes: Vec<KeyStroke>) {
        let mut tail = lock(&self.tail);
        let Some(console) = &self.console else { return };
        if let Some(keys) = display_keys(&strokes)
            && console.idle()
        {
            drop(tail);
            keys.into_iter().for_each(|k| self.send(&Input::Key(k)));
            return;
        }
        if let Some(queued) = tail.as_ref()
            && let Some(waiting) = lock(queued).as_mut()
        {
            let room = STROKE_LIMIT.saturating_sub(waiting.len());
            waiting.extend(strokes.into_iter().take(room));
            return;
        }
        let mut strokes = strokes;
        strokes.truncate(STROKE_LIMIT);
        let queued: QueuedStrokes = Arc::new(Mutex::new(Some(strokes)));
        let job = queued.clone();
        if console.run(move |find: &Lookup| {
            // Taken (and the lock let go) before the session: typing joins a later job meanwhile.
            let strokes = lock(&job).take();
            match strokes {
                Some(strokes) => find()?.strokes(&strokes).map(|()| None),
                None => Ok(None),
            }
        }) {
            *tail = Some(queued);
        }
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// The device's display size as it's shown, turned as the guest rotated it (input
    /// coordinates); `None` until the first frame.
    pub fn device_size(&self) -> Option<(u32, u32)> {
        let (w, h) = (self.device.0.load(Ordering::Relaxed), self.device.1.load(Ordering::Relaxed));
        (w > 0 && h > 0).then(|| self.rotation().turned((w, h)))
    }

    /// How far to turn [`Screen::pixels`] (as the panel lies) clockwise to show it upright.
    pub fn rotation(&self) -> Rotation {
        Rotation::from_quarters(self.turn.load(Ordering::Relaxed))
    }

    fn send(&self, input: &Input) {
        let mut link = self.link.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if link.send(input).is_err() {
            self.alive.store(false, Ordering::Relaxed);
            // A frame thread paused for a hidden window finds out too.
            self.shown.changed.notify_all();
        }
    }

    /// A touch at device pixels.
    pub fn touch(&self, x: f32, y: f32, phase: Phase) {
        self.send(&Input::Touch { x, y, phase });
    }

    /// Presses and releases a key by console name or Linux keycode.
    pub fn key(&self, name: &str) {
        self.send(&Input::Key(name.to_owned()));
    }

    /// Turns the rotary input.
    pub fn wheel(&self, ticks: i32) {
        self.send(&Input::Wheel(ticks));
    }
}

impl Drop for Screen {
    /// Ends the frame thread: it holds its own handle on the connection, so it would otherwise
    /// keep reading (or, paused, keep waiting) after the screen is gone.
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        self.shown.changed.notify_all();
        let link = self.link.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(s) = link.try_clone() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::FileExt;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    use sim_core::stream::{FrameHeader, Record, ring};

    #[test]
    fn streams_are_asked_for_at_least_the_pixels_the_view_shows() {
        // emu's factor for a frame `fw` wide under `max`, as `downscale_factor` computes it.
        let factor = |fw: u32, max: u32| sim_core::pixels::downscale_factor(fw, max);
        // Pixel Tablet, Pixel 4 XL, a medium phone, a 4K TV, a round watch: full size.
        for size in [(2560, 1600), (1440, 3040), (1080, 2400), (3840, 2160), (454, 454)] {
            let max = super::stream_max_width(size);
            assert_eq!((factor(size.0, max), factor(size.1, max)), (1, 1), "{size:?} asks for {max}");
        }
        // Only a screen at least twice the view on its shorter side is shrunk, and never below it.
        let max = super::stream_max_width((6400, 3600));
        assert_eq!(factor(6400, max), 2);
        assert!(3600 / factor(3600, max) >= super::SCREEN_PIXELS);
    }

    use super::*;
    use crate::work::Outbox;

    /// A console worker for an AVD that isn't running: its jobs fail at once.
    fn console(outbox: Outbox) -> Worker<Lookup> {
        counted_console(outbox, Arc::default())
    }

    /// The same, counting the lookups (one per job that reaches the device).
    fn counted_console(outbox: Outbox, lookups: Arc<std::sync::atomic::AtomicUsize>) -> Worker<Lookup> {
        let find: Lookup = Box::new(move || {
            lookups.fetch_add(1, Ordering::Relaxed);
            let info = sim_core::device::DeviceInfo {
                udid: "pixel".into(),
                name: "pixel".into(),
                state: sim_core::device::DeviceState::Booted,
                runtime_id: String::new(),
                device_type_id: "custom".into(),
                available: true,
                platform: sim_core::platform::Platform::Android,
                form_factor: None,
                sub_platform: None,
            };
            Ok(sim_android::Device { info, avd: None, running: None })
        });
        Worker::start("test-console", find, outbox)
    }

    fn stroke(c: char) -> KeyStroke {
        sim_core::hid::keystroke(c).expect("US key")
    }

    /// A display socket that accepts one client and reads whatever it sends until the test ends.
    fn quiet_display(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp").join(format!("rs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            BufReader::new(conn).lines().map_while(Result::ok).for_each(drop);
        });
        dir
    }

    fn wait_idle(screen: &Screen) {
        let start = Instant::now();
        while !screen.console.as_ref().is_none_or(Worker::idle) {
            assert!(start.elapsed() < Duration::from_secs(5), "console idle");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Key repeat on a stalled emulator: the strokes typed while a console session holds the
    /// queue join one queued session, in order, instead of one session each; a console action
    /// typed between them keeps its place.
    #[test]
    fn strokes_behind_a_stalled_console_join_one_session() {
        let dir = quiet_display("coalesce");
        let (outbox, lookups) = (Outbox::default(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let screen = Screen::connect("pixel", dir.clone(), 1080, counted_console(outbox.clone(), lookups.clone())).expect("connect");
        let (release, gate) = std::sync::mpsc::channel::<()>();
        assert!(screen.console(move |_| {
            gate.recv().ok();
            Ok(None)
        }));
        for c in "ABCDEFGHIJ".repeat(10).chars() {
            screen.strokes(vec![stroke(c)]);
        }
        assert!(screen.console(|_| Ok(Some("rotated".into()))));
        screen.strokes(vec![stroke('Z')]);
        screen.strokes(vec![stroke('z')]);
        release.send(()).expect("release");
        wait_idle(&screen);
        // The gate, the 100 strokes, the action, then 'Z' and 'z': four sessions, not 103.
        assert_eq!(lookups.load(Ordering::Relaxed), 4);
        let lines = outbox.take();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("isn't running") && lines[1] == "rotated" && lines[2].contains("isn't running"), "{lines:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The console worker looks the device up for every job, so an AVD started elsewhere after
    /// the screen connected (while the window held a "not running" copy) gets the next action.
    #[test]
    fn console_jobs_look_the_device_up_when_they_run() {
        // Not `rs-lookup-<pid>`: that is `quiet_display("lookup")`'s directory where the temp dir is
        // /tmp (Linux), and it removes it.
        let root = std::env::temp_dir().join(format!("rs-lookup-avds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (avds, running) = (root.join("avd"), root.join("running"));
        std::fs::create_dir_all(&avds).expect("avd home");
        std::fs::create_dir_all(&running).expect("running dir");
        std::fs::write(avds.join("pixel.ini"), "").expect("ini");
        let mut backend = sim_android::Android::from_env();
        (backend.env.avd_home, backend.env.sdk_root, backend.running_dir) = (avds, None, Some(running.clone()));
        let dir = quiet_display("lookup");
        let outbox = Outbox::default();
        let worker = Worker::start("test-lookup", lookup(backend, "pixel".into()), outbox.clone());
        let screen = Screen::connect("pixel", dir.clone(), 1080, worker).expect("connect");
        let probe = |d: &sim_android::Device| Ok(Some(format!("running: {}", d.running.is_some())));
        assert!(screen.console(probe));
        wait_idle(&screen);
        // The emulator advertises itself (as this process, which is alive).
        std::fs::write(running.join(format!("pid_{}.ini", std::process::id())), "avd.name=pixel\nport.serial=5554\n").expect("advertise");
        assert!(screen.console(probe));
        wait_idle(&screen);
        assert_eq!(outbox.take(), ["running: false", "running: true"]);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn plain_keys_go_over_the_display_socket_and_shifted_ones_need_the_console() {
        assert_eq!(display_keys(&[stroke('a'), stroke('b')]), Some(vec!["30".to_owned(), "48".to_owned()]));
        assert_eq!(display_keys(&[stroke('a'), stroke('B')]), None, "shift needs the console");
        assert_eq!(display_keys(&[]), Some(vec![]));
    }

    /// Typing never waits on the console: plain keys tap over the display connection at once,
    /// shifted ones queue on the console worker, and a plain key typed while console work is
    /// queued waits behind it, so keys arrive in the order they were typed.
    #[test]
    fn keystrokes_keep_their_order_across_the_socket_and_the_console() {
        let dir = PathBuf::from("/tmp").join(format!("rs-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn).lines();
            (0..6).map(|_| lines.next().expect("line").expect("utf8")).collect::<Vec<_>>()
        });
        let outbox = Outbox::default();
        let screen = Screen::connect("pixel", dir.clone(), 1080, console(outbox.clone())).expect("connect");
        screen.strokes(vec![stroke('a'), stroke('b')]);
        let (release, gate) = std::sync::mpsc::channel::<()>();
        screen.console(move |_| {
            gate.recv().ok();
            Ok(None)
        });
        // Behind the slow job: neither may reach the socket before it.
        screen.strokes(vec![stroke('c')]);
        screen.strokes(vec![stroke('D')]);
        release.send(()).expect("release");
        let start = Instant::now();
        while outbox.take().is_empty() {
            assert!(start.elapsed() < Duration::from_secs(5), "console jobs ran");
            std::thread::sleep(Duration::from_millis(2));
        }
        while !screen.console.as_ref().is_none_or(Worker::idle) {
            assert!(start.elapsed() < Duration::from_secs(5), "console idle");
            std::thread::sleep(Duration::from_millis(2));
        }
        screen.strokes(vec![stroke('e')]);
        assert_eq!(server.join().expect("server"), ["shm", "rotates", "hello 1080", "key 30", "key 48", "key 18"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A fake emulator announces a ring and a frame in it; the window's pixels get that frame,
    /// as the panel lies, and the screen says how to turn it (a quarter, here).
    #[test]
    fn frames_from_the_shared_ring_reach_the_window() {
        let dir = PathBuf::from("/tmp").join(format!("rs-android-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let ring_path = dir.join("simee/frames-1-0.ring");
        let rp = ring_path.clone();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut lines = BufReader::new(conn.try_clone().expect("clone")).lines();
            let asked: Vec<String> = (0..3).map(|_| lines.next().expect("line").expect("utf8")).collect();
            // A 2x1 frame of a 4x2 device in slot 1 of a 3-slot ring.
            let layout = ring::Layout::new(3, 8);
            let file = std::fs::File::create(&rp).expect("ring");
            file.set_len(layout.file_len()).expect("len");
            file.write_all_at(&layout.encode(), 0).expect("header");
            let slot = ring::Slot { seq: 2, frame: 7, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
            file.write_all_at(&slot.encode(), layout.slot_header_offset(1)).expect("slot");
            file.write_all_at(&[1, 2, 3, 4, 5, 6, 7, 8], layout.slot_data_offset(1)).expect("pixels");
            let path = rp.to_str().expect("utf8").as_bytes();
            let mut msg = Record::Ring { path_len: path.len() as u32 }.encode().to_vec();
            msg.extend(path);
            msg.extend(Record::Rotation(Rotation::Deg90).encode());
            msg.extend(Record::Shared { header: FrameHeader::new(2, 1, 4, 2, 7).expect("size"), slot: 1 }.encode());
            conn.write_all(&msg).expect("send");
            // Hold the connection open until the test has looked.
            let _ = finished.recv();
            asked
        });
        let screen = Screen::connect("pixel", dir.clone(), 1080, console(Outbox::default())).expect("connect");
        let start = Instant::now();
        while screen.pixels.size() != (2, 1) {
            assert!(start.elapsed() < Duration::from_secs(5), "no frame");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(screen.pixels.rgba(), [1, 2, 3, 4, 5, 6, 7, 8], "as the panel lies");
        assert_eq!((screen.device_size(), screen.rotation()), (Some((2, 4)), Rotation::Deg90), "the upright screen's size");
        assert!(screen.alive());
        done.send(()).expect("done");
        assert_eq!(server.join().expect("server"), ["shm", "rotates", "hello 1080"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// #17: each frame is read into the buffer the frame before last was in (Rune's
    /// `Pixels::replace` hands it back), so a steady stream alternates two buffers and allocates
    /// none after its second frame. Between frames the test holds a same-size allocation, so a
    /// path that frees each frame's buffer and allocates the next can't pass by being handed the
    /// freed address back.
    #[test]
    fn a_steady_stream_alternates_two_frame_buffers() {
        let dir = PathBuf::from("/tmp").join(format!("rs-recycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let (w, h) = (64u32, 64u32);
        let len = (w * h * 4) as usize;
        let (next, go) = std::sync::mpsc::channel::<u32>();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            for n in go {
                let mut msg = Record::Frame(FrameHeader::new(w, h, w, h, u64::from(n)).expect("size")).encode().to_vec();
                msg.extend(n.to_le_bytes().repeat(len / 4));
                conn.write_all(&msg).expect("send");
            }
        });
        let screen = Screen::connect("pixel", dir.clone(), 1080, console(Outbox::default())).expect("connect");
        let (mut at, mut soak) = (Vec::new(), Vec::new());
        for n in 1..=8u32 {
            next.send(n).expect("send");
            wait_until("the frame", || screen.pixels.size() == (w, h) && shown(&screen) == u64::from(n));
            at.push(screen.pixels.with_rgba(|_, _, px| px.as_ptr() as usize).expect("a frame"));
            soak.push(Vec::<u8>::with_capacity(len));
        }
        assert_ne!(at[0], at[1], "two buffers");
        for k in 2..at.len() {
            assert_eq!(at[k], at[k - 2], "frame {} refilled frame {}'s buffer: {at:x?}", k + 1, k - 1);
        }
        assert_eq!(screen.frames_read(), 8);
        drop((next, screen, soak));
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What the fake emulator saw: the client's lines, frames it announced while asked for none
    /// (only the one already on its way when the window hid), and the newest frame it made.
    #[derive(Default)]
    struct Seen {
        lines: Mutex<Vec<String>>,
        sent_paused: AtomicU64,
        newest: AtomicU64,
    }

    /// A fake emulator that streams like `emu`'s display server: the guest makes a frame every
    /// 4 ms, written into a two-slot ring and announced while the client's last `hello` asks for
    /// frames (a `hello 0` stops it); one frame is announced right after a `hello 0`, as if it
    /// was already on its way. A frame's pixels are its number.
    fn fake_emulator(listener: UnixListener, ring_path: PathBuf, seen: Arc<Seen>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let (max_width, in_flight, gone) =
                (Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (mw, fl, g, s) = (max_width.clone(), in_flight.clone(), gone.clone(), seen.clone());
            let reader = BufReader::new(conn.try_clone().expect("clone"));
            std::thread::spawn(move || {
                for line in reader.lines().map_while(Result::ok) {
                    if let Some(w) = line.strip_prefix("hello ").and_then(|w| w.parse().ok()) {
                        mw.store(w, Ordering::Relaxed);
                        fl.store(w == 0, Ordering::Relaxed);
                    }
                    s.lines.lock().expect("lines").push(line);
                }
                g.store(true, Ordering::Relaxed);
            });
            let layout = ring::Layout::new(2, 8);
            let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&ring_path).expect("ring");
            file.set_len(layout.file_len()).expect("len");
            file.write_all_at(&layout.encode(), 0).expect("header");
            let mut announced = false;
            let mut frame = 0u64;
            while !gone.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(4));
                frame += 1;
                seen.newest.store(frame, Ordering::Relaxed);
                let asked = max_width.load(Ordering::Relaxed) > 0;
                let on_its_way = in_flight.swap(false, Ordering::Relaxed);
                if !asked && !on_its_way {
                    continue;
                }
                let n = (frame % 2) as u32;
                let slot = ring::Slot { seq: 2 * frame, frame, width: 2, height: 1, device_width: 4, device_height: 2, len: 8 };
                file.write_all_at(&slot.encode(), layout.slot_header_offset(n)).expect("slot");
                file.write_all_at(&[(frame as u32).to_le_bytes(); 2].concat(), layout.slot_data_offset(n)).expect("pixels");
                let mut msg = Vec::new();
                if !announced {
                    let path = ring_path.to_str().expect("utf8").as_bytes();
                    msg.extend(Record::Ring { path_len: path.len() as u32 }.encode());
                    msg.extend(path);
                    announced = true;
                }
                msg.extend(Record::Shared { header: FrameHeader::new(2, 1, 4, 2, frame).expect("size"), slot: n }.encode());
                if conn.write_all(&msg).is_err() {
                    break;
                }
                if !asked {
                    seen.sent_paused.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let start = Instant::now();
        while !f() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The frame number the window's pixels show.
    fn shown(screen: &Screen) -> u64 {
        let rgba = screen.pixels.rgba();
        u64::from(u32::from_le_bytes(rgba[..4].try_into().expect("a frame")))
    }

    /// #21: a hidden window reads no frames, and the emulator stops sending them; shown again,
    /// the newest frame arrives at once.
    #[test]
    fn a_hidden_window_reads_no_frames_and_catches_up_when_shown() {
        let dir = PathBuf::from("/tmp").join(format!("rs-hidden-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let seen = Arc::new(Seen::default());
        let server = fake_emulator(listener, dir.join("simee/frames-1-0.ring"), seen.clone());
        let screen = Screen::connect("pixel", dir.clone(), 1080, console(Outbox::default())).expect("connect");
        wait_until("frames while shown", || screen.frames_read() >= 3);

        screen.set_visible(false);
        screen.set_visible(false); // no change: nothing sent
        wait_until("the emulator to hear the window hid", || seen.lines.lock().expect("lines").len() == 4);
        // The frame that was on its way goes out (and is passed over).
        wait_until("the frame on its way", || seen.sent_paused.load(Ordering::Relaxed) == 1);
        let (read, newest) = (screen.frames_read(), seen.newest.load(Ordering::Relaxed));
        // Counted in the guest's frames, not wall time: a busy CI runner oversleeps.
        wait_until("the guest to make frames while hidden", || seen.newest.load(Ordering::Relaxed) - newest >= 20);
        assert_eq!(screen.frames_read(), read, "no frame read while hidden");
        assert_eq!(seen.sent_paused.load(Ordering::Relaxed), 1, "only the frame already on its way was announced");
        let last_shown = shown(&screen);

        let before = seen.newest.load(Ordering::Relaxed);
        let start = Instant::now();
        screen.set_visible(true);
        wait_until("a frame after showing", || screen.frames_read() > read);
        println!("shown again: newest frame in {:.1} ms (the fake guest makes one every 4 ms)", start.elapsed().as_secs_f64() * 1000.0);
        wait_until("the newest frame", || shown(&screen) > before);
        assert!(shown(&screen) > last_shown);
        assert_eq!(*seen.lines.lock().expect("lines"), ["shm", "rotates", "hello 1080", "hello 0", "hello 1080"]);

        drop(screen);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(dir);
    }
}
