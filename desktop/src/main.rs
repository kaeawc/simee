//! simee desktop: one window onto iOS simulator and Android emulator devices, rendered with Rune.
//!
//! Pick a device on the left (grouped by platform, each labelled with its sub-platform); boot it if
//! needed. iOS screens stream from the device framebuffer without copying; Android screens stream
//! from the emulator's display socket. Clicks and drags become touches, typing goes to the device
//! keyboard, and the toolbar has the controls the device's form factor has (Back/Home/Recents, a
//! foldable's hinge, a watch's rotary input, a TV's D-pad). ⌘⇧H is Home, ⌘L Lock, ⌘S saves a
//! screenshot to the Desktop, ⌘3 switches the 3D device view on and off ([`scene`]),
//! ⌘⇧O pops the device out into a window of its own and back ([`popout`]).
//!
//! Handlers only queue [`Action`]s and [`Input`]s; `poll` performs them. That keeps handlers pure,
//! so the headless tests drive the real view without a device. Rune runs `poll` at the start of a
//! frame and again after that frame's handlers, so what a handler queues is performed in the same
//! frame without asking for a wake.
//!
//! Switching away from an Android device pauses its stream and keeps its screen (and last frame)
//! for a while; switching back shows that frame at once and resumes the stream.
//!
//! `cargo run` opens the window (`SIMEE_DEVICE=<udid|name|booted>` preselects a device);
//! `cargo run -- --snap shot.png --scene demo` renders headless (scenes: demo, android, foldable,
//! folded, tablet, ipad, watch, tv, apple-watch, apple-tv, auto, carplay, vision-pro, android-xr,
//! glasses, turned, turned-tablet, empty,
//! sidebar-hidden, inspector-hidden, panes-hidden, panes-wide, resources, resources-memory, admission,
//! admission-many, popped); a release build needs `--features tooling` for it.

mod admission;
mod android;
mod bezel;
mod controls;
mod debug;
mod device;
mod icons;
#[cfg(target_os = "macos")]
mod ios;
mod keys;
mod mark;
mod panes;
mod popout;
mod resources;
mod scan;
mod scene;
mod settings;
mod shot_preview;
mod stylus;
#[cfg(target_os = "macos")]
mod surface;
mod view;
mod work;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use rune::{AppSpec, NativeSurface, Pixels, Tx, Waker};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::form_factor::{FormFactor, Kind, Posture, Shape};
use sim_core::gesture::Phase;
use sim_core::hid::{Button, Direction};
use sim_core::platform::Platform;
use sim_core::rotation::Rotation;

use device::{Context, Live};
use keys::KeyAction;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Refresh,
    Select(String),
    /// Boot the selected device, past [`admission`] first.
    Boot,
    /// Boot these devices now: admitted, or the user chose to on the admission sheet.
    BootNow(Vec<String>),
    Shutdown,
    Button(Button),
    ToggleAppearance,
    Screenshot,
    /// Rotate 90° clockwise.
    Rotate,
    /// Android foldables: set the hinge.
    Fold(Posture),
    /// Watches: turn the rotary input (Wear OS) or the Digital Crown (Apple Watch).
    Rotary(i32),
    /// Android XR: head pose and recentering.
    Xr(sim_core::xr::XrCommand),
    /// Android tablets: the pointer becomes a stylus, or a finger again.
    ToggleStylus,
    /// iPhones on iOS 26 or earlier: the CarPlay display on (the API's `carplay`, then the view
    /// shows it in the iPhone's place) or off (the iPhone's screen again).
    ToggleCarPlay,
    /// ⌘3: the 3D device view on or off ([`scene`]).
    ToggleView3d,
    /// A change of the 3D view: face on, a saved view, flip, spin ([`scene::Command`]).
    View3d(scene::Command),
    Debug(debug::DebugAction),
    /// The device moves to a window of its own, and back ([`popout`]).
    PopOut(String),
    PopIn(String),
}

/// Device input, in device pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    /// The pointer over the screen with no button held (the stylus hovering).
    Hover(f32, f32),
    /// The pointer left the screen (the stylus out of range).
    Leave,
    /// One sample of a touch that came in from the screen's edge: a drag that started on the
    /// bezel ([`bezel`]).
    Edge(f32, f32, Phase, sim_core::gesture::ScreenEdge),
    Key(KeyAction),
}

pub struct State {
    pub devices: Vec<DeviceInfo>,
    pub selected: Option<String>,
    /// The synthetic screen of headless scenes.
    pub pixels: Pixels,
    /// The live iOS framebuffer, shown without copying.
    pub native: NativeSurface,
    /// The selected iOS device, when it's running.
    pub live: Option<Live>,
    /// The selected Android device, when it's running.
    pub screen: Option<android::Screen>,
    /// Android screens switched away from, oldest first: streams paused, last frames kept.
    paused: Vec<Paused>,
    /// The selected Android device's guest console, newest last.
    pub console_log: Vec<String>,
    /// The selected device's screen outline (round Wear OS watches).
    pub shape: Shape,
    /// A pressed hardware button and when to release it.
    held: Option<(Button, f64)>,
    /// What's running in the background ("Booting…", "Shutting down…").
    pub busy: Option<&'static str>,
    pub status: Option<String>,
    pub debug: debug::Debug,
    /// The unix socket this window serves the API on.
    pub socket: Option<String>,
    pub actions: Vec<Action>,
    pub inputs: Vec<Input>,
    /// No device service (headless scenes and tests).
    pub offline: bool,
    /// Headless scenes show the screen without a live device.
    pub offline_screen: bool,
    offline_size: (u32, u32),
    /// How far a headless scene's screen is turned (a live Android screen says itself).
    offline_turn: Rotation,
    /// A headless scene shows the iPhone's CarPlay display: the iPhone's screen size to go back
    /// to (a live device says itself, [`device::Shared::showing_carplay`]).
    offline_carplay: Option<(u32, u32)>,
    /// The UI thread's simulator service connection, made when an iOS device is first shown
    /// (the scanner has its own).
    ctx: Option<Result<Context, String>>,
    android_backend: Option<sim_android::Android>,
    /// Lists devices off the UI thread.
    scanner: Option<scan::Scanner>,
    /// The selected AVD as the last scan resolved it, and the UDID that scan was for.
    android: Option<sim_android::Device>,
    android_for: Option<String>,
    /// A refresh asked to resolve `selected` (`booted`, a name) against the next scan.
    resolve_pending: bool,
    /// The next scan should (re)connect the selected device.
    connect_pending: bool,
    /// Status lines from background work.
    outbox: work::Outbox,
    waker: Option<Waker>,
    work_done: Arc<AtomicBool>,
    work_error: Arc<Mutex<Option<String>>>,
    sized_for: Option<(u32, u32)>,
    /// Whether the window shows (not minimised, hidden, or covered where the system says so).
    visible: bool,
    /// Android tablets: the pointer is a stylus (drags draw, moving over the screen hovers).
    pub stylus: bool,
    /// The stylus's own line to the device while it's on (one console session, samples
    /// coalesced; see [`stylus::Pen`]).
    pen: Option<stylus::Pen>,
    /// The screen element's size in points (pointer moves come in its own points).
    pub screen_view: (f32, f32),
    /// Where the screen and its bezel are, and the edge touch in progress.
    pub bezel: bezel::Bezel,
    /// The 3D device view's switch.
    pub view3d: scene::Toggle,
    /// The screenshot previews in the window's bottom-right corner ([`shot_preview`]).
    pub previews: shot_preview::Previews,
    /// Seconds the mark has been moving (a job running, a device starting or stopping), and the
    /// poll time it started at; `None` while it rests.
    pub mark_t: f32,
    mark_from: Option<f64>,
    /// The saved settings ([`settings`]) and the panes' own state ([`panes`]).
    pub prefs: settings::Store,
    pub panes: panes::Panes,
    /// The resource panel's samples and its line to the API ([`resources`]).
    pub resources: resources::Resources,
    /// Devices out in windows of their own ([`popout`]).
    pub popouts: popout::Popouts,
    /// The check before a boot and its warning sheet ([`admission`]).
    pub admission: admission::Gate,
}

/// An Android screen switched away from, with what the window showed it with.
struct Paused {
    screen: android::Screen,
    shape: Shape,
    size: (u32, u32),
}

/// Paused Android screens kept at most. Each holds its display connection, a parked frame thread
/// and its last frame (about 10 MB for a 1080x2400 phone); the oldest goes first.
const PAUSED_LIMIT: usize = 4;

impl State {
    pub fn selected_info(&self) -> Option<&DeviceInfo> {
        self.selected.as_ref().and_then(|u| self.devices.iter().find(|d| &d.udid == u))
    }

    /// The device screen in pixels; the device's configured size (or a phone) until the first frame.
    pub fn screen_size(&self) -> (u32, u32) {
        let live = self.live.as_ref().and_then(|l| l.shared.size());
        live.or_else(|| self.screen.as_ref().and_then(android::Screen::device_size)).unwrap_or(self.offline_size)
    }

    /// How far to turn the screen's pixels (as a turned Android device's panel lies) clockwise
    /// to show them upright.
    pub fn screen_rotation(&self) -> Rotation {
        self.screen.as_ref().map_or(self.offline_turn, android::Screen::rotation)
    }

    /// Whether a device screen is shown (live, or a headless scene's).
    pub fn showing_screen(&self) -> bool {
        self.live.is_some() || self.screen.is_some() || self.offline_screen
    }

    /// Whether the screen shown is the iPhone's CarPlay display: its size, touches and body
    /// ([`scene::body_for`]) are the car display's.
    pub fn carplay_shown(&self) -> bool {
        self.offline_carplay.is_some() || self.live.as_ref().is_some_and(|l| l.shared.showing_carplay.load(Ordering::Relaxed))
    }

    /// Asks the scanner for a scan now; `poll` applies it when it arrives.
    fn rescan(&self) {
        if let Some(sc) = &self.scanner {
            sc.ask(self.selected.as_deref());
        }
    }

    /// Applies a scan: the device list, the selected AVD and its console, and a (re)connect when
    /// the list changed or one is pending. Returns whether anything shown changed.
    fn apply_scan(&mut self, scan: scan::Scan) -> bool {
        let before: Vec<(String, DeviceState)> = self.devices.iter().map(|d| (d.udid.clone(), d.state)).collect();
        self.devices = scan.devices;
        let mut changed = self.devices.iter().map(|d| (d.udid.clone(), d.state)).ne(before);
        if let Some(e) = scan.error {
            self.status = Some(e);
        }
        if std::mem::take(&mut self.resolve_pending) {
            if self.selected.as_deref() == Some("booted") {
                self.selected = self.devices.iter().find(|d| d.state == DeviceState::Booted).map(|d| d.udid.clone());
            } else if let Some(q) = self.selected.clone()
                && !self.devices.iter().any(|d| d.udid == q)
            {
                self.selected = self.devices.iter().find(|d| d.name == q).map(|d| d.udid.clone());
            }
            changed = true;
        }
        if scan.selected.is_some() && scan.selected == self.selected {
            self.android_for = scan.selected;
            self.android = match scan.android {
                Some(Ok(d)) => Some(d),
                Some(Err(e)) => {
                    self.status = Some(e);
                    None
                }
                None => None,
            };
            if self.android.is_some() && scan.console_log != self.console_log {
                self.console_log = scan.console_log;
                changed = true;
            }
        }
        if self.screen.as_ref().is_some_and(|sc| !sc.alive()) {
            self.screen = None;
        }
        // A paused device that stopped (here or elsewhere) has nothing to come back to.
        let devices = &self.devices;
        self.paused.retain(|p| p.screen.alive() && devices.iter().any(|d| d.udid == p.screen.udid && d.state == DeviceState::Booted));
        if changed || self.connect_pending {
            self.connect();
        }
        changed
    }

    /// Pauses the Android screen shown, keeping it (and its last frame) for a switch back.
    fn pause_screen(&mut self) {
        let Some(mut screen) = self.screen.take() else { return };
        if !screen.alive() {
            return;
        }
        screen.pause();
        self.paused.retain(|p| p.screen.udid != screen.udid);
        self.paused.push(Paused { screen, shape: self.shape, size: self.offline_size });
        if self.paused.len() > PAUSED_LIMIT {
            self.paused.remove(0);
        }
    }

    /// Shows `udid`'s paused screen again: its last frame at once, live frames as the emulator
    /// sends them. False when there's none, or its emulator went away meanwhile.
    fn resume_screen(&mut self, udid: &str) -> bool {
        let Some(i) = self.paused.iter().position(|p| p.screen.udid == udid) else { return false };
        let mut p = self.paused.remove(i);
        // The previous device's log, until the rescan that follows brings this one's.
        self.console_log.clear();
        let backend = self.android_backend.clone().unwrap_or_default();
        let console = work::Worker::start("simee-android-console", android::lookup(backend, udid.to_owned()), self.outbox.clone());
        p.screen.resume(console, self.visible);
        if !p.screen.alive() {
            return false;
        }
        (self.shape, self.offline_size) = (p.shape, p.size);
        self.screen = Some(p.screen);
        self.status = None;
        true
    }

    fn connect(&mut self) {
        self.connect_pending = false;
        let Some(info) = self.selected_info().cloned() else { return };
        // Its own window has its connection.
        if self.popouts.is_open(&info.udid) {
            return;
        }
        if self.screen.as_ref().is_some_and(|sc| sc.udid != info.udid) {
            self.pause_screen();
        }
        self.shape = info.kind().map_or(Shape::Rounded, |k| k.profile().shape);
        match info.platform {
            Platform::Ios => {
                self.screen = None;
                if self.ctx.is_none() {
                    self.ctx = Some(Context::new().map_err(|e| e.to_string()));
                }
                let (Some(Ok(ctx)), Some(waker)) = (&self.ctx, self.waker.clone()) else { return };
                if self.live.as_ref().is_some_and(|l| l.udid == info.udid) {
                    return;
                }
                self.live = None;
                if info.state == DeviceState::Booted {
                    match Live::connect(ctx, &info.udid, self.native.clone(), waker) {
                        Ok(live) => {
                            self.debug.data_path = Some(live.data_path());
                            self.live = Some(live);
                            self.status = None;
                            self.debug.reload(&info.udid);
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
            }
            Platform::Android => {
                self.live = None;
                if self.screen.as_ref().is_some_and(|sc| sc.udid == info.udid && sc.alive()) {
                    return;
                }
                self.screen = None;
                if info.state == DeviceState::Booted && self.resume_screen(&info.udid) {
                    // Its log and running instance, without waiting for the periodic scan.
                    self.rescan();
                    return;
                }
                // The AVD is resolved on the scanner thread (it reads its config): wait for a
                // scan of this one, unless the last one already failed to resolve it.
                let Some(device) = self.android.clone().filter(|d| d.info.udid == info.udid) else {
                    if self.android_for.as_deref() != Some(info.udid.as_str()) {
                        self.connect_pending = true;
                        self.rescan();
                    }
                    return;
                };
                if let Ok(p) = device.profile() {
                    self.shape = p.shape;
                }
                if let Ok(size) = device.screen_size() {
                    self.offline_size = size;
                }
                let dir = device.avd.as_ref().map(|a| a.dir.clone());
                if info.state == DeviceState::Booted
                    && let Some(dir) = dir
                {
                    // The worker looks the AVD up for each job: this scan's copy goes stale
                    // when the AVD starts, stops or restarts elsewhere.
                    let backend = self.android_backend.clone().unwrap_or_default();
                    let find = android::lookup(backend, info.udid.clone());
                    let console = work::Worker::start("simee-android-console", find, self.outbox.clone());
                    match android::Screen::connect(&info.udid, dir, android::stream_max_width(self.offline_size), console) {
                        Ok(sc) => {
                            sc.set_visible(self.visible);
                            self.screen = Some(sc);
                            self.status = None;
                        }
                        Err(e) => self.status = Some(e),
                    }
                }
            }
        }
    }

    /// Runs `job` on a worker thread (booting and shutting down block for seconds); `poll`
    /// refreshes the list when it's done.
    fn run_async(&mut self, label: &'static str, job: impl FnOnce() -> Result<(), String> + Send + 'static) {
        let Some(waker) = self.waker.clone() else { return };
        self.busy = Some(label);
        self.work_done.store(false, Ordering::Relaxed);
        let (done, error) = (self.work_done.clone(), self.work_error.clone());
        std::thread::spawn(move || {
            if let Err(e) = job()
                && let Ok(mut slot) = error.lock()
            {
                *slot = Some(e);
            }
            done.store(true, Ordering::Relaxed);
            waker();
        });
    }
}

fn init() -> State {
    let (view3d, view3d_status) = scene::Toggle::from_process();
    State {
        devices: Vec::new(),
        selected: std::env::var("SIMEE_DEVICE").ok(),
        pixels: Pixels::new(),
        native: NativeSurface::new(),
        live: None,
        screen: None,
        paused: Vec::new(),
        console_log: Vec::new(),
        shape: Shape::Rounded,
        held: None,
        busy: None,
        mark_t: 0.0,
        mark_from: None,
        status: view3d_status,
        debug: debug::Debug::default(),
        socket: None,
        actions: vec![Action::Refresh],
        inputs: Vec::new(),
        offline: false,
        offline_screen: false,
        offline_size: (1206, 2622),
        offline_turn: Rotation::Deg0,
        offline_carplay: None,
        ctx: None,
        android_backend: None,
        scanner: None,
        android: None,
        android_for: None,
        resolve_pending: false,
        connect_pending: false,
        outbox: work::Outbox::default(),
        waker: None,
        work_done: Arc::new(AtomicBool::new(false)),
        work_error: Arc::new(Mutex::new(None)),
        sized_for: None,
        visible: true,
        stylus: false,
        pen: None,
        screen_view: (0.0, 0.0),
        bezel: bezel::Bezel::default(),
        view3d,
        previews: shot_preview::Previews::default(),
        prefs: settings::Store::default(),
        panes: panes::Panes::default(),
        resources: resources::Resources::default(),
        popouts: popout::Popouts::default(),
        admission: admission::Gate::default(),
    }
}

/// The window's state from its saved settings.
#[cfg_attr(feature = "hot", allow(dead_code))]
fn init_with(store: settings::Store) -> State {
    let mut s = init();
    panes::restore(&mut s, store);
    s
}

fn info(udid: &str, name: &str, device_type: &str, runtime: &str, state: DeviceState) -> DeviceInfo {
    let runtime_id = format!("com.apple.CoreSimulator.SimRuntime.{runtime}");
    let device_type_id = format!("com.apple.CoreSimulator.SimDeviceType.{device_type}");
    let kind = sim_core::form_factor::apple_kind(&device_type_id, &runtime_id);
    DeviceInfo {
        udid: udid.into(),
        name: name.into(),
        state,
        runtime_id,
        device_type_id,
        available: true,
        platform: Platform::Ios,
        form_factor: None,
        sub_platform: None,
    }
    .with_kind(kind)
}

fn android_info(udid: &str, form_factor: FormFactor, image: &str, state: DeviceState) -> DeviceInfo {
    DeviceInfo {
        udid: udid.into(),
        name: udid.into(),
        state,
        runtime_id: format!("system-images;{image};arm64-v8a"),
        device_type_id: "custom".into(),
        available: true,
        platform: Platform::Android,
        form_factor: None,
        sub_platform: None,
    }
    .with_kind(Kind::new(Platform::Android, form_factor).ok())
}

/// A synthetic screen of `w` x `h` for headless scenes.
fn gradient(s: &mut State, w: u32, h: u32) {
    s.offline_size = (w, h);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 200, 255]);
        }
    }
    s.pixels.set(w, h, rgba);
}

/// Fixed data and a synthetic screen for snapshots and tests.
fn demo(s: &mut State) {
    s.offline = true;
    s.offline_screen = true;
    s.actions.clear();
    s.devices = vec![
        info("A1", "iPhone 17", "iPhone-17", "iOS-26-5", DeviceState::Booted),
        info("B2", "iPhone 17 Pro Max", "iPhone-17-Pro-Max", "iOS-26-5", DeviceState::Shutdown),
        info("D4", "iPad Pro 11-inch (M4)", "iPad-Pro-11-inch-M4", "iOS-26-5", DeviceState::Shutdown),
        info("C3", "iPhone 18 Pro", "iPhone-18-Pro", "iOS-27-0", DeviceState::Shutdown),
        info("E5", "Apple Watch Series 10 (46mm)", "Apple-Watch-Series-10-46mm", "watchOS-26-0", DeviceState::Shutdown),
        info("F6", "Apple TV 4K (3rd generation)", "Apple-TV-4K-3rd-generation-4K", "tvOS-26-0", DeviceState::Shutdown),
        info("G7", "Apple Vision Pro", "Apple-Vision-Pro", "xrOS-26-0", DeviceState::Shutdown),
        android_info("pixel_9", FormFactor::Phone, "android-36;google_apis", DeviceState::Booted),
        android_info("pixel_fold", FormFactor::Foldable, "android-36;google_apis", DeviceState::Shutdown),
        android_info("pixel_tablet", FormFactor::Tablet, "android-36;google_apis", DeviceState::Shutdown),
        android_info("wear_round", FormFactor::Watch, "android-34;android-wear", DeviceState::Shutdown),
        android_info("google_tv", FormFactor::Tv, "android-34;google-tv", DeviceState::Shutdown),
        android_info("automotive", FormFactor::Auto, "android-34;android-automotive", DeviceState::Shutdown),
    ];
    s.selected = Some("A1".into());
    gradient(s, 402, 874);
    s.debug.overlays = vec![(2, true)];
    s.debug.text_size = 3;
    s.debug.processes = vec![
        sim_debug::process::Process {
            pid: 412,
            ppid: 1,
            cpu: 7.5,
            rss_kb: 612_000,
            elapsed: "10:00".into(),
            name: "SpringBoard".into(),
            path: "/x/SpringBoard".into(),
        },
        sim_debug::process::Process {
            pid: 980,
            ppid: 1,
            cpu: 1.2,
            rss_kb: 88_000,
            elapsed: "01:00".into(),
            name: "Preferences".into(),
            path: "/x/Preferences".into(),
        },
    ];
    s.debug.connections = vec![sim_debug::net::Connection {
        pid: 980,
        process: "nsurlsessiond".into(),
        protocol: "TCP".into(),
        local: "10.0.0.2:51000".into(),
        remote: "17.253.4.125:443".into(),
        state: "ESTABLISHED".into(),
    }];
    s.debug.traffic = vec![sim_debug::net::Traffic { pid: 980, process: "nsurlsessiond".into(), bytes_in: 1_400_000, bytes_out: 52_000 }];
    s.debug.containers = vec![sim_debug::storage::App {
        bundle_id: "com.example.playground".into(),
        data: Some("Containers/Data/Application/X".into()),
        bundle: None,
        data_bytes: 3 << 20,
        bundle_bytes: 0,
    }];
    s.debug.logs = vec!["2026-11-22 01:00:00.000 Df SpringBoard[412:1] launched".into()];
    s.console_log = vec![
        "[    0.000000] Booting Linux on physical CPU 0x0000000000".into(),
        "[    0.871529] init: init first stage started!".into(),
        "[    8.431130] init: Encryption policy of /data/incremental set".into(),
        "[   22.104210] init: starting service 'adbd'...".into(),
    ];
}

/// The demo data with one device selected and running.
fn select_running(s: &mut State, udid: &str, (w, h): (u32, u32), shape: Shape) {
    demo(s);
    for d in &mut s.devices {
        d.state = if d.udid == udid { DeviceState::Booted } else { DeviceState::Shutdown };
    }
    s.selected = Some(udid.into());
    s.shape = shape;
    gradient(s, w, h);
}

fn android_scene(s: &mut State) {
    select_running(s, "pixel_9", (360, 800), Shape::Rounded);
}

/// A phone turned a quarter clockwise: its frames lie as the panel does (360x800) and the view
/// turns them as it draws, so the screen shows 800x360, upright.
fn turned_scene(s: &mut State) {
    select_running(s, "pixel_9", (360, 800), Shape::Rounded);
    (s.offline_size, s.offline_turn) = ((800, 360), Rotation::Deg90);
}

/// A Pixel Tablet (landscape panel, at half size) turned a quarter clockwise: portrait.
fn turned_tablet_scene(s: &mut State) {
    select_running(s, "pixel_tablet", (1280, 800), Shape::Rounded);
    (s.offline_size, s.offline_turn) = ((800, 1280), Rotation::Deg90);
}

fn foldable_scene(s: &mut State) {
    select_running(s, "pixel_fold", (552, 460), Shape::Rectangle);
}

/// The Pixel Fold folded: emu streams only its outer screen (949x1840 of the 2208x1840 scanout,
/// here at a quarter), so the view takes that shape.
fn folded_scene(s: &mut State) {
    select_running(s, "pixel_fold", (237, 460), Shape::Rectangle);
}

/// A Pixel Tablet in landscape (its natural orientation), at half size.
fn tablet_scene(s: &mut State) {
    select_running(s, "pixel_tablet", (1280, 800), Shape::Rounded);
}

/// An 11-inch iPad Pro, portrait, at half size.
fn ipad_scene(s: &mut State) {
    select_running(s, "D4", (834, 1210), Shape::Rounded);
}

fn watch_scene(s: &mut State) {
    select_running(s, "wear_round", (454, 454), Shape::Round);
}

fn tv_scene(s: &mut State) {
    select_running(s, "google_tv", (960, 540), Shape::Rectangle);
}

fn apple_watch_scene(s: &mut State) {
    select_running(s, "E5", (416, 496), Shape::Rounded);
}

fn apple_tv_scene(s: &mut State) {
    select_running(s, "F6", (960, 540), Shape::Rectangle);
}

/// The iPhone 17 (iOS 26.5) with its CarPlay display on: the car display's 800x480 in the
/// iPhone's place. The 3D view draws the `CarPlay-800x480` head unit, flat while there is none.
fn carplay_scene(s: &mut State) {
    select_running(s, "A1", (402, 874), Shape::Rounded);
    show_offline_carplay(s, true);
}

/// A headless scene's CarPlay display on (the synthetic car display) or off (the iPhone's
/// screen it showed before).
fn show_offline_carplay(s: &mut State, on: bool) {
    match (on, s.offline_carplay) {
        (true, None) => {
            s.offline_carplay = Some(s.offline_size);
            gradient(s, sim_core::carplay::WIDTH, sim_core::carplay::HEIGHT);
        }
        (false, Some((w, h))) => {
            s.offline_carplay = None;
            gradient(s, w, h);
        }
        _ => {}
    }
}

/// The demo data with `device`, which the demo list doesn't have, added, selected and running.
fn select_added(s: &mut State, device: DeviceInfo, size: (u32, u32), shape: Shape) {
    select_running(s, &device.udid.clone(), size, shape);
    s.devices.push(DeviceInfo { state: DeviceState::Booted, ..device });
}

/// What a form factor's scene says with the 3D view on (`SIMEE_VIEW=3d`) while its form factor
/// has no body: what ⌘3 would, the screen staying flat.
fn say_if_no_body(s: &mut State) {
    if s.view3d.on && s.showing_screen() && scene::body_for(s).is_none() {
        s.status = Some(scene::NO_BODY.clone());
    }
}

/// An Android Automotive head unit (the reference 1024x768 screen).
fn auto_scene(s: &mut State) {
    select_running(s, "automotive", (1024, 768), Shape::Rectangle);
    say_if_no_body(s);
}

/// Apple Vision Pro, at half size.
fn vision_pro_scene(s: &mut State) {
    select_running(s, "G7", (1366, 1024), Shape::Rectangle);
    say_if_no_body(s);
}

/// An Android XR headset (the SDK's `xr_headset_device`), at half size.
fn android_xr_scene(s: &mut State) {
    let headset = android_info("xr_headset", FormFactor::Vision, "android-36;google-xr", DeviceState::Booted);
    select_added(s, headset, (1280, 1279), Shape::Rectangle);
    say_if_no_body(s);
}

/// Android XR AI glasses (the SDK's `ai_glasses_device`, 450x450).
fn glasses_scene(s: &mut State) {
    let glasses = android_info("ai_glasses", FormFactor::Vision, "android-36;ai-glasses", DeviceState::Booted);
    select_added(s, glasses, (450, 450), Shape::Rectangle);
    say_if_no_body(s);
}

fn empty(s: &mut State) {
    s.offline = true;
    s.actions.clear();
    s.selected = None;
}

fn headless() -> bool {
    std::env::args().any(|a| matches!(a.as_str(), "--snap" | "--tree" | "--script" | "--list-scenes" | "--video"))
}

fn setup(s: &mut State, waker: Waker) {
    s.native.set_waker(waker.clone());
    s.previews.connect(waker.clone());
    s.outbox = work::Outbox::new(Some(waker.clone()));
    s.bezel.set_outbox(s.outbox.clone());
    s.waker = Some(waker);
    if s.offline || headless() {
        s.offline = true;
        return;
    }
    s.android_backend = Some(sim_android::Android::from_env());
    // Devices are listed on the scanner thread, so the window paints before the simulator
    // service has answered. Android devices start and stop outside this window (the CLI,
    // AutoMobile, a crash): it rescans every two seconds, following the selected device's
    // guest console.
    if let Some(w) = s.waker.clone() {
        match scan::Scanner::start(Duration::from_secs(2), w) {
            Ok(sc) => s.scanner = Some(sc),
            Err(e) => s.status = Some(format!("device scanner: {e}")),
        }
    }
    if let Some(w) = s.waker.clone() {
        s.debug.start(w.clone());
        s.resources.connect(w.clone());
        s.admission.connect(w);
    }
    // The window also serves the API, so scripts can drive the device it shows.
    let socket = std::env::var_os("SIMEE_SOCKET").map_or_else(|| std::env::temp_dir().join("simee-desktop.sock"), std::path::PathBuf::from);
    match sim_api::serve_in_background(socket.clone()) {
        Ok(()) => s.socket = Some(socket.display().to_string()),
        Err(e) => s.status = Some(format!("API socket {}: {e}", socket.display())),
    }
}

/// The window went off screen or came back: a hidden window's Android screen reads no frames.
fn on_visibility(s: &mut State, visible: bool, _: &mut Tx) {
    s.visible = visible;
    if let Some(sc) = &s.screen {
        sc.set_visible(visible);
    }
}

/// Presses a button: on iOS now, releasing it on a later frame; on Android as one key tap.
fn press_button(s: &mut State, button: Button, t: f64, tx: &mut Tx) {
    if let Some(sc) = &s.screen {
        match button.android_key() {
            Some(key) => sc.key(key),
            None => s.status = Some(format!("the {} button doesn't exist on Android", button.as_str())),
        }
        return;
    }
    let Some(live) = &s.live else { return };
    if let Some((held, _)) = s.held.take() {
        live.button(held, Direction::Up);
    }
    live.button(button, Direction::Down);
    s.held = Some((button, t + 0.06));
    tx.wake_after(Duration::from_millis(60));
}

/// A foldable toolbar button, as stock's fold controls: fold shows the folded screen (a fold-in
/// shut, a dual-screen folded back), unfold lays it flat, half open stands it like a book. The
/// screen then streams at the folded screen's size, so the view resizes with it.
fn fold_to(d: &sim_android::Device, posture: Posture) -> Result<(), String> {
    match posture {
        Posture::Closed => d.fold_display(true),
        Posture::HalfOpened => d.posture(sim_core::fold::StockPosture::HalfOpen),
        Posture::Opened => d.fold_display(false),
    }
}

/// Folds an iPhone Duo through the API's `fold` (SpringBoard sweeps its hinge: a second or two),
/// off the UI thread; the capture thread then shows the screen in use.
fn apple_fold(s: &mut State, udid: String, posture: Posture) {
    // The busy slot stays with the job holding it until that returns: a fold sharing it would
    // clear it with the first job's completion, letting a second attach start beside the fold.
    if let Some(running) = s.busy {
        s.status = Some(format!("Wait for {running} to finish"));
        return;
    }
    s.run_async("Folding…", move || {
        let params = serde_json::json!({"device": udid, "posture": posture});
        sim_api::call_method(&mut sim_api::Session::default(), "fold", &params).map(drop)
    });
}

/// Queues a console action for the selected Android device on its console worker; failures go
/// to the status line when it finishes.
fn android_action(s: &State, f: impl FnOnce(&sim_android::Device) -> Result<(), String> + Send + 'static) {
    if let Some(sc) = &s.screen {
        sc.console(move |d| f(d).map(|()| None));
    }
}

/// Keeps only the newest of each run of pointer moves queued between two polls: a frame's worth
/// of moves is one sample (iOS drops moves under ~16 ms apart anyway, so sending them all could
/// lose the last one), and downs, ups and keys keep their order.
fn coalesce_moves(inputs: Vec<Input>) -> Vec<Input> {
    let mut out: Vec<Input> = Vec::with_capacity(inputs.len());
    for input in inputs {
        if let (Input::Move(..), Some(Input::Move(..)))
        | (Input::Hover(..), Some(Input::Hover(..)))
        | (Input::Edge(.., Phase::Move, _), Some(Input::Edge(.., Phase::Move, _))) = (&input, out.last())
        {
            out.pop();
        }
        out.push(input);
    }
    out
}

fn poll(s: &mut State, t: f64, tx: &mut Tx) -> bool {
    // A landed admission check queues its boot (or shows its sheet) before the actions run.
    let mut changed = admission::poll(s);
    if let Some((button, due)) = s.held
        && t >= due
    {
        s.held = None;
        s.live.iter().for_each(|l| l.button(button, Direction::Up));
    }
    for action in std::mem::take(&mut s.actions) {
        changed = true;
        if action == Action::ToggleView3d {
            toggle_view3d(s);
            continue;
        }
        if action == Action::ToggleCarPlay {
            toggle_carplay(s);
            continue;
        }
        if let Action::View3d(c) = action {
            s.view3d.view.back = scene::shows_back(s);
            s.view3d.view.command(c, t);
            continue;
        }
        if popout::perform(s, &action, tx) {
            continue;
        }
        if s.offline {
            if let Action::Select(udid) = action {
                s.selected = Some(udid);
            }
            continue;
        }
        match action {
            Action::Refresh => {
                s.resolve_pending = true;
                s.connect_pending = true;
                s.rescan();
            }
            Action::Select(udid) => {
                if s.selected.as_ref() != Some(&udid) {
                    // The scan of the new selection brings its log.
                    s.console_log.clear();
                    stylus_off(s);
                }
                s.selected = Some(udid);
                if let Some(sc) = &s.scanner {
                    sc.follow(s.selected.as_deref());
                }
                s.connect();
            }
            Action::Boot => {
                let Some(info) = s.selected_info().cloned() else { continue };
                let admitted = admission::ask(s, &info);
                boot(s, &admitted);
            }
            Action::BootNow(udids) => boot(s, &udids),
            Action::Shutdown => {
                if let Some(live) = s.live.take() {
                    if let Err(e) = live.shutdown() {
                        s.status = Some(e);
                    }
                    drop(live);
                    s.rescan();
                }
                if let Some(sc) = s.screen.take()
                    && let Some(backend) = s.android_backend.clone()
                {
                    s.run_async("Shutting down…", move || backend.device(&sc.udid)?.shutdown());
                }
                s.sized_for = None;
            }
            Action::Button(b) => press_button(s, b, t, tx),
            Action::ToggleAppearance => s.live.iter_mut().for_each(Live::toggle_appearance),
            Action::Screenshot => {
                if let Some(msg) = screenshot(s) {
                    s.status = Some(msg);
                }
            }
            Action::Rotate => match s.live.as_mut() {
                Some(live) => live.rotate(),
                None => android_action(s, sim_android::Device::rotate),
            },
            Action::Fold(posture) => match s.live.as_ref().map(|l| l.udid.clone()) {
                Some(udid) => apple_fold(s, udid, posture),
                None => android_action(s, move |d| fold_to(d, posture)),
            },
            Action::Rotary(ticks) => {
                s.live.iter().for_each(|l| l.crown(ticks));
                s.screen.iter().for_each(|sc| sc.wheel(ticks));
            }
            Action::Xr(command) => android_action(s, move |d| d.xr(&command).map(drop)),
            Action::ToggleStylus => toggle_stylus(s),
            Action::ToggleView3d | Action::ToggleCarPlay | Action::View3d(_) | Action::PopOut(_) | Action::PopIn(_) => {}
            Action::Debug(a) => {
                if let Some(udid) = s.live.as_ref().map(|l| l.udid.clone())
                    && let Some(msg) = s.debug.perform(&udid, a)
                {
                    s.status = Some(msg);
                }
            }
        }
    }
    for input in coalesce_moves(std::mem::take(&mut s.inputs)) {
        if s.offline || (s.live.is_none() && s.screen.is_none()) {
            continue;
        }
        match input {
            Input::Key(KeyAction::Home) => press_button(s, Button::Home, t, tx),
            Input::Key(KeyAction::Lock) => press_button(s, Button::Lock, t, tx),
            Input::Key(KeyAction::Screenshot) => {
                s.actions.push(Action::Screenshot);
                tx.request_poll();
            }
            Input::Key(KeyAction::Strokes(strokes)) if s.screen.is_some() => {
                if let Some(sc) = &s.screen {
                    sc.strokes(strokes);
                }
            }
            input if s.stylus && s.screen.is_some() => stylus_input(s, input),
            input => apply(s, input),
        }
    }
    let (debug_changed, debug_status) = s.debug.drain();
    if let Some(msg) = debug_status {
        s.status = Some(msg);
    }
    changed |= debug_changed;
    for line in s.outbox.take() {
        s.status = Some(line);
        changed = true;
    }
    if s.busy.is_some() && s.work_done.load(Ordering::Relaxed) {
        s.busy = None;
        if let Some(e) = s.work_error.lock().ok().and_then(|mut g| g.take()) {
            s.status = Some(e);
        }
        s.connect_pending = true;
        s.rescan();
        changed = true;
    }
    if let Some(scan) = s.scanner.as_ref().and_then(scan::Scanner::take) {
        changed |= s.apply_scan(scan);
    }
    // Fit the window to the device once its screen size is known.
    let live_size = s.live.as_ref().and_then(|l| l.shared.size()).or_else(|| s.screen.as_ref().and_then(android::Screen::device_size));
    if let Some(size) = live_size
        && s.sized_for != Some(size)
    {
        s.sized_for = Some(size);
        let form_factor = s.selected_info().and_then(|i| i.form_factor);
        let bezel = s.selected_info().and_then(controls::bezel).is_some();
        // The window opened at its saved size for this device keeps it; resized or hidden panes
        // widen or narrow the fit.
        if s.panes.keep_window.take().is_none_or(|udid| s.selected.as_ref() != Some(&udid)) {
            let (w, h) = fit_window(size, form_factor, bezel);
            tx.resize_window((w + panes::chrome_delta(&s.prefs.current, true), h));
        }
        changed = true;
    }
    // The mark turns while a job runs or a device starts or stops: a frame every
    // `mark::FRAME`, and none once everything rests.
    if s.busy.is_some() || s.devices.iter().any(|d| mark::moves(d.state)) {
        let from = *s.mark_from.get_or_insert(t);
        s.mark_t = (t - from) as f32;
        changed = true;
        tx.wake_after(mark::FRAME);
    } else if s.mark_from.take().is_some() {
        changed = true;
    }
    // The 3D view's camera and turn: a frame for every step of a running change and one to land
    // it, then nothing.
    if s.view3d.view.step(t) {
        changed = true;
        if s.view3d.view.moving() {
            tx.wake_after(Duration::ZERO);
        }
    }
    // Screenshot previews: a frame for every step of a slide, a wake at the next expiry.
    changed |= s.previews.step(t);
    if let Some(secs) = s.previews.wake_in(t) {
        tx.wake_after(Duration::from_secs_f64(secs));
    }
    changed |= resources::poll(s, t, tx);
    changed |= panes::poll(s, t, tx);
    changed |= popout::poll(s, t, tx);
    changed
}

/// Boots the listed devices, past admission already.
fn boot(s: &mut State, udids: &[String]) {
    for udid in udids {
        let Some(info) = s.devices.iter().find(|d| &d.udid == udid).cloned() else { continue };
        match info.platform {
            Platform::Ios => {
                let Some(waker) = s.waker.clone() else { continue };
                s.busy = Some("Booting…");
                s.work_done.store(false, Ordering::Relaxed);
                device::boot_async(info.udid, s.work_done.clone(), s.work_error.clone(), waker);
            }
            Platform::Android => {
                let Some(backend) = s.android_backend.clone() else { continue };
                // With audio: a person is watching this one (tools and farms boot silent).
                s.run_async("Booting…", move || backend.device(&info.udid)?.boot(&backend, Duration::from_secs(300), true));
            }
        }
    }
}

/// ⌘3: the 3D view on or off, or the reason it can't be (a backend without depth buffers, a
/// device with no body).
fn toggle_view3d(s: &mut State) {
    s.status = s.view3d.flip();
    if s.view3d.on && s.showing_screen() && scene::body_for(s).is_none() {
        s.status = Some(scene::NO_BODY.clone());
    } else if s.view3d.on {
        s.status = Some(scene::ON.into());
    }
}

/// The toolbar's CarPlay button: attaches the iPhone's CarPlay display through the API's `carplay`
/// off the UI thread (it takes a second or more), then the capture thread shows it in the
/// iPhone's place as soon as it has rendered; off, the iPhone's screen comes back at once and the
/// display detaches behind it. A headless scene swaps its synthetic screens.
fn toggle_carplay(s: &mut State) {
    let on = !(s.carplay_shown() || s.live.as_ref().is_some_and(|l| l.shared.carplay.load(Ordering::Relaxed)));
    if s.offline {
        show_offline_carplay(s, on);
        return;
    }
    // One attach or detach at a time: a second press while one runs would race it.
    let Some(live) = s.live.as_ref().filter(|_| s.busy.is_none()) else { return };
    let (udid, shared) = (live.udid.clone(), live.shared.clone());
    if !on {
        shared.carplay.store(false, Ordering::Relaxed);
    }
    s.run_async(if on { "Attaching CarPlay…" } else { "Detaching CarPlay…" }, move || {
        let params = serde_json::json!({"device": udid, "on": on});
        sim_api::call_method(&mut sim_api::Session::default(), "carplay", &params)?;
        shared.carplay.store(on, Ordering::Relaxed);
        Ok(())
    });
}

/// Window shortcuts the focused control left alone: ⌘3 toggles the 3D view; while it shows, ⌘0,
/// ⌘1 ⌘2 ⌘4 ⌘5, ⌘F and ⌘[ ⌘] move it ([`scene::Command::for_key`]). ⌘S saves a screenshot
/// wherever the focus is. ⌘⌥S and ⌘⌥I hide and show the sidebar and the inspector ([`panes`]),
/// ⌘⌥R the resource panel ([`resources`]).
fn on_key(s: &mut State, k: &rune::KeyEvent, tx: &mut Tx) -> bool {
    if panes::on_key(s, k, tx) || popout::on_key(s, k, tx) {
        return true;
    }
    let action = if k.key == rune::Key::Command('3') && !k.mods.shift {
        Action::ToggleView3d
    } else if keys::map(k) == KeyAction::Screenshot {
        Action::Screenshot
    } else if let Some(c) = scene::Command::for_key(k)
        && scene::active(s).is_some()
    {
        Action::View3d(c)
    } else {
        return false;
    };
    s.actions.push(action);
    tx.request_poll();
    true
}

/// The toolbar's Stylus button (Android tablets): the pointer becomes a pen (what stock's gRPC
/// pen events send, through simee's `pen` console command); turning it off takes the pen out
/// of range.
fn toggle_stylus(s: &mut State) {
    if s.stylus {
        stylus_off(s);
        s.status = Some("Stylus off".into());
        return;
    }
    let Some(udid) = s.screen.as_ref().map(|sc| sc.udid.clone()) else {
        s.status = Some("The stylus needs a running Android device".into());
        return;
    };
    let find = android::lookup(s.android_backend.clone().unwrap_or_default(), udid);
    s.pen = Some(stylus::Pen::for_device(find, s.outbox.clone()));
    s.stylus = true;
    s.status = Some("Stylus on: drag to draw, move over the screen to hover".into());
}

/// Turns the stylus off, taking the pen out of range first: its thread sends the leave after
/// whatever is still queued, then ends.
fn stylus_off(s: &mut State) {
    if let Some(pen) = s.pen.take() {
        pen.send(sim_core::stylus::PenSample::new(0, 0, sim_core::stylus::PenPhase::Leave));
    }
    s.stylus = false;
}

/// Pointer input while the stylus is on: one pen sample per input, on the pen's own line to the
/// device. Anything else goes on as usual.
fn stylus_input(s: &State, input: Input) {
    use sim_core::stylus::{PenPhase, PenSample};
    let (phase, x, y) = match input {
        Input::Down(x, y) => (PenPhase::Down, x, y),
        Input::Move(x, y) => (PenPhase::Move, x, y),
        Input::Up(x, y) => (PenPhase::Up, x, y),
        Input::Hover(x, y) => (PenPhase::Hover, x, y),
        Input::Leave => (PenPhase::Leave, 0.0, 0.0),
        other => return apply(s, other),
    };
    let (w, h) = s.screen_size();
    let at = |v: f32, max: u32| (v.round().max(0.0) as i32).min(max.max(1) as i32 - 1);
    let sample = PenSample::new(at(x, w), at(y, h), phase);
    if let Some(pen) = &s.pen {
        pen.send(sample);
    }
}

/// The widest window the fit asks for, in points: a 13-inch laptop's display (1440 points)
/// holds it whole.
const MAX_WINDOW_W: f32 = 1440.0;

/// The window that fits a device screen of `size` pixels: the sidebar (240), the gutters (32),
/// the screen at [`android::SCREEN_POINTS`] tall (at least 420 wide, for the toolbar) and the
/// Android panel (340); 110 points of toolbar and status around it. A device with a bezel
/// ([`controls::bezel`]) has a [`bezel::BAND`] of stage around its screen on every side, which the
/// window budgets on top, so the screen is drawn at the size it has without one. A landscape tablet's screen
/// (2560x1600 is 1216 points wide at that height) would make the window wider than a laptop's
/// display, so a tablet's wide screen is shown shorter instead, as far as fits [`MAX_WINDOW_W`].
/// Other form factors (an unfolded foldable, automotive, TV) keep the full-height screen.
fn fit_window(size: (u32, u32), form_factor: Option<FormFactor>, bezel: bool) -> (f32, f32) {
    const CHROME_W: f32 = 240.0 + 32.0 + 340.0;
    let aspect = size.0 as f32 / size.1.max(1) as f32;
    let full_w = android::SCREEN_POINTS * aspect;
    let band = if bezel { 2.0 * bezel::BAND } else { 0.0 };
    // A landscape tablet keeps its laptop-sized window: the band comes out of the screen there.
    let screen_w = if form_factor == Some(FormFactor::Tablet) { full_w.min(MAX_WINDOW_W - CHROME_W - band) } else { full_w };
    let screen_h = screen_w / aspect;
    (CHROME_W + (screen_w + band).max(420.0), screen_h + band + 110.0)
}

/// Sends one touch or keystroke input to the device on screen.
fn apply(s: &State, input: Input) {
    let (x, y, phase) = match input {
        Input::Down(x, y) => (x, y, Phase::Down),
        Input::Move(x, y) => (x, y, Phase::Move),
        Input::Up(x, y) => (x, y, Phase::Up),
        Input::Key(KeyAction::Strokes(strokes)) => {
            s.live.iter().for_each(|l| l.strokes(&strokes));
            return;
        }
        Input::Edge(x, y, phase, edge) => return edge_touch(s, x, y, phase, edge),
        Input::Key(_) | Input::Hover(..) | Input::Leave => return,
    };
    if let Some(live) = &s.live {
        // Edge touches go through the API's worker; while any are still on their way, this one
        // follows them there, so a tap after a bezel swipe can't overtake it. CarPlay's display
        // has no edge gestures: its touches go straight to it.
        if s.bezel.busy() && !s.carplay_shown() {
            s.bezel.send(&live.udid, (x, y), phase, None);
        } else {
            live.touch(x, y, phase);
        }
    } else if let Some(sc) = &s.screen {
        sc.touch(x, y, phase);
    }
}

/// Sends one sample of a touch from the screen's `edge` (a drag from the bezel): on iOS through the
/// API's `touch` with `edge`, on Android as an ordinary touch, already on the edge.
/// CarPlay's display has no edge gestures: there it is an ordinary touch.
fn edge_touch(s: &State, x: f32, y: f32, phase: Phase, edge: sim_core::gesture::ScreenEdge) {
    if let Some(live) = s.live.as_ref().filter(|_| s.carplay_shown()) {
        live.touch(x, y, phase);
    } else if let Some(live) = &s.live {
        s.bezel.send(&live.udid, (x, y), phase, Some(edge));
    } else if let Some(sc) = &s.screen {
        sc.touch(x, y, phase);
    }
}

/// Saves the current frame to the Desktop, off the UI thread: an Android device's emulator
/// writes its PNG itself (on the console worker, no decode and re-encode), an iOS frame is
/// copied here and encoded on a thread. The result reaches the status line through the outbox;
/// returns a status line only for what fails at once.
fn screenshot(s: &State) -> Option<String> {
    let path = work::screenshot_path(s.prefs.current.screenshot_dir.as_deref());
    let inbox = s.previews.inbox();
    if let Some(sc) = &s.screen {
        sc.console(move |d| match d.screenshot_png_to(&path) {
            Ok(_) => {
                inbox.post_file(&path);
                Ok(Some(format!("saved {}", path.display())))
            }
            Err(e) => Err(format!("screenshot failed: {e}")),
        });
        return None;
    }
    let frame = match s.live.as_ref().map(Live::capture) {
        Some(Ok(f)) => f,
        Some(Err(e)) => return Some(format!("screenshot failed: {e}")),
        None => {
            let (width, height) = s.pixels.size();
            sim_core::frame::Frame { width, height, rgba: s.pixels.rgba() }
        }
    };
    if frame.width == 0 || frame.height == 0 {
        return Some("no frame to save".into());
    }
    shot_preview::save_png(frame, path, s.outbox.clone(), inbox);
    None
}

fn main() {
    // Live Rust editing runs the spec as it is: no saved window frame.
    #[cfg_attr(feature = "hot", allow(unused_mut))]
    let mut spec = AppSpec::<State> {
        scenes: &[
            ("demo", demo),
            ("android", android_scene),
            ("foldable", foldable_scene),
            ("folded", folded_scene),
            ("tablet", tablet_scene),
            ("ipad", ipad_scene),
            ("watch", watch_scene),
            ("tv", tv_scene),
            ("apple-watch", apple_watch_scene),
            ("apple-tv", apple_tv_scene),
            ("auto", auto_scene),
            ("carplay", carplay_scene),
            ("vision-pro", vision_pro_scene),
            ("android-xr", android_xr_scene),
            ("glasses", glasses_scene),
            ("turned", turned_scene),
            ("turned-tablet", turned_tablet_scene),
            ("empty", empty),
            ("screenshot", |s| {
                demo(s);
                shot_preview::demo_shots(s, 2);
            }),
            ("android-screenshot", |s| {
                android_scene(s);
                shot_preview::demo_shots(s, 1);
            }),
            ("sidebar-hidden", panes::sidebar_hidden_scene),
            ("inspector-hidden", panes::inspector_hidden_scene),
            ("panes-hidden", panes::panes_hidden_scene),
            ("panes-wide", panes::panes_wide_scene),
            ("resources", resources::scene),
            ("resources-memory", resources::memory_scene),
            ("popped", popout::scene),
            ("admission", admission::scene),
            ("admission-many", admission::many_scene),
        ],
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((1000.0, 560.0)),
        ..AppSpec::new("simee", (1240.0, 870.0), init, admission::view)
    };
    // Live Rust editing runs the spec its own way, without the visibility hook.
    #[cfg(feature = "hot")]
    rune::run(spec);
    #[cfg(not(feature = "hot"))]
    {
        let setting = std::env::var("SIMEE_GPU_BACKEND").ok();
        // Before the window's state is made: it reads whether the 3D view can draw. Both
        // backends draw it (Rune's native Metal one since anomalyco/rune#2563, #193).
        scene::set_supported(true);
        // Headless runs and scenes never read or write the user's settings.
        let store = if headless() { settings::Store::default() } else { settings::Store::open_default() };
        let opened_at = panes::open_at(&mut spec, &store);
        // With the view off the compositor draws Rune's default frame.
        let app = rune::App::new(spec)
            .on_visibility(on_visibility)
            .on_window(panes::remember_window)
            .on_close(popout::on_main_close)
            .on_quit(popout::on_main_close)
            .init(move || init_with(store))
            .compositor(scene::Scene::default());
        let app = match opened_at {
            Some(options) => app.window_options(options),
            None => app,
        };
        let app = match gpu_backend(setting.as_deref()) {
            Ok(Some(backend)) => app.gpu_backend(backend),
            Ok(None) => app,
            Err(e) => {
                eprintln!("simee-desktop: {e}");
                app
            }
        };
        app.run();
    }
}

/// The GPU backend `SIMEE_GPU_BACKEND` chooses on macOS: `metal` (Rune's native Metal backend,
/// what the window and the 3D view draw with when it is unset) or `wgpu` (wgpu's Metal backend).
/// Unset or empty keeps Rune's default; an unknown name is an error the caller reports before
/// keeping it. `RUNE_GPU_BACKEND` still wins over either. What each costs is in docs/desktop.md
/// and docs/benchmarks.md.
#[cfg_attr(feature = "hot", allow(dead_code))]
fn gpu_backend(setting: Option<&str>) -> Result<Option<rune::GpuBackend>, String> {
    match setting.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") => Ok(None),
        Some("metal") => Ok(Some(rune::GpuBackend::Metal)),
        Some("wgpu") => Ok(Some(rune::GpuBackend::Wgpu)),
        Some(other) => Err(format!("SIMEE_GPU_BACKEND={other}: expected metal or wgpu; drawing with the default")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Key, KeyEvent, Runtime};

    fn runtime(scene: fn(&mut State)) -> Runtime<State> {
        let mut s = init();
        scene(&mut s);
        let mut rt = Runtime::new(s, view::view, (1240.0, 870.0), Env::new(true, 2.0));
        rt.frame(0.0);
        rt
    }

    /// As the app runs: every frame polls first, so queued actions are performed (the plain
    /// runtime leaves them queued for tests that check what a handler queued).
    fn polled_runtime(scene: fn(&mut State)) -> Runtime<State> {
        let mut rt = runtime(scene);
        rt.poll = Some(poll);
        rt
    }

    fn press(rt: &mut Runtime<State>, key: &str) {
        let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(1.0 / 60.0);
    }

    #[test]
    fn demo_scene_shows_devices_screen_and_controls() {
        let rt = runtime(demo);
        for key in ["device-list", "device-0", "device-1", "device-2", "device-screen", "home-button", "lock-button", "device-title"] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        assert!(rt.find("placeholder").is_none());
        // The 3D view is off unless asked for: the window is the flat one.
        assert!(rt.find("scene-3d").is_none());
    }

    fn three_d(s: &mut State) {
        demo(s);
        s.view3d = scene::Toggle { on: true, supported: true, ..scene::Toggle::default() };
    }

    fn cmd(c: char) -> KeyEvent {
        KeyEvent { key: Key::Command(c), mods: rune::Mods { cmd: true, ..rune::Mods::default() } }
    }

    /// The 3D view lays its slot over the stage only: the toolbar and the panel still take
    /// clicks, the flat screen stays laid out beneath (it is the scene's texture) and a drag
    /// there sends no touch.
    #[test]
    fn three_d_view_covers_the_stage_and_leaves_the_controls_working() {
        let mut rt = runtime(three_d);
        let ([sx, sy, sw, sh], _) = rt.presented("scene-3d").expect("the 3D slot is drawn");
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("the flat screen stays laid out");
        assert!(sx <= x && sy <= y && sx + sw >= x + w && sy + sh >= y + h, "the slot covers the screen");
        let (_, hy) = rt.find("home-button").unwrap();
        assert!(hy < sy, "the toolbar is above the slot");
        press(&mut rt, "home-button");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Home)]);
        rt.state.actions.clear();
        rt.pointer_down(x + w / 2.0, y + h / 2.0);
        rt.pointer_up(x + w / 2.0, y + h / 2.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.inputs.is_empty(), "no touch through the 3D view: {:?}", rt.state.inputs);
        press(&mut rt, "device-1");
        assert_eq!(rt.state.actions, vec![Action::Select("B2".into())]);
    }

    /// One click on the 3D view must not blur the device: keys still reach it.
    #[test]
    fn keys_still_reach_the_device_after_a_click_on_the_3d_view() {
        let mut rt = runtime(three_d);
        press(&mut rt, "scene-3d");
        rt.state.inputs.clear();
        rt.key(KeyEvent::from(Key::Text("a".into())));
        rt.key(KeyEvent::from(Key::Enter));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.inputs.len(), 2, "{:?}", rt.state.inputs);
        assert!(matches!(&rt.state.inputs[0], Input::Key(KeyAction::Strokes(s)) if s[0].usage == 0x04));
    }

    #[test]
    fn cmd_3_toggles_the_3d_view_where_the_compositor_draws_it() {
        let mut rt = polled_runtime(demo);
        rt.on_key = Some(on_key);
        rt.state.view3d.supported = true;
        rt.key(cmd('3'));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.view3d.on);
        assert!(rt.find("scene-3d").is_some());
        assert_eq!(rt.state.status.as_deref(), Some(scene::ON));
        rt.key(cmd('3'));
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.view3d.on);
        assert!(rt.find("scene-3d").is_none());
        assert_eq!(rt.state.status, None, "off says nothing");
        // No compositor (live Rust editing): the toggle says why it does nothing.
        rt.state.view3d.supported = false;
        rt.key(cmd('3'));
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.view3d.on);
        assert_eq!(rt.state.status.as_deref(), Some(scene::UNAVAILABLE));
        assert!(rt.find("scene-3d").is_none());
    }

    /// Runs frames at 60 Hz for `secs`.
    fn run_for(rt: &mut Runtime<State>, secs: f64) {
        for _ in 0..(secs * 60.0).round() as usize {
            rt.frame(1.0 / 60.0);
        }
    }

    /// ⌘0 animates the 3D camera to face on: the window asks for frames only while it moves,
    /// then sleeps again. With the 3D view off the view keys are nobody's.
    #[test]
    fn view_keys_animate_the_3d_camera_and_then_the_window_sleeps() {
        let mut rt = polled_runtime(three_d);
        rt.on_key = Some(on_key);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.wake_deadline(), None, "a still 3D view sleeps");
        rt.key(cmd('0'));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.view3d.view.moving(), "face on animates");
        assert!(rt.wake_deadline().is_some(), "a running change asks for the next frame");
        run_for(&mut rt, 0.4);
        assert!(!rt.state.view3d.view.moving());
        assert_eq!(rt.wake_deadline(), None, "landed: the window sleeps again");
        assert_eq!(rt.state.view3d.view.camera_now(), scene::Camera::preset(scene::Preset::Face));
        // ⌘F turns the device over, ⌘2 moves the camera to the front view.
        rt.key(cmd('f'));
        rt.key(cmd('2'));
        run_for(&mut rt, 0.4);
        assert!(rt.state.view3d.view.turn_now().flipped());
        assert_eq!(rt.state.view3d.view.camera_now(), scene::Camera::preset(scene::Preset::Front));
        assert_eq!(rt.wake_deadline(), None);
        // Off, ⌘0 does nothing.
        rt.state.view3d.on = false;
        rt.frame(1.0 / 60.0);
        rt.key(cmd('0'));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.actions.is_empty() && !rt.state.view3d.view.moving());
    }

    /// The toolbar has Face on and Flip while the 3D view shows, and a drag on the 3D view
    /// orbits the camera without touching the device.
    #[test]
    fn the_3d_toolbar_and_a_drag_move_the_view() {
        let mut rt = polled_runtime(three_d);
        assert!(rt.find("face-on-button").is_some() && rt.find("flip-button").is_some());
        press(&mut rt, "flip-button");
        run_for(&mut rt, 0.4);
        assert!(rt.state.view3d.view.turn_now().flipped());
        press(&mut rt, "face-on-button");
        run_for(&mut rt, 0.4);
        assert!(!rt.state.view3d.view.turn_now().flipped());
        let before = rt.state.view3d.view.camera_now();
        let ([x, y, w, h], _) = rt.presented("scene-3d").expect("the 3D slot is drawn");
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        rt.pointer_down(cx, cy);
        rt.frame(1.0 / 60.0);
        rt.pointer_move(cx + 60.0, cy);
        rt.frame(1.0 / 60.0);
        rt.pointer_move(cx + 120.0, cy + 20.0);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(cx + 120.0, cy + 20.0);
        rt.frame(1.0 / 60.0);
        let after = rt.state.view3d.view.camera_now();
        assert!(after.yaw < before.yaw && after.pitch > before.pitch, "{before:?} -> {after:?}");
        assert!(rt.state.inputs.is_empty(), "no touch: {:?}", rt.state.inputs);
        assert_eq!(rt.wake_deadline(), None, "a drag asks for no wake");
        // A pinch over it zooms in.
        rt.magnify_event(rune::MagnifyEvent::new(0.5).at(cx, cy));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.view3d.view.camera_now().zoom > 0.5);
    }

    #[test]
    fn the_no_body_status_names_every_form_factor_now_that_each_has_a_body() {
        // No form factor lacks a body, so no scene can show the flat fallback; the status that a
        // future one would get lists them all.
        assert!(sim_core::form_factor::FormFactor::ALL.into_iter().all(sim_core::body::has_body));
        for name in ["phones", "tablets", "foldables", "watches", "TVs", "car head units", "XR headsets and glasses"] {
            assert!(scene::NO_BODY.contains(name), "{name}: {}", scene::NO_BODY.as_str());
        }
        let mut rt = polled_runtime(vision_pro_scene);
        rt.on_key = Some(on_key);
        rt.state.view3d.supported = true;
        rt.key(cmd('3'));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.view3d.on);
        assert_ne!(rt.state.status.as_deref(), Some(scene::NO_BODY.as_str()), "a Vision Pro has a body");
        assert!(rt.find("scene-3d").is_some());
    }

    #[test]
    fn form_factor_scenes_show_their_device_in_3d_or_say_it_has_no_body() {
        use sim_core::platform::{AndroidOs, AppleOs, SubPlatform};
        type Scene = fn(&mut State);
        let cases: [(Scene, SubPlatform, (u32, u32)); 4] = [
            (auto_scene, SubPlatform::Android(AndroidOs::Automotive), (1024, 768)),
            (vision_pro_scene, SubPlatform::Apple(AppleOs::VisionOs), (1366, 1024)),
            (android_xr_scene, SubPlatform::Android(AndroidOs::Xr), (1280, 1279)),
            (glasses_scene, SubPlatform::Android(AndroidOs::Xr), (450, 450)),
        ];
        for (scene_fn, sub, size) in cases {
            let mut s = init();
            s.view3d = scene::Toggle { on: true, supported: true, ..scene::Toggle::default() };
            scene_fn(&mut s);
            let d = s.selected_info().expect("a device is selected");
            let kind = d.kind().expect("a kind");
            assert_eq!((kind.sub_platform, s.offline_size), (sub, size), "{}", d.name);
            assert!(s.showing_screen(), "{}", d.name);
            // A body once its form factor has one; until then the flat screen and the reason.
            let body = scene::body_for(&s);
            assert_eq!(body.is_some(), sim_core::body::has_body(kind.form_factor), "{}", d.name);
            let status = if body.is_none() { Some(scene::NO_BODY.as_str()) } else { None };
            assert_eq!(s.status.as_deref(), status, "{}", d.name);
            if let Some(b) = body {
                let placed = scene::place(&b, size, sim_core::rotation::Rotation::Deg0).expect("placed");
                let screen = b.screen(placed.display).expect("the placed screen");
                // The frame lands on the body screen at its shape (either orientation), not a
                // fallback: a half-size scene (a headset's) is the same frame at half the pixels.
                let (px, size) = ((u64::from(screen.px.0), u64::from(screen.px.1)), (u64::from(size.0), u64::from(size.1)));
                assert!(px.0 * size.1 == px.1 * size.0 || px.0 * size.0 == px.1 * size.1, "{}: {px:?} vs {size:?}", d.name);
            }
        }
        // The glasses' body is built from the glasses' own screen, not the headset's.
        let mut s = init();
        glasses_scene(&mut s);
        let d = s.selected_info().expect("glasses");
        let tag = d.runtime_id.split(';').nth(2).expect("an image tag");
        let (w, h, _) = sim_core::form_factor::ANDROID_GLASSES_SCREEN;
        let p = d.kind().expect("a kind").image_profile(tag);
        assert_eq!((p.width, p.height), (w, h));
    }

    fn text_of(rt: &Runtime<State>, key: &str) -> String {
        let e = rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} rendered"));
        e.text.or(e.label).unwrap_or_default()
    }

    #[test]
    fn devices_and_title_carry_platform_labels() {
        let rt = runtime(demo);
        assert_eq!(text_of(&rt, "device-0-platform"), "iOS");
        assert_eq!(text_of(&rt, "device-2-platform"), "iPadOS");
        assert_eq!(text_of(&rt, "device-4-platform"), "watchOS");
        assert_eq!(text_of(&rt, "device-title"), "iPhone 17 · iOS · Phone · Booted");
        let row = rt.elements().into_iter().find(|e| e.key.as_deref() == Some("device-2")).and_then(|e| e.label);
        assert_eq!(row.as_deref(), Some("iPad Pro 11-inch (M4), iPadOS · Tablet"));
    }

    #[test]
    fn the_device_list_groups_platforms() {
        let rt = runtime(demo);
        assert_eq!(text_of(&rt, "platform-ios"), "iOS");
        assert_eq!(text_of(&rt, "platform-android"), "Android");
        assert_eq!(text_of(&rt, "device-5-platform"), "tvOS");
        assert_eq!(text_of(&rt, "device-6-platform"), "visionOS");
        assert_eq!(text_of(&rt, "device-7-platform"), "Android");
        assert_eq!(text_of(&rt, "device-10-platform"), "Wear OS");
        assert_eq!(text_of(&rt, "device-11-platform"), "Android TV");
        assert_eq!(text_of(&rt, "device-12-platform"), "Android Automotive");
    }

    #[test]
    fn android_devices_get_android_controls_and_their_console() {
        let mut rt = runtime(android_scene);
        assert_eq!(text_of(&rt, "device-title"), "pixel_9 · Android · Phone · Booted");
        for key in ["back-button", "home-button", "recents-button", "lock-button", "rotate-button", "android-panel", "log-0", "log-3"] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        for key in ["appearance-button", "debug-panel", "fold-half", "crown-up"] {
            assert!(rt.find(key).is_none(), "{key} is iOS- or form-factor-specific");
        }
        press(&mut rt, "back-button");
        press(&mut rt, "rotate-button");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Back), Action::Rotate]);
    }

    #[test]
    fn form_factors_get_their_own_controls() {
        let mut rt = runtime(foldable_scene);
        press(&mut rt, "fold-half");
        assert_eq!(rt.state.actions, vec![Action::Fold(Posture::HalfOpened)]);
        let mut rt = runtime(watch_scene);
        assert!(rt.find("recents-button").is_none());
        press(&mut rt, "crown-down");
        assert_eq!(rt.state.actions, vec![Action::Rotary(1)]);
        let mut rt = runtime(tv_scene);
        assert!(rt.find("rotate-button").is_none());
        press(&mut rt, "dpad-select");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::DpadCenter)]);
        // Apple's: the crown and side button on a watch, the Siri Remote on a TV.
        let mut rt = runtime(apple_watch_scene);
        assert_eq!(text_of(&rt, "device-title"), "Apple Watch Series 10 (46mm) · watchOS · Watch · Booted");
        assert!(rt.find("lock-button").is_none() && rt.find("rotate-button").is_none());
        press(&mut rt, "crown-up");
        press(&mut rt, "side-button");
        assert_eq!(rt.state.actions, vec![Action::Rotary(-1), Action::Button(Button::Side)]);
        let mut rt = runtime(apple_tv_scene);
        press(&mut rt, "back-button");
        press(&mut rt, "dpad-left");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Menu), Action::Button(Button::DpadLeft)]);
    }

    #[test]
    fn round_watches_get_a_round_screen() {
        let rt = runtime(watch_scene);
        let ([_, _, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert!((w - h).abs() < 1.0, "a square box for a round screen: {w}x{h}");
        assert_eq!(rt.state.shape, Shape::Round);
        assert!(view::screen_radius(Shape::Round, w) >= w / 2.0);
        assert!(view::screen_radius(Shape::Rounded, w) < w / 4.0);
    }

    /// The body a state draws, as a pointer: two states pick the same row or the same nothing.
    fn body_ptr(s: &State) -> Option<*const sim_core::body::Body> {
        scene::body_for(s).as_ref().map(Arc::as_ptr)
    }

    /// The headless `carplay` scene: an iPhone on iOS 26 with its CarPlay display on, shown in
    /// the iPhone's place at the car display's 800x480, with the toggle to turn it off.
    #[test]
    fn the_carplay_scene_shows_the_car_display_in_the_iphones_place() {
        let rt = runtime(carplay_scene);
        assert_eq!(text_of(&rt, "device-title"), "iPhone 17 · iOS · Phone · Booted");
        assert!(rt.state.carplay_shown());
        assert_eq!(rt.state.screen_size(), (sim_core::carplay::WIDTH, sim_core::carplay::HEIGHT));
        let ([_, _, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert!((w / h - 800.0 / 480.0).abs() < 0.01, "the car display's shape: {w}x{h}");
        assert!(rt.find("carplay-button").is_some());
        // An iPhone on iOS 27 has no toggle: CarPlay renders black there.
        let rt = runtime(|s| {
            demo(s);
            s.selected = Some("C3".into());
        });
        assert!(rt.find("home-button").is_some() && rt.find("carplay-button").is_none());
    }

    /// While the CarPlay display shows, the 3D view draws the car head unit's body
    /// (`CarPlay-800x480`), not the iPhone's; with no such body (#503) it stays flat and says so.
    #[test]
    fn body_selection_picks_the_carplay_body_while_the_display_shows() {
        let iphone = runtime(demo);
        let carplay = runtime(carplay_scene);
        assert!(body_ptr(&iphone.state).is_some(), "the iPhone has a body");
        assert_eq!(body_ptr(&carplay.state), sim_core::body::get(scene::CARPLAY_BODY).as_ref().map(Arc::as_ptr));
        assert_ne!(body_ptr(&carplay.state), body_ptr(&iphone.state), "never the iPhone's body on the car display");
        let mut rt = polled_runtime(carplay_scene);
        rt.on_key = Some(on_key);
        rt.state.view3d.supported = true;
        rt.key(cmd('3'));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.view3d.on);
        if sim_core::body::get(scene::CARPLAY_BODY).is_none() {
            assert_eq!(rt.state.status.as_deref(), Some(scene::NO_BODY.as_str()));
            assert!(rt.find("scene-3d").is_none(), "no head unit body yet: flat");
            assert!(rt.find("device-screen").is_some());
        } else {
            assert!(rt.find("scene-3d").is_some(), "the head unit is drawn");
        }
    }

    /// A drag on the CarPlay view lands at CarPlay's pixels (the 800x480 car display, not the
    /// iPhone's screen); the live device normalizes them by that size for `carplay_touch`.
    #[test]
    fn touches_on_the_carplay_view_map_to_carplay_pixels() {
        let mut rt = runtime(carplay_scene);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        rt.pointer_down(x + w * 0.25, y + h * 0.75);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.25, y + h * 0.75);
        rt.frame(1.0 / 60.0);
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            matches!(rt.state.inputs.first(), Some(Input::Down(px, py)) if near(*px, 200.0) && near(*py, 360.0)),
            "{:?}",
            rt.state.inputs
        );
        assert!(matches!(rt.state.inputs.last(), Some(Input::Up(..))), "{:?}", rt.state.inputs);
    }

    /// The toggle turns the CarPlay display off: the iPhone's own screen, its size and its body
    /// come back; on again shows the car display.
    #[test]
    fn toggling_carplay_off_returns_to_the_iphone_screen() {
        let iphone = runtime(demo);
        let mut rt = polled_runtime(carplay_scene);
        press(&mut rt, "carplay-button");
        run_for(&mut rt, 0.5);
        assert!(!rt.state.carplay_shown());
        assert_eq!(rt.state.screen_size(), iphone.state.screen_size());
        assert_eq!(body_ptr(&rt.state), body_ptr(&iphone.state));
        let ([_, _, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert!(h > w, "the iPhone's portrait screen again: {w}x{h}");
        press(&mut rt, "carplay-button");
        rt.frame(1.0 / 60.0);
        assert!(rt.state.carplay_shown());
        assert_eq!(rt.state.screen_size(), (800, 480));
    }

    /// A Duo fold while CarPlay attaches (or any job runs) leaves the busy slot alone: the slot
    /// stays held until the attach call returns, so a second attach can't start beside the fold.
    #[test]
    fn a_fold_waits_for_the_job_holding_the_busy_slot() {
        let mut s = init();
        s.waker = Some(Arc::new(|| {}));
        s.busy = Some("Attaching CarPlay…");
        s.work_done.store(false, Ordering::Relaxed);
        apple_fold(&mut s, "no-such-device".into(), Posture::Closed);
        assert_eq!(s.busy, Some("Attaching CarPlay…"));
        assert!(!s.work_done.load(Ordering::Relaxed), "the attach's completion flag was not reset");
        s.busy = None;
        apple_fold(&mut s, "no-such-device".into(), Posture::Closed);
        assert_eq!(s.busy, Some("Folding…"), "a free slot runs it");
    }

    #[test]
    fn drags_map_to_android_device_pixels() {
        let mut rt = runtime(android_scene);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert_eq!(rt.state.screen_size(), (360, 800));
        rt.pointer_down(x + w * 0.5, y + h * 0.25);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.5, y + h * 0.25);
        rt.frame(1.0 / 60.0);
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            matches!(rt.state.inputs.first(), Some(Input::Down(px, py)) if near(*px, 180.0) && near(*py, 200.0)),
            "{:?}",
            rt.state.inputs
        );
    }

    /// A turned phone's screen is upright: the view's box has the turned shape, the panel's
    /// picture is turned into it, and a drag lands at the upright screen's pixels (emu maps
    /// those onto the panel).
    #[test]
    fn a_turned_screen_is_drawn_upright_and_takes_upright_touches() {
        for (scene, upright) in [(turned_scene as fn(&mut State), (800, 360)), (turned_tablet_scene, (800, 1280))] {
            // The picture takes the box's size from its layout, so it fills the box the frame
            // after: at once, not springing there.
            let mut rt = runtime(scene);
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.state.screen_size(), upright);
            let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
            let ratio = upright.0 as f32 / upright.1 as f32;
            assert!((w / h - ratio).abs() < 0.02, "{upright:?}: the box is {w}x{h}");
            let ([px, py, pw, ph], _) = rt.presented("device-pixels").expect("the turned picture");
            // Its upright bounding box: the screen's own.
            assert!((px - x).abs() < 1.0 && (py - y).abs() < 1.0 && (pw - w).abs() < 1.0 && (ph - h).abs() < 1.0, "{:?}", [px, py, pw, ph]);
            rt.pointer_down(x + w * 0.75, y + h * 0.25);
            rt.frame(1.0 / 60.0);
            rt.pointer_up(x + w * 0.75, y + h * 0.25);
            rt.frame(1.0 / 60.0);
            let near = |a: f32, b: f32| (a - b).abs() < 2.0;
            let (ux, uy) = (upright.0 as f32 * 0.75, upright.1 as f32 * 0.25);
            assert!(
                matches!(rt.state.inputs.first(), Some(Input::Down(dx, dy)) if near(*dx, ux) && near(*dy, uy)),
                "{upright:?}: {:?}",
                rt.state.inputs
            );
        }
        // Unturned, nothing is drawn turned.
        let rt = runtime(android_scene);
        assert!(rt.find("device-pixels").is_none());
    }

    /// A drag that starts just below an iPad's screen is a touch from its bottom edge, from the
    /// edge pixel up to where the pointer goes; one on the screen stays an ordinary touch.
    #[test]
    fn a_drag_from_the_bezel_comes_in_from_that_edge() {
        use sim_core::gesture::ScreenEdge;
        let mut rt = runtime(ipad_scene);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        let (dw, dh) = rt.state.screen_size();
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        rt.pointer_down(x + w * 0.5, y + h + bezel::BAND * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_move(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        let inputs = std::mem::take(&mut rt.state.inputs);
        assert!(
            matches!(inputs.first(), Some(Input::Edge(px, py, Phase::Down, ScreenEdge::Bottom)) if near(*px, dw as f32 * 0.5) && near(*py, dh as f32 - 1.0)),
            "{inputs:?}"
        );
        assert!(
            inputs.iter().any(|i| matches!(i, Input::Edge(_, py, Phase::Move, ScreenEdge::Bottom) if near(*py, dh as f32 * 0.5))),
            "{inputs:?}"
        );
        assert!(matches!(inputs.last(), Some(Input::Edge(_, _, Phase::Up, ScreenEdge::Bottom))), "{inputs:?}");
        assert_eq!(rt.state.bezel.edge, None, "the drag ended");

        // Above the top-right corner: Control Center's edge.
        rt.pointer_down(x + w - 4.0, y - bezel::BAND * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w - 4.0, y + h * 0.3);
        rt.frame(1.0 / 60.0);
        let inputs = std::mem::take(&mut rt.state.inputs);
        assert!(matches!(inputs.first(), Some(Input::Edge(_, py, Phase::Down, ScreenEdge::Top)) if *py == 0.0), "{inputs:?}");

        // On the screen it's a touch; far outside the bezel, nothing.
        rt.pointer_down(x + w * 0.5, y + h - 2.0);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        let inputs = std::mem::take(&mut rt.state.inputs);
        assert!(matches!(inputs.first(), Some(Input::Down(..))), "{inputs:?}");
        assert!(!inputs.iter().any(|i| matches!(i, Input::Edge(..))), "{inputs:?}");
        if x > bezel::BAND * 2.0 + 10.0 {
            rt.pointer_down(x - bezel::BAND * 2.0, y + h * 0.5);
            rt.frame(1.0 / 60.0);
            rt.pointer_up(x, y + h * 0.5);
            rt.frame(1.0 / 60.0);
            assert!(rt.state.inputs.is_empty(), "{:?}", rt.state.inputs);
        }
    }

    /// A click on the bezel that never moves does nothing (on Android its touch would tap the
    /// screen's edge pixel); the touch-down is held until the drag has moved a few points.
    #[test]
    fn a_click_on_the_bezel_does_nothing_until_it_drags() {
        use sim_core::gesture::ScreenEdge;
        for scene in [ipad_scene as fn(&mut State), android_scene] {
            let mut rt = runtime(scene);
            let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
            let at = (x + w * 0.5, y + h + bezel::BAND * 0.5);
            rt.pointer_down(at.0, at.1);
            rt.frame(1.0 / 60.0);
            rt.pointer_move(at.0 + 1.0, at.1 - 2.0);
            rt.frame(1.0 / 60.0);
            rt.pointer_up(at.0 + 1.0, at.1 - 2.0);
            rt.frame(1.0 / 60.0);
            assert!(rt.state.inputs.is_empty(), "a click sends nothing: {:?}", rt.state.inputs);
            assert_eq!(rt.state.bezel.press, None);

            // Past the threshold: down at the press, then the moves, then up.
            rt.pointer_down(at.0, at.1);
            rt.frame(1.0 / 60.0);
            rt.pointer_move(at.0, at.1 - 3.0);
            rt.frame(1.0 / 60.0);
            assert!(rt.state.inputs.is_empty(), "still under the threshold: {:?}", rt.state.inputs);
            rt.pointer_move(at.0, at.1 - bezel::DRAG_THRESHOLD - 4.0);
            rt.frame(1.0 / 60.0);
            rt.pointer_move(at.0, y + h * 0.5);
            rt.frame(1.0 / 60.0);
            rt.pointer_up(at.0, y + h * 0.5);
            rt.frame(1.0 / 60.0);
            let inputs = std::mem::take(&mut rt.state.inputs);
            let phases: Vec<_> = inputs.iter().map(|i| if let Input::Edge(_, _, p, e) = i { (*p, *e) } else { panic!("{i:?}") }).collect();
            assert_eq!(phases.first(), Some(&(Phase::Down, ScreenEdge::Bottom)), "{inputs:?}");
            assert!(phases.len() >= 3 && phases[1..phases.len() - 1].iter().all(|p| p.0 == Phase::Move), "{inputs:?}");
            assert_eq!(phases.last(), Some(&(Phase::Up, ScreenEdge::Bottom)), "{inputs:?}");
        }
    }

    /// Android's edge gestures are ordinary touches from the edge: a bezel drag on an Android phone
    /// starts on the left edge pixel (gesture navigation's back); a TV has no bezel.
    #[test]
    fn android_bezel_drags_start_on_the_edge_and_tvs_have_no_bezel() {
        use sim_core::gesture::ScreenEdge;
        let mut rt = runtime(android_scene);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert!(rt.find("device-bezel").is_some());
        rt.pointer_down(x - bezel::BAND * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.3, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        assert!(
            matches!(rt.state.inputs.first(), Some(Input::Edge(px, _, Phase::Down, ScreenEdge::Left)) if *px == 0.0),
            "{:?}",
            rt.state.inputs
        );
        // Turned a quarter, the edge pixels are the upright screen's (800x360), which emu maps
        // onto the panel as it does any touch: the bottom edge is y 359, not the panel's 799.
        let mut rt = runtime(turned_scene);
        rt.frame(1.0 / 60.0);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert_eq!(rt.state.screen_size(), (800, 360));
        rt.pointer_down(x + w * 0.5, y + h + bezel::BAND * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            matches!(rt.state.inputs.first(), Some(Input::Edge(px, py, Phase::Down, ScreenEdge::Bottom)) if near(*px, 400.0) && *py == 359.0),
            "{:?}",
            rt.state.inputs
        );
        let rt = runtime(tv_scene);
        assert!(rt.find("device-bezel").is_none());
        let rt = runtime(apple_tv_scene);
        assert!(rt.find("device-bezel").is_none());
    }

    #[test]
    fn edge_moves_coalesce_but_keep_their_ends() {
        use sim_core::gesture::ScreenEdge::Bottom;
        let inputs = vec![
            Input::Edge(1.0, 9.0, Phase::Down, Bottom),
            Input::Edge(1.0, 8.0, Phase::Move, Bottom),
            Input::Edge(1.0, 7.0, Phase::Move, Bottom),
            Input::Edge(1.0, 6.0, Phase::Up, Bottom),
        ];
        assert_eq!(
            coalesce_moves(inputs),
            [
                Input::Edge(1.0, 9.0, Phase::Down, Bottom),
                Input::Edge(1.0, 7.0, Phase::Move, Bottom),
                Input::Edge(1.0, 6.0, Phase::Up, Bottom)
            ]
        );
    }

    #[test]
    fn empty_scene_shows_placeholders() {
        let rt = runtime(empty);
        assert!(rt.find("devices-empty").is_some());
        assert!(rt.find("placeholder").is_some());
        assert!(rt.find("device-screen").is_none());
    }

    #[test]
    fn buttons_queue_actions() {
        let mut rt = runtime(demo);
        press(&mut rt, "home-button");
        press(&mut rt, "lock-button");
        press(&mut rt, "device-1");
        assert_eq!(rt.state.actions, vec![Action::Button(Button::Home), Action::Button(Button::Lock), Action::Select("B2".into())]);
    }

    #[test]
    fn drags_map_to_device_pixels() {
        let mut rt = runtime(demo);
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        let (dw, dh) = rt.state.screen_size();
        rt.pointer_down(x + w * 0.25, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_move(x + w * 0.75, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.pointer_up(x + w * 0.75, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        let inputs = &rt.state.inputs;
        let near = |a: f32, b: f32| (a - b).abs() < 2.0;
        assert!(
            matches!(inputs.first(), Some(Input::Down(px, py)) if near(*px, dw as f32 * 0.25) && near(*py, dh as f32 * 0.5)),
            "{inputs:?}"
        );
        assert!(inputs.iter().any(|i| matches!(i, Input::Move(px, _) if near(*px, dw as f32 * 0.75))), "{inputs:?}");
        assert!(matches!(inputs.last(), Some(Input::Up(px, _)) if near(*px, dw as f32 * 0.75)), "{inputs:?}");
    }

    #[test]
    fn debug_panel_tabs_render_and_queue_actions() {
        let mut rt = runtime(demo);
        for key in [
            "debug-panel",
            "tab-device",
            "overlay-blended-layers",
            "toggle-contrast",
            "text-larger",
            "bio-face-match",
            "status-demo",
            "location-tokyo",
        ] {
            assert!(rt.find(key).is_some(), "{key} rendered");
        }
        press(&mut rt, "overlay-blended-layers");
        press(&mut rt, "text-larger");
        assert_eq!(
            rt.state.actions,
            vec![Action::Debug(debug::DebugAction::ToggleOverlay(2)), Action::Debug(debug::DebugAction::TextSize(1))]
        );
        rt.state.actions.clear();
        for (tab, row) in
            [("tab-network", "socket-0"), ("tab-storage", "container-0"), ("tab-processes", "process-0"), ("tab-logs", "log-0")]
        {
            let next = match tab {
                "tab-network" => debug::Tab::Network,
                "tab-storage" => debug::Tab::Storage,
                "tab-processes" => debug::Tab::Processes,
                _ => debug::Tab::Logs,
            };
            rt.update(move |s, _| s.debug.tab = next);
            rt.frame(1.0 / 60.0);
            assert!(rt.find(row).is_some(), "{row} rendered on {tab}");
        }
        press(&mut rt, "logs-2m");
        assert_eq!(rt.state.actions, vec![Action::Debug(debug::DebugAction::Logs("2m"))]);
    }

    #[test]
    fn every_icon_renders() {
        fn icon_view(_: &State, _: &mut rune::Cx<State>) -> rune::El {
            let mut r = rune::row();
            for (i, glyph) in icons::ALL.iter().enumerate() {
                r = r.child(rune::icon(glyph, 16.0).key(format!("icon-{i}")));
            }
            r
        }
        let mut rt = Runtime::new(init(), icon_view, (900.0, 100.0), Env::new(true, 2.0));
        rt.frame(0.0);
        for i in 0..icons::ALL.len() {
            assert!(rt.find(&format!("icon-{i}")).is_some(), "icon {i}");
        }
    }

    /// 1000 moves between two polls send one, the last, before the up; downs, ups and keys keep
    /// their places.
    #[test]
    fn queued_moves_coalesce_to_the_newest_before_the_up() {
        let mut inputs = vec![Input::Down(0.0, 0.0)];
        inputs.extend((1..=1000).map(|i| Input::Move(i as f32, 0.0)));
        inputs.push(Input::Up(1000.0, 0.0));
        inputs.push(Input::Down(5.0, 5.0));
        inputs.push(Input::Move(6.0, 6.0));
        inputs.push(Input::Key(KeyAction::Home));
        inputs.push(Input::Move(7.0, 7.0));
        inputs.push(Input::Move(8.0, 8.0));
        inputs.push(Input::Up(8.0, 8.0));
        assert_eq!(
            coalesce_moves(inputs),
            [
                Input::Down(0.0, 0.0),
                Input::Move(1000.0, 0.0),
                Input::Up(1000.0, 0.0),
                Input::Down(5.0, 5.0),
                Input::Move(6.0, 6.0),
                Input::Key(KeyAction::Home),
                Input::Move(8.0, 8.0),
                Input::Up(8.0, 8.0),
            ]
        );
    }

    /// A scan replaces the list, resolves a `booted` selection when a refresh asked, and an
    /// AVD that failed to resolve says why instead of asking again.
    #[test]
    fn scans_update_the_list_and_selection() {
        let mut s = init();
        demo(&mut s);
        s.offline = false;
        s.selected = Some("booted".into());
        s.resolve_pending = true;
        let mut devices = s.devices.clone();
        for d in &mut devices {
            d.state = if d.udid == "pixel_fold" { DeviceState::Booted } else { DeviceState::Shutdown };
        }
        assert!(s.apply_scan(scan::Scan { devices: devices.clone(), ..scan::Scan::default() }));
        assert_eq!(s.selected.as_deref(), Some("pixel_fold"));
        assert!(s.connect_pending, "waits for a scan that resolves the AVD");
        let failed = scan::Scan {
            devices: devices.clone(),
            selected: Some("pixel_fold".into()),
            android: Some(Err("pixel_fold: config unreadable".into())),
            ..scan::Scan::default()
        };
        s.apply_scan(failed);
        assert_eq!(s.status.as_deref(), Some("pixel_fold: config unreadable"));
        assert!(!s.connect_pending && s.screen.is_none(), "no retry loop for an AVD that can't resolve");
        assert!(!s.apply_scan(scan::Scan { devices, ..scan::Scan::default() }), "same list: nothing changed");
    }

    fn element(rt: &Runtime<State>, key: &str) -> rune::ElementInfo {
        rt.elements().into_iter().find(|e| e.key.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} rendered"))
    }

    /// The selected device's row stands out from the sidebar (and says it's selected to
    /// assistive technology); the others don't, and the highlight follows a new selection.
    #[test]
    fn the_selected_device_is_highlighted() {
        let mut rt = polled_runtime(demo);
        let sel = view::selection(rune::tok::theme(true), true);
        let (row0, row1) = (element(&rt, "device-0"), element(&rt, "device-1"));
        assert!(row0.item_selected && !row1.item_selected, "only iPhone 17 is selected");
        assert_eq!(row0.fill, sel.fill, "the selected row is tinted");
        assert_ne!(row1.fill, sel.fill);
        assert_eq!(element(&rt, "device-0-bar").fill, sel.bar, "and marked with the accent bar");
        assert_eq!(element(&rt, "device-1-bar").fill.a, 0.0, "the bar's slot is empty on the others");
        // The press's frame asks for the next, whose poll selects.
        press(&mut rt, "device-1");
        rt.frame(1.0 / 60.0);
        assert!(element(&rt, "device-1").item_selected && !element(&rt, "device-0").item_selected);
    }

    /// WCAG contrast of two opaque colours (linear light in, as Rune's colours are).
    fn contrast(a: rune::Color, b: rune::Color) -> f32 {
        let (la, lb) = (a.luminance() + 0.05, b.luminance() + 0.05);
        la.max(lb) / la.min(lb)
    }

    /// In both themes: the row's text (and its badge, drawn in the primary text colour when
    /// selected) reads at 4.5:1 on the tint, and the accent bar stands at 3:1 against the sidebar.
    #[test]
    fn the_selection_highlight_has_accessible_contrast_in_light_and_dark() {
        for dark in [true, false] {
            let t = rune::tok::theme(dark);
            let sel = view::selection(t, dark);
            assert!(contrast(t.text, sel.fill) >= 4.5, "text on the tint (dark: {dark}): {}", contrast(t.text, sel.fill));
            assert!(contrast(sel.bar, t.card) >= 3.0, "bar on the sidebar (dark: {dark}): {}", contrast(sel.bar, t.card));
            assert_ne!(sel.fill, t.card, "the tint differs from the sidebar (dark: {dark})");
        }
    }

    /// Rune polls again after a frame's handlers (rune#2391): what a press, a touch or a keystroke
    /// queues is performed in the frame that handled it, with no wake asked for, so the window
    /// sleeps again at once.
    #[test]
    fn work_a_handler_queues_is_performed_in_its_frame() {
        let mut rt = polled_runtime(demo);
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.wake_deadline(), None, "a still window sleeps");
        let before = rt.state.selected.clone();
        for key in ["device-1", "home-button", "overlay-blended-layers", "tab-network", "debug-reload"] {
            press(&mut rt, key);
            assert!(rt.state.actions.is_empty(), "{key}: performed in its frame");
            assert_eq!(rt.wake_deadline(), None, "{key}: no wake, the window sleeps");
        }
        assert!(rt.state.selected.is_some() && rt.state.selected != before, "device-1 was selected in its frame");
        let ([x, y, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        rt.pointer_down(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.inputs.is_empty(), "a touch is performed in its frame: {:?}", rt.state.inputs);
        assert_eq!(rt.wake_deadline(), None, "a touch asks for no wake");
        rt.pointer_up(x + w * 0.5, y + h * 0.5);
        rt.frame(1.0 / 60.0);
        rt.key(KeyEvent::from(Key::Text("a".into())));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.inputs.is_empty(), "a keystroke is performed in its frame: {:?}", rt.state.inputs);
        assert_eq!(rt.wake_deadline(), None, "a keystroke asks for no wake");
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.wake_deadline(), None, "a still window still sleeps");
    }

    /// A fake worker finishing queued work wakes the window, and the poll that wakes it shows
    /// the result.
    #[test]
    fn finished_work_wakes_the_window_and_the_next_poll_shows_it() {
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let mut rt = runtime(demo);
        rt.state.outbox = work::Outbox::new(Some(Arc::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        })));
        let worker = work::Worker::start("fake-worker", (), rt.state.outbox.clone());
        assert!(worker.run(|()| Ok(Some("rotated".into()))));
        let start = std::time::Instant::now();
        while woken.load(Ordering::Relaxed) == 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "the worker woke the window");
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut changed = false;
        rt.update(|s, tx| changed = poll(s, 1.0, tx));
        assert!(changed && rt.state.status.as_deref() == Some("rotated"));
    }

    /// A display socket that takes one client and records its lines.
    pub(crate) fn recording_display(name: &str) -> (std::path::PathBuf, Arc<Mutex<Vec<String>>>) {
        use std::io::BufRead;
        let dir = std::path::PathBuf::from("/tmp").join(format!("rs-main-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("simee")).expect("dir");
        let listener = std::os::unix::net::UnixListener::bind(sim_android::display::socket(&dir)).expect("bind");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let l = lines.clone();
        std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            for line in std::io::BufReader::new(conn).lines().map_while(Result::ok) {
                l.lock().expect("lines").push(line);
            }
        });
        (dir, lines)
    }

    /// Switching away from an Android device pauses its stream and keeps its last frame; switching
    /// back shows that frame at once (the same buffer, no placeholder, no reconnect) and asks the
    /// emulator for live frames again. With the window hidden it still says something (`hello 0`,
    /// staying paused): that write is what finds an emulator that went away meanwhile. The log
    /// panel never shows the device switched away from.
    #[test]
    fn a_paused_device_shows_its_last_frame_when_switched_back() {
        for visible in [true, false] {
            let (dir, lines) = recording_display(&format!("paused{visible}"));
            let mut rt = polled_runtime(android_scene);
            rt.update(|s, _| {
                s.offline = false;
                s.offline_screen = false;
                s.visible = visible;
                let console = work::Worker::start(
                    "test-console",
                    android::lookup(sim_android::Android::default(), "pixel_9".into()),
                    s.outbox.clone(),
                );
                let sc = android::Screen::connect("pixel_9", dir.clone(), 1080, console).expect("connect");
                sc.pixels.set(2, 1, vec![9; 8]);
                s.screen = Some(sc);
            });
            rt.frame(1.0 / 60.0);
            let shown = rt.state.screen.as_ref().map(|sc| sc.pixels.signature()).expect("a screen");
            // Away to a device that isn't running, and back.
            for udid in ["pixel_fold", "pixel_9"] {
                rt.state.console_log = vec![format!("{udid}'s predecessor's log")];
                rt.update(|s, _| s.actions.push(Action::Select(udid.into())));
                rt.frame(1.0 / 60.0);
                assert!(rt.state.console_log.is_empty(), "{udid}: {:?}", rt.state.console_log);
                if udid == "pixel_fold" {
                    assert!(rt.find("placeholder").is_some() && rt.state.screen.is_none(), "the fold isn't running");
                }
            }
            assert!(rt.find("placeholder").is_none() && rt.find("device-screen").is_some(), "the screen shows at once");
            let sc = rt.state.screen.as_ref().expect("the paused screen came back");
            assert_eq!(sc.pixels.signature(), shown, "the same buffer and frame, not a new one");
            assert_eq!(sc.pixels.rgba(), [9; 8]);
            let start = std::time::Instant::now();
            while lines.lock().expect("lines").len() < 5 {
                assert!(start.elapsed() < Duration::from_secs(5), "{:?}", lines.lock().expect("lines"));
                std::thread::sleep(Duration::from_millis(2));
            }
            let back = if visible { "hello 1080" } else { "hello 0" };
            assert_eq!(
                *lines.lock().expect("lines"),
                ["shm", "rotates", "hello 1080", "hello 0", back],
                "paused, then resumed (visible: {visible})"
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn typing_on_the_focused_screen_queues_keystrokes() {
        let mut rt = runtime(demo);
        press(&mut rt, "device-screen");
        rt.state.inputs.clear();
        rt.key(KeyEvent::from(Key::Text("a".into())));
        rt.key(KeyEvent::from(Key::Enter));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.inputs.len(), 2, "{:?}", rt.state.inputs);
        assert!(matches!(&rt.state.inputs[0], Input::Key(KeyAction::Strokes(s)) if s[0].usage == 0x04));
    }

    #[test]
    fn the_stylus_hovers_where_the_pointer_is_and_leaves_with_it() {
        let mut rt = runtime(demo);
        let (x, y) = rt.find("device-screen").expect("screen");
        rt.state.inputs.clear();
        rt.pointer_move(x, y);
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.inputs.iter().any(|i| matches!(i, Input::Hover(..))), "no hover with the stylus off");
        rt.update(|s, _| s.stylus = true);
        rt.pointer_move(x + 1.0, y);
        rt.frame(1.0 / 60.0);
        let (w, h) = rt.state.screen_size();
        let hover = rt.state.inputs.iter().find_map(|i| if let Input::Hover(hx, hy) = i { Some((*hx, *hy)) } else { None });
        let (hx, hy) = hover.expect("a hover sample");
        assert!((hx - w as f32 / 2.0).abs() < w as f32 * 0.02 && (hy - h as f32 / 2.0).abs() < h as f32 * 0.02, "{hover:?} on {w}x{h}");
        rt.pointer_leave();
        rt.frame(1.0 / 60.0);
        assert!(matches!(rt.state.inputs.last(), Some(Input::Leave)), "{:?}", rt.state.inputs);
        // Offline (no Android screen) the toggle says why it does nothing.
        rt.update(|s, _| {
            s.stylus = false;
            toggle_stylus(s);
        });
        assert!(!rt.state.stylus && rt.state.status.as_deref() == Some("The stylus needs a running Android device"));
    }

    /// Switching to another device with the stylus on takes the pen out of range on the device
    /// switched away from, rather than leaving it hovering (or touching) there.
    #[test]
    fn switching_devices_takes_the_pen_out_of_range_first() {
        use sim_core::stylus::{PenPhase, PenSample};
        let sent = Arc::new(Mutex::new(Vec::new()));
        let s2 = sent.clone();
        let pen = stylus::Pen::start(
            Box::new(move || {
                let s = s2.clone();
                Ok(Box::new(move |p: &PenSample| {
                    s.lock().expect("sent").push(p.phase);
                    Ok(())
                }) as stylus::Sink)
            }),
            work::Outbox::default(),
        );
        pen.send(PenSample::new(5, 5, PenPhase::Hover));
        let mut rt = runtime(android_scene);
        rt.update(|s, tx| {
            s.offline = false;
            (s.stylus, s.pen) = (true, Some(pen));
            s.actions.push(Action::Select("pixel_fold".into()));
            poll(s, 0.0, tx);
        });
        assert!(!rt.state.stylus && rt.state.pen.is_none(), "the stylus is off for the next device");
        let start = std::time::Instant::now();
        while sent.lock().expect("sent").len() < 2 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(*sent.lock().expect("sent"), [PenPhase::Hover, PenPhase::Leave]);
    }

    #[test]
    fn landscape_tablets_fit_a_laptop_display() {
        // Phones and portrait tablets keep the full-height screen.
        let tablet = Some(FormFactor::Tablet);
        let phone = fit_window((720, 1280), Some(FormFactor::Phone), false);
        assert_eq!(phone.1, android::SCREEN_POINTS + 110.0);
        assert!(fit_window((1600, 2560), tablet, false).1 == phone.1 && fit_window((1600, 2560), tablet, false).0 < MAX_WINDOW_W);
        // A Pixel Tablet in landscape would be 1828 points wide at full height: it is shorter.
        let (w, h) = fit_window((2560, 1600), tablet, false);
        assert_eq!(w, MAX_WINDOW_W);
        let screen = (w - 612.0, h - 110.0);
        assert!((screen.0 / screen.1 - 1.6).abs() < 1e-3, "the screen keeps its aspect: {screen:?}");
    }

    /// The cap is for tablets: an unfolded Pixel Fold (1.2:1), a landscape automotive screen and
    /// a TV keep the full-height screen and the window they had before it.
    #[test]
    fn only_tablets_are_shown_shorter_to_fit_a_laptop() {
        let full = |(w, h): (u32, u32)| {
            let screen_w = android::SCREEN_POINTS * w as f32 / h as f32;
            (240.0 + 32.0 + screen_w.max(420.0) + 340.0, android::SCREEN_POINTS + 110.0)
        };
        for (size, ff) in [
            ((2208, 1840), FormFactor::Foldable),
            ((1920, 1080), FormFactor::Auto),
            ((1920, 1080), FormFactor::Tv),
            ((720, 1280), FormFactor::Phone),
        ] {
            assert_eq!(fit_window(size, Some(ff), false), full(size), "{ff:?} {size:?}");
        }
        assert_eq!(fit_window((2208, 1840), None, false), full((2208, 1840)), "an unknown form factor keeps its size");
        assert_eq!(fit_window((2560, 1600), Some(FormFactor::Tablet), false).0, MAX_WINDOW_W);
    }

    /// A device with a bezel has a band of stage around its screen: the window budgets it, so
    /// the screen is drawn as tall as it was before the bezel; a landscape tablet keeps its
    /// laptop-sized window instead, its screen narrower by the band.
    #[test]
    fn the_fitted_window_budgets_the_bezel_band() {
        let band = 2.0 * bezel::BAND;
        let phone = fit_window((720, 1280), Some(FormFactor::Phone), true);
        assert_eq!(phone, (612.0 + android::SCREEN_POINTS * 0.5625 + band, android::SCREEN_POINTS + 110.0 + band));
        let tablet = fit_window((2560, 1600), Some(FormFactor::Tablet), true);
        assert_eq!(tablet.0, MAX_WINDOW_W, "a landscape tablet stays within the laptop cap");
        assert_eq!(tablet.1, (MAX_WINDOW_W - 612.0 - band) / 1.6 + 110.0 + band);
        // No bezel, no band: a TV is the window it had.
        assert_eq!(
            fit_window((1920, 1080), Some(FormFactor::Tv), false),
            (612.0 + android::SCREEN_POINTS * 1920.0 / 1080.0, android::SCREEN_POINTS + 110.0)
        );
    }

    /// In the fitted window the screen is drawn at least [`android::SCREEN_POINTS`] tall, bezel or
    /// not (the headless layout has less chrome than the app, so more).
    #[test]
    fn a_phone_with_a_bezel_is_drawn_full_height_in_its_fitted_window() {
        let mut s = init();
        android_scene(&mut s);
        let size = s.screen_size();
        let bezel = s.selected_info().and_then(controls::bezel).is_some();
        assert!(bezel);
        let window = fit_window(size, Some(FormFactor::Phone), bezel);
        let mut rt = Runtime::new(s, view::view, window, Env::new(true, 2.0));
        rt.frame(0.0);
        let ([_, _, w, h], _) = rt.presented("device-screen").expect("screen drawn");
        assert!(h >= android::SCREEN_POINTS - 1.0, "the band took height from the screen: {w}x{h} in {window:?}");
    }

    #[test]
    fn the_gpu_backend_setting_names_either_backend() {
        assert_eq!(gpu_backend(Some("wgpu")), Ok(Some(rune::GpuBackend::Wgpu)));
        assert_eq!(gpu_backend(Some(" WGPU ")), Ok(Some(rune::GpuBackend::Wgpu)));
        assert_eq!(gpu_backend(Some("metal")), Ok(Some(rune::GpuBackend::Metal)));
    }

    /// Unset, empty or misspelt, the window keeps Rune's default (its native Metal backend on macOS).
    #[test]
    fn without_a_valid_gpu_backend_setting_the_default_stays() {
        assert_eq!(gpu_backend(None), Ok(None));
        assert_eq!(gpu_backend(Some("")), Ok(None));
        let err = gpu_backend(Some("vulkan")).expect_err("unknown backend");
        assert!(err.contains("SIMEE_GPU_BACKEND") && err.contains("vulkan"), "{err}");
    }
}
