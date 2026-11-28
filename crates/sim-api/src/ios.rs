//! The iOS backend: every device method, served through the host's simulator service.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use sim_core::debug;
use sim_core::device::{DeviceState, privacy_service};
use sim_core::duo;
use sim_core::form_factor::FormFactor;
use sim_core::geometry::{Point, Size};
use sim_core::gesture::{Sample, ScreenEdge, pace, pace_samples};
use sim_core::hid::{Button, KeyEvent, key_events};
use sim_core::indigo::PointerButton;
use sim_core::pointer::{self, Held, HeldPointer, PointerAction};
use sim_core::reuse::{BootKey, Reuse};
use sim_core::worker::Workers;
use sim_coresim::{Appearance, Context, Device, Direction, Display, Hid, Orientation, StateWatch, StatusBar, WatchGesture};

use crate::params::{
    PointerRequest, button, crown_delta, display, err, need_bool, need_f64, need_str, opt_bool, opt_edge, opt_form_factor, opt_orientation,
    opt_u64, pairs, pencil_samples, pointer_request, remote_swipe_samples, screenshot_result, str_param, strings, swipe_samples,
    tap_samples, touch_sample,
};

/// Shaping proxies, one per device, owned by this (long-running) process.
static PROXIES: std::sync::Mutex<Option<std::collections::HashMap<String, sim_netshape::proxy::Proxy>>> = std::sync::Mutex::new(None);

fn with_proxies<T>(f: impl FnOnce(&mut std::collections::HashMap<String, sim_netshape::proxy::Proxy>) -> T) -> T {
    let mut guard = PROXIES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Default::default))
}

fn profile_from(p: &Value, base: sim_netshape::Profile) -> Result<sim_netshape::Profile, String> {
    let mut profile = match str_param(p, "preset") {
        Some(name) => sim_netshape::Profile::preset(name).ok_or_else(|| format!("unknown preset `{name}`"))?,
        None => base,
    };
    let num = |key: &str| p.get(key).and_then(Value::as_u64);
    if let Some(v) = p.get("offline").and_then(Value::as_bool) {
        profile.offline = v;
    }
    if let Some(v) = num("latency_ms") {
        profile.latency_ms = v as u32;
    }
    if let Some(v) = num("down_kbps") {
        profile.down_kbps = v as u32;
    }
    if let Some(v) = num("up_kbps") {
        profile.up_kbps = v as u32;
    }
    if let Some(v) = num("loss_pct") {
        profile.loss_pct = (v as u32).min(100);
    }
    if let Some(v) = num("port_base") {
        profile.port_base = u16::try_from(v).map_err(err)?;
    }
    if let Some(v) = num("port_count") {
        profile.port_count = u16::try_from(v).map_err(err)?;
    }
    if let Some(rules) = p.get("rules") {
        profile.rules = serde_json::from_value(rules.clone()).map_err(|e| format!("bad rules: {e}"))?;
    }
    Ok(profile)
}

/// A local port nothing listens on.
fn closed_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(err)?;
    l.local_addr().map(|a| a.port()).map_err(err)
}

/// The simulator SDK the device's shaper and proxy tool are built for (its runtime's platform).
fn shaper_sdk(device: &Device) -> Result<sim_netshape::Sdk, String> {
    sim_netshape::Sdk::for_runtime(&device.info().runtime_id)
}

/// The launch environment that injects the device's shaper (built on first use).
fn shaper_env(device: &Device) -> Result<Vec<(String, String)>, String> {
    Ok(sim_netshape::launch_env(&device.info().udid, &sim_netshape::dylib(shaper_sdk(device)?)?))
}

/// The simulator SDKs a set of devices' runtimes need a shaper for, each once, in first-seen
/// order (`boot_many` may mix iPhones and Apple TVs: each needs its own build).
fn shaper_sdks<'a>(runtime_ids: impl IntoIterator<Item = &'a str>) -> Result<Vec<sim_netshape::Sdk>, String> {
    let mut sdks = Vec::new();
    for id in runtime_ids {
        let sdk = sim_netshape::Sdk::for_runtime(id)?;
        if !sdks.contains(&sdk) {
            sdks.push(sdk);
        }
    }
    Ok(sdks)
}

/// Shapers built for a boot plan, one per simulator SDK.
type Shapers = Vec<(sim_netshape::Sdk, std::path::PathBuf)>;

/// Builds (or reuses) one shaper per SDK the runtimes need.
fn build_shapers(runtime_ids: &[String]) -> Result<Shapers, String> {
    shaper_sdks(runtime_ids.iter().map(String::as_str))?.into_iter().map(|sdk| Ok((sdk, sim_netshape::dylib(sdk)?))).collect()
}

/// The shaper among `shapers` for a device on `runtime_id`.
fn shaper_for<'a>(shapers: &'a [(sim_netshape::Sdk, std::path::PathBuf)], runtime_id: &str) -> Result<&'a Path, String> {
    let sdk = sim_netshape::Sdk::for_runtime(runtime_id)?;
    shapers
        .iter()
        .find(|(s, _)| *s == sdk)
        .map(|(_, dylib)| dylib.as_path())
        .ok_or_else(|| format!("no network shaper was built for `{runtime_id}`"))
}

/// Runs the in-device proxy tool; succeeds only if it reports success.
fn device_proxy(device: &Device, args: &[&str]) -> Result<(), String> {
    let tool = sim_netshape::scproxy_tool(shaper_sdk(device)?)?;
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let out = device.exec(&tool.display().to_string(), &args, &[]).map_err(err)?;
    if out.stdout.contains(" ok ") || out.stdout.starts_with("set ok") || out.stdout.starts_with("clear ok") {
        Ok(())
    } else {
        Err(format!("device proxy {}: {}", args.join(" "), out.stdout.lines().next().unwrap_or(out.stderr.trim())))
    }
}

/// Whether the device was booted with the current shaper build in its environment: its
/// SpringBoard (Apple TV: PineBoard) was started with that exact dylib in
/// `DYLD_INSERT_LIBRARIES` (dyld stops a process whose inserted library doesn't load, so a running
/// SpringBoard has it mapped). An older build counts as not injected (so it gets rebooted).
fn boot_injected(device: &Device) -> bool {
    let Ok(dylib) = shaper_sdk(device).and_then(sim_netshape::dylib) else { return false };
    device_tree(&device.info().udid).is_ok_and(|tree| springboard_inserts(&tree, &dylib.display().to_string()))
}

/// Whether the SpringBoard (or PineBoard) among `tree` was started with `dylib` inserted, read
/// from its `KERN_PROCARGS2` environment.
fn springboard_inserts(tree: &[i32], dylib: &str) -> bool {
    let springboard =
        tree.iter().copied().find(|&pid| sim_coresim::procs::proc(pid).is_some_and(|p| matches!(p.name(), "SpringBoard" | "PineBoard")));
    springboard
        .and_then(sim_coresim::procs::procargs)
        .and_then(|buf| sim_debug::process::ProcArgs::parse(&buf))
        .is_some_and(|args| args.inserts(dylib))
}

/// The pid of the device's `launchd_sim` (the root of its process tree).
fn launchd_sim(udid: &str) -> Result<i32, String> {
    sim_coresim::procs::launchd_sim(udid).ok_or_else(|| format!("no launchd_sim for {udid}; is the device booted?"))
}

/// The device's process tree: its `launchd_sim`, then every descendant (read through libproc).
fn device_tree(udid: &str) -> Result<Vec<i32>, String> {
    Ok(sim_coresim::procs::tree(launchd_sim(udid)?))
}

/// `processes`: each process of `tree` as `ps` showed it (read through libproc; a process that
/// exits meanwhile is left out, and so is a zombie, which has no task left to read, where `ps`
/// listed it).
fn processes_in(tree: &[i32]) -> Vec<sim_debug::process::Process> {
    let samples: Vec<sim_debug::process::Sample> = tree.iter().filter_map(|&pid| sample(pid)).collect();
    let now_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    sim_debug::process::tree(&samples, now_s)
}

fn sample(pid: i32) -> Option<sim_debug::process::Sample> {
    let task = sim_coresim::procs::task(pid)?;
    Some(sim_debug::process::Sample {
        pid,
        ppid: task.ppid,
        cpu_usage: task.cpu_usage,
        resident_bytes: task.resident_bytes,
        start_s: task.start_s,
        path: sim_coresim::procs::path(pid).unwrap_or(task.name),
    })
}

/// `connections`: the internet sockets of the processes of `tree`, as `lsof -i` showed them.
fn connections_in(tree: &[i32]) -> Vec<sim_debug::net::Connection> {
    let mut sockets = Vec::new();
    for &pid in tree {
        let found = sim_coresim::procs::sockets(pid);
        if found.is_empty() {
            continue;
        }
        let process = sim_coresim::procs::name(pid).unwrap_or_default();
        sockets.extend(found.into_iter().map(|s| sim_debug::net::Socket {
            pid,
            process: process.clone(),
            fd: s.fd,
            protocol: s.protocol,
            local: s.local,
            remote: s.remote,
            tcp_state: s.tcp_state,
        }));
    }
    sim_debug::net::connections(&sockets)
}

/// `kill`: sends `signal` to `pid` if it is one of `tree`'s processes.
fn kill_in(tree: &[i32], pid: i64, signal: i64) -> Result<Value, String> {
    let Some(pid32) = i32::try_from(pid).ok().filter(|p| tree.contains(p)) else {
        return Err(format!("pid {pid} is not a process of this device"));
    };
    let signal = i32::try_from(signal).map_err(|_| format!("kill {pid} failed: bad signal {signal}"))?;
    sim_coresim::procs::signal(pid32, signal).map(|()| Value::Null).map_err(|e| format!("kill {pid} failed: {e}"))
}

/// The pid `p` names among `tree`'s processes: `pid` itself (it must be one of the device's), or
/// the running app whose launchd label (`XPC_SERVICE_NAME`, read through `KERN_PROCARGS2`) is
/// `bundle_id`'s.
fn app_pid(tree: &[i32], p: &Value) -> Result<i32, String> {
    let pid = match p.get("pid") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64().ok_or("pid must be an integer")?),
    };
    match (str_param(p, "bundle_id"), pid) {
        (Some(_), Some(_)) => Err("give `bundle_id` or `pid`, not both".into()),
        (None, Some(pid)) => {
            i32::try_from(pid).ok().filter(|p| tree.contains(p)).ok_or_else(|| format!("pid {pid} is not a process of this device"))
        }
        (Some(bundle_id), None) => tree
            .iter()
            .copied()
            .find(|&pid| {
                sim_coresim::procs::procargs(pid)
                    .and_then(|buf| sim_debug::process::ProcArgs::parse(&buf))
                    .is_some_and(|a| a.var("XPC_SERVICE_NAME").is_some_and(|l| crate::memtools::is_app_label(l, bundle_id)))
            })
            .ok_or_else(|| format!("`{bundle_id}` isn't running on this device; launch it first")),
        (None, None) => Err("give `bundle_id` or `pid`".into()),
    }
}

/// `mem_leaks`, `mem_heap`, `mem_vmmap`, `mem_history`: the host's tool on one app of the device,
/// within `timeout_s`.
fn mem_tool(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    use crate::memtools::{self, Tool};
    let tool = Tool::for_method(method).ok_or_else(|| format!("`{method}` isn't a memory tool"))?;
    let address = if tool == Tool::History { Some(memtools::address(need_str(p, "address")?)?) } else { None };
    let raw = opt_bool(p, "raw")?.unwrap_or(false);
    let deadline = mem_deadline(p);
    let pid = app_pid(&device_tree(&s.device(p)?.info().udid)?, p)?;
    let out = memtools::run(&tool.program(), &tool.args(pid, address.as_deref()), deadline)?;
    let mut result = memtools::finish(tool, pid, &out, raw)?;
    if let Some(address) = address {
        result["address"] = address.into();
    }
    Ok(result)
}

/// Reboots the device with the shaper in its launchd environment, so every process (including
/// WebKit's networking extension and system daemons) is shaped by the device's profile.
fn boot_inject(device: &Device) -> Result<(), String> {
    if boot_injected(device) {
        return Ok(());
    }
    let env = shaper_env(device)?;
    if device.state() == DeviceState::Booted {
        device.shutdown().map_err(err)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while device.state() != DeviceState::Shutdown && std::time::Instant::now() < deadline {
            sleep(Duration::from_millis(200));
        }
    }
    device.boot_with_env(&env).map_err(err)?;
    boot(device, Duration::from_secs(600))
}

/// Runs one program with the shaper injected first (the device must be booted). A shaper the
/// runtime can't load (a library the runtime lacks) would stop every process the device starts
/// afterwards, `launchctl` too, so injection couldn't be undone without a reboot.
fn shaper_loads(device: &Device, env: &[(String, String)]) -> Result<(), String> {
    let out = device.exec("launchctl", &["getenv".to_owned(), "SIMEE_NETSHAPE".to_owned()], env).map_err(err)?;
    match crate::camera::dyld_failure(&out.stderr) {
        Some(line) => Err(format!("the network shaper doesn't load on this device's runtime: {}", line.trim())),
        None => Ok(()),
    }
}

/// One step of `network_shape`, in the order [`shape_steps`] gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShapeStep {
    /// Check the shaper loads on the device's runtime ([`shaper_loads`]).
    Preflight,
    WriteProfile,
    /// Start the shaping proxy and point the device at it.
    Proxy,
    /// Inject the shaper (`launchd` or `boot`).
    Inject,
}

/// `network_shape`'s steps for `inject_mode` ("none", "launchd" or "boot"), with or without the
/// proxy, on a booted device or not. The preflight comes first, so a shaper the runtime can't load
/// is refused before the profile, proxy or injection changes. It runs only on a booted device:
/// programs run only there, and a shut-down device given "boot" is booted with the shaper.
fn shape_steps(inject_mode: &str, proxy: bool, booted: bool) -> Vec<ShapeStep> {
    let inject = inject_mode != "none";
    let mut steps = Vec::with_capacity(4);
    if inject && booted {
        steps.push(ShapeStep::Preflight);
    }
    steps.push(ShapeStep::WriteProfile);
    if proxy {
        steps.push(ShapeStep::Proxy);
    }
    if inject {
        steps.push(ShapeStep::Inject);
    }
    steps
}

/// Sets or removes device-wide socket-shaper injection for processes started from now on.
fn device_inject(device: &Device, on: bool) -> Result<(), String> {
    let args: Vec<String> = if on {
        let mut a = vec!["setenv".to_owned()];
        for (k, v) in shaper_env(device)? {
            a.extend([k, v]);
        }
        a
    } else {
        vec!["unsetenv".into(), "DYLD_INSERT_LIBRARIES".into()]
    };
    device.exec("launchctl", &args, &[]).map_err(err)?;
    if !on {
        device.exec("launchctl", &["unsetenv".to_owned(), "SIMEE_NETSHAPE".to_owned()], &[]).map_err(err)?;
    }
    Ok(())
}

/// Running audio taps, keyed by capture id (`mute-<udid>` for mutes).
static CAPTURES: std::sync::Mutex<Option<std::collections::HashMap<String, sim_audio::Capture>>> = std::sync::Mutex::new(None);
static NEXT_CAPTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn with_captures<T>(f: impl FnOnce(&mut std::collections::HashMap<String, sim_audio::Capture>) -> T) -> T {
    let mut guard = CAPTURES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Default::default))
}

/// One simulator service connection, created on first use.
pub struct Session {
    /// One connection per device set used (`None`: the default set), each made on first use.
    ctxs: Vec<(Option<String>, Context)>,
    /// The device set of the call in hand ([`Session::use_set`]), or why it can't be used.
    set: Result<Option<String>, String>,
    open: Opener,
}

/// Connects to the simulator service on a device set (`None`: the default set): [`service`], or a
/// test's fake. Input workers and boot threads connect with their session's.
pub(crate) type Opener = Arc<dyn Fn(Option<&str>) -> Result<Context, String> + Send + Sync>;

/// The simulator service on `set`.
fn service(set: Option<&str>) -> Result<Context, String> {
    Context::open(set).map_err(|e| e.to_string())
}

impl Default for Session {
    fn default() -> Session {
        Session::with_opener(Arc::new(service))
    }
}

impl Session {
    /// A session that connects through `open`.
    pub(crate) fn with_opener(open: Opener) -> Session {
        Session { ctxs: Vec::new(), set: Ok(None), open }
    }

    /// The device set the next calls use (#409): a path, `None` for the default set, or why the
    /// call's set can't be used (each method that connects fails with it).
    pub(crate) fn use_set(&mut self, set: Result<Option<String>, String>) {
        self.set = set;
    }

    /// The device set of the call in hand.
    pub(crate) fn set(&self) -> Result<Option<String>, String> {
        self.set.clone()
    }

    /// How this session connects, for a thread of the call's own.
    pub(crate) fn opener(&self) -> Opener {
        self.open.clone()
    }

    fn ctx(&mut self) -> Result<&Context, String> {
        let set = self.set.clone()?;
        let i = match self.ctxs.iter().position(|(s, _)| *s == set) {
            Some(i) => i,
            None => {
                let ctx = (self.open)(set.as_deref())?;
                self.ctxs.push((set, ctx));
                self.ctxs.len() - 1
            }
        };
        Ok(&self.ctxs[i].1)
    }

    fn device(&mut self, p: &Value) -> Result<Device, String> {
        let q = str_param(p, "device").unwrap_or("booted");
        self.ctx()?.device(q).map_err(|e| e.to_string())
    }

    /// The UDID the `device` parameter names: as given when it is one (no lookup), else resolved.
    fn udid(&mut self, p: &Value) -> Result<String, String> {
        let q = str_param(p, "device").unwrap_or("booted");
        if sim_core::device::is_udid(q) {
            return Ok(q.to_ascii_uppercase());
        }
        self.ctx()?.resolve(q).map(|(_, info)| info.udid).map_err(|e| e.to_string())
    }
}

/// From now on, this process's HID sends report when they have left, for [`flush_input`]. Only
/// a process that exits after its input asks (an in-process CLI command): a server never waits.
pub fn track_input() {
    sim_coresim::track_sends();
}

/// Waits up to `timeout` for the HID messages sent since [`track_input`] to leave this process;
/// whether they all did.
pub fn flush_input(timeout: Duration) -> bool {
    sim_coresim::wait_sent(timeout)
}

/// How long `tap` holds the touch without `hold_ms`. UIKit and SwiftUI buttons, WKWebView links
/// and table rows recognised 20 of 20 taps at every hold from 0 ms (down and up sent back to back,
/// as idb does) to 30 ms, quiet and with 32 busy processes on the host (docs/benchmarks.md
/// "iOS input defaults"); 10 ms keeps a margin over that floor while a tap stays one display frame.
pub const TAP_HOLD: Duration = Duration::from_millis(10);
/// How long `home` and `lock` hold without `hold_ms` on an iPhone or iPad ([`button_hold`]): both
/// were recognised 20 of 20 times at every hold from 0 ms to 30 ms, quiet and loaded
/// (docs/benchmarks.md "iOS input defaults").
pub const BUTTON_HOLD: Duration = Duration::from_millis(10);
/// The hold before `BUTTON_HOLD` was measured, kept where it wasn't: the other buttons (side, Siri,
/// Apple Pay, Action, volume), the Siri Remote's keys on Apple TV, the Apple Watch's buttons, and a
/// device whose form factor isn't known.
const UNMEASURED_BUTTON_HOLD: Duration = Duration::from_millis(80);

/// How long `button` holds without `hold_ms` on a device of `form_factor`.
fn button_hold(button: Button, form_factor: Option<FormFactor>) -> Duration {
    match (button, form_factor) {
        (Button::Home | Button::Lock, Some(FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable)) => BUTTON_HOLD,
        _ => UNMEASURED_BUTTON_HOLD,
    }
}
/// The time between key reports when typing or pressing a key, sent against deadlines from the
/// first. A 41-character mixed-case string with punctuation typed the same, autocorrected the same,
/// 10 of 10 times at every gap from 1 ms to 5 ms, quiet and loaded; at 0 ms one run in 10 lost
/// keys (docs/benchmarks.md "iOS input defaults").
pub const KEY_GAP: Duration = Duration::from_millis(3);

/// The methods that send input, each run on its device's input worker.
const INPUT_METHODS: &[&str] =
    &["tap", "swipe", "touch", "rotary", "watch_gesture", "remote_swipe", "button", "type", "key", "pointer", "stylus"];

/// One input worker per device (by UDID), shared by every API connection. A worker keeps its
/// device's HID client and display between calls (the service's objects stay on the thread that
/// made them), runs that device's input in order (main-display `screen` and `screenshot` calls
/// too, so a capture follows the input sent before it), and lets go after five idle minutes.
static INPUT: Workers<String, Input> = Workers::new("simee-input", Duration::from_secs(300));

/// A device's input connection, owned by its worker thread: the device, a watch on its state, and
/// the HID client and main display, reused while the device stays in the same boot.
#[derive(Default)]
struct Input {
    /// The device set the device is in (#409) and how to connect to it: the calling session's.
    set: Option<String>,
    open: Option<Opener>,
    ctx: Option<Context>,
    device: Option<Device>,
    /// State changes the watch has seen.
    changes: Arc<AtomicU64>,
    watch: Option<StateWatch>,
    hid: Reuse<BootKey, Hid>,
    screens: Reuse<BootKey, Screens>,
    /// The buttons `pointer` holds (and where the pointer is while it holds one) in this boot: the
    /// moves of a drag start there instead of being homed.
    pointer: HeldPointer,
}

/// A device's built-in displays, kept by its input worker: the main one and an iPhone Duo's inner
/// one.
struct Screens {
    main: Display,
    inner: Option<Display>,
    /// Each one's framebuffer size once read (main, inner), with the [`TURNS`] it was read at.
    /// Reading it fetches the surface through a remote proxy (~0.15 ms, most of a `screen` call);
    /// a port keeps its surface's size for the boot (rotating turns the picture, not the surface:
    /// 1206x2622 in all four orientations on an iPhone 17 Pro, iOS 27.2), and these displays are
    /// found again, `updateIOPorts` and all, in every new boot.
    sizes: [std::cell::Cell<Option<SizeAt>>; 2],
}

/// A framebuffer size (width, height) and the [`TURNS`] it was read at.
type SizeAt = (u64, (u32, u32));

/// Rotations and folds this process has sent, on any device: a framebuffer size read before one
/// is read again.
static TURNS: AtomicU64 = AtomicU64::new(0);

/// A screen a touch lands on: its size, and the screen ID whose digitizer takes it (`None`: the
/// main digitizer, as before).
type Touched = (Size, Option<u32>);

/// A second built-in screen counts as an iPhone Duo's inner screen only on that device type (as
/// the desktop decides), so no other device's touches change path.
fn inner_for<T>(device_type_id: &str, second: Option<T>) -> Option<T> {
    second.filter(|_| duo::folds(device_type_id))
}

impl Screens {
    fn open(device: &Device) -> Result<Self, String> {
        let (main, inner) = device.builtin_displays().map_err(err)?;
        Ok(Self { main, inner: inner_for(&device.info().device_type_id, inner), sizes: Default::default() })
    }

    /// The framebuffer size of `display`, one of these: read once, then kept until a rotation or
    /// fold ([`TURNS`]). A display with no surface yet is an error, and nothing is kept.
    fn size(&self, display: &Display) -> Result<(u32, u32), String> {
        let slot = &self.sizes[usize::from(!std::ptr::eq(display, &self.main))];
        let turns = TURNS.load(Ordering::Acquire);
        if let Some((at, size)) = slot.get()
            && at == turns
        {
            return Ok(size);
        }
        let size = display.size().map_err(err)?;
        slot.set(Some((turns, size)));
        Ok(size)
    }

    /// The display `which` names and, on a device with two built-in screens, the screen ID of its
    /// digitizer: the main digitizer follows SpringBoard's primary screen there, so an iPhone Duo's
    /// touches name their screen. The screen in use is the main one unless the device has a second
    /// (the iPhone Duo's inner screen) and only that one is lit ([`duo::active`]).
    fn pick(&self, which: duo::Display) -> Result<(&Display, Option<u32>), String> {
        fn addressed(d: &Display) -> Result<(&Display, Option<u32>), String> {
            d.screen_id().map(|id| (d, Some(id))).ok_or_else(|| "the simulator service doesn't say the screen's ID".into())
        }
        let screen = match (which, &self.inner) {
            (duo::Display::CarPlay, _) => return Err("the CarPlay screen isn't a built-in one".into()),
            (_, None) if which == duo::Display::Builtin(duo::Screen::Inner) => {
                return Err("display=inner is an iPhone Duo's inner screen; this device has one built-in screen".into());
            }
            (_, None) => return Ok((&self.main, None)),
            (duo::Display::Builtin(screen), Some(_)) => screen,
            (duo::Display::Active, Some(inner)) => duo::active(self.main.lit(), inner.lit()),
        };
        match (screen, &self.inner) {
            (duo::Screen::Inner, Some(inner)) => addressed(inner),
            _ => addressed(&self.main),
        }
    }
}

impl Input {
    /// Connects through `open` on `set` from now on: the calling session's. A worker that held a
    /// device of another set (the same UDID asked for in the wrong set, say) lets go of everything.
    fn on_set(&mut self, set: Option<String>, open: Opener) {
        if self.set != set {
            *self = Input { set, ..Input::default() };
        }
        self.open = Some(open);
    }

    /// Runs `send` with the device's HID client and, when `screen` is set, its main screen's size.
    /// Both are reused until the device's state changes (a reboot) and dropped, with the device, on
    /// any failure; a device whose boots can't be told apart connects on every call, as before.
    fn run<R>(&mut self, udid: &str, screen: bool, send: impl FnOnce(&Hid, Option<Size>) -> Result<R, String>) -> Result<R, String> {
        let main = screen.then_some(duo::Display::Builtin(duo::Screen::Outer));
        self.run_on(udid, main, |hid, at| send(hid, at.map(|(size, _)| size)))
    }

    /// [`Input::run`] on the built-in screen `screen` names: `send` gets its size and digitizer.
    fn run_on<R>(
        &mut self,
        udid: &str,
        screen: Option<duo::Display>,
        send: impl FnOnce(&Hid, Option<Touched>) -> Result<R, String>,
    ) -> Result<R, String> {
        let result = self.try_run(udid, screen, send);
        if result.is_err() {
            self.device = None;
            self.watch = None;
        }
        result
    }

    /// The key of the device's current boot (none when its boots can't be told apart); the device
    /// is dropped on failure, as in [`Input::run`].
    fn boot(&mut self, udid: &str) -> Result<Option<BootKey>, String> {
        let Self { set, open, ctx, device, changes, watch, .. } = self;
        let result = connect(ctx, || connect_on(open.as_ref(), set.as_deref()), device, changes, watch, udid).map(|(_, key)| key);
        if result.is_err() {
            self.device = None;
            self.watch = None;
        }
        result
    }

    /// Runs `with` on the built-in display `which` names (and the screens it is one of), reused like
    /// the HID client (and dropped, with the device, on any failure).
    fn display<R>(
        &mut self,
        udid: &str,
        which: duo::Display,
        with: impl FnOnce(&Screens, &Display) -> Result<R, String>,
    ) -> Result<R, String> {
        let result = self.try_display(udid, which, with);
        if result.is_err() {
            self.device = None;
            self.watch = None;
        }
        result
    }

    fn try_display<R>(
        &mut self,
        udid: &str,
        which: duo::Display,
        with: impl FnOnce(&Screens, &Display) -> Result<R, String>,
    ) -> Result<R, String> {
        let Self { set, open, ctx, device, changes, watch, screens, .. } = self;
        let (device, key) = connect(ctx, || connect_on(open.as_ref(), set.as_deref()), device, changes, watch, udid)?;
        screens.with(key, || Screens::open(device), |s| with(s, s.pick(which)?.0))
    }

    fn try_run<R>(
        &mut self,
        udid: &str,
        screen: Option<duo::Display>,
        send: impl FnOnce(&Hid, Option<Touched>) -> Result<R, String>,
    ) -> Result<R, String> {
        let Self { set, open, ctx, device, changes, watch, hid, screens, .. } = self;
        let (device, key) = connect(ctx, || connect_on(open.as_ref(), set.as_deref()), device, changes, watch, udid)?;
        let at = match screen {
            Some(which) => Some(screens.with(
                key,
                || Screens::open(device),
                |s| {
                    let (display, id) = s.pick(which)?;
                    let (w, h) = s.size(display)?;
                    Ok((Size::new(f64::from(w), f64::from(h)), id))
                },
            )?),
            None => None,
        };
        hid.with(key, || device.hid().map_err(err), |hid| send(hid, at))
    }
}

/// A worker's connection: through its session's `open` on `set`, else to the service.
fn connect_on(open: Option<&Opener>, set: Option<&str>) -> Result<Context, String> {
    match open {
        Some(open) => open(set),
        None => service(set),
    }
}

/// The worker's device (found and watched on first use) and the key its connections are reused
/// under.
fn connect<'a>(
    ctx: &'a mut Option<Context>,
    open: impl FnOnce() -> Result<Context, String>,
    device: &'a mut Option<Device>,
    changes: &Arc<AtomicU64>,
    watch: &mut Option<StateWatch>,
    udid: &str,
) -> Result<(&'a Device, Option<BootKey>), String> {
    let ctx = match ctx {
        Some(ctx) => ctx,
        None => ctx.insert(open()?),
    };
    let device = match device {
        Some(device) => device,
        None => {
            let found = ctx.device(udid).map_err(err)?;
            let seen = changes.clone();
            *watch = found
                .watch_state(move || {
                    seen.fetch_add(1, Ordering::Release);
                })
                .ok();
            device.insert(found)
        }
    };
    let changes = watch.as_ref().map(|_| changes.load(Ordering::Acquire));
    let key = BootKey::of(device.state() == DeviceState::Booted, changes, device.last_booted_at());
    Ok((device, key))
}

/// Runs `with` on the built-in display `which` names on the device's input worker, which keeps the
/// displays between calls (and orders a capture after the input sent before it).
fn on_display<R: Send + 'static>(
    s: &mut Session,
    p: &Value,
    which: duo::Display,
    with: impl FnOnce(&Screens, &Display) -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let (set, open) = (s.set()?, s.opener());
    let udid = s.udid(p)?;
    let result = INPUT.run(&udid.clone(), move |input| {
        input.on_set(set, open);
        sim_coresim::autoreleasepool(|| input.display(&udid, which, with))
    })?;
    result.map_err(|e| as_typed(e, p))
}

/// An error about the device the `device` parameter names, with the UDID as the caller typed it
/// (workers look devices up by the uppercased UDID).
fn as_typed(error: String, p: &Value) -> String {
    match str_param(p, "device") {
        Some(q) if sim_core::device::is_udid(q) && q != q.to_ascii_uppercase() => {
            error.replace(&format!("`{}`", q.to_ascii_uppercase()), &format!("`{q}`"))
        }
        _ => error,
    }
}

/// The `display` a method names: its size, or a capture. Built-in displays go through the
/// device's worker; the CarPlay screen is looked up per call (attaching it isn't a new boot).
fn screen_size(s: &mut Session, p: &Value) -> Result<(u32, u32), String> {
    match display(p)? {
        duo::Display::CarPlay => s.device(p)?.external_display().and_then(|d| d.size()).map_err(err),
        which => on_display(s, p, which, |screens, d| screens.size(d)),
    }
}

fn capture(s: &mut Session, p: &Value) -> Result<sim_core::frame::Frame, String> {
    match display(p)? {
        duo::Display::CarPlay => s.device(p)?.external_display().and_then(|d| d.capture()).map_err(err),
        which => on_display(s, p, which, |_, d| d.capture().map_err(err)),
    }
}

/// `contrast_check` on a simulator: its screen, its scale, and `appearance`'s two modes.
struct ContrastTarget<'a> {
    s: &'a mut Session,
    p: &'a Value,
}

impl crate::contrast::Target for ContrastTarget<'_> {
    fn capture(&mut self) -> Result<sim_core::frame::Frame, String> {
        capture(self.s, self.p)
    }

    fn scale(&mut self) -> Option<f64> {
        self.s.device(self.p).ok()?.device_type().ok().map(|t| t.screen_scale).filter(|s| *s > 0.0)
    }

    fn set_appearance(&mut self, mode: crate::contrast::Mode) -> Result<(), String> {
        let mode = match mode {
            crate::contrast::Mode::Light => Appearance::Light,
            crate::contrast::Mode::Dark => Appearance::Dark,
        };
        self.s.device(self.p)?.set_appearance(mode).map_err(err)
    }
}

/// The simulators to resolve a `device` across platforms with: what selection reads of each.
pub fn selection_infos(s: &mut Session) -> Result<Vec<sim_core::device::DeviceInfo>, String> {
    s.ctx()?.selection_infos().map_err(err)
}

/// A simulator as the camera links (`crate::camera`, #323) see it.
fn camera_target(d: &Device) -> crate::camera::IosTarget {
    let info = d.info();
    crate::camera::IosTarget {
        udid: info.udid,
        name: info.name,
        data_dir: std::path::PathBuf::from(d.data_path()),
        booted: d.state() == DeviceState::Booted,
    }
}

/// The simulator `query` names, for the camera links.
pub(crate) fn camera_device(s: &mut Session, query: &str) -> Result<crate::camera::IosTarget, String> {
    s.ctx()?.device(query).map(|d| camera_target(&d)).map_err(err)
}

/// Every simulator, for `camera_links` without a device.
pub(crate) fn camera_devices(s: &mut Session) -> Result<Vec<crate::camera::IosTarget>, String> {
    Ok(s.ctx()?.devices().map_err(err)?.iter().map(camera_target).collect())
}

/// Runs the device's own `launchctl` (the runtime's, see AGENTS.md): its stdout and stderr.
pub(crate) fn camera_launchctl(s: &mut Session, udid: &str, args: &[String], env: &[(String, String)]) -> Result<(String, String), String> {
    let out = s.ctx()?.device(udid).map_err(err)?.exec("launchctl", args, env).map_err(err)?;
    Ok((out.stdout, out.stderr))
}

/// Runs an input method on its device's worker.
fn input(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let (set, open) = (s.set()?, s.opener());
    let udid = s.udid(p)?;
    let (method, params) = (method.to_owned(), p.clone());
    let result = INPUT.run(&udid.clone(), move |input| {
        input.on_set(set, open);
        sim_coresim::autoreleasepool(|| input_method(input, &udid, &method, &params))
    })?;
    result.map_err(|e| as_typed(e, p))
}

fn input_method(input: &mut Input, udid: &str, method: &str, p: &Value) -> Result<Value, String> {
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "tap" => play_on(input, udid, p, &tap_samples(p, TAP_HOLD)?, None),
        "swipe" => play_on(input, udid, p, &swipe_samples(p)?, opt_edge(p)?),
        "touch" => {
            let (at, phase) = touch_sample(p)?;
            play_on(input, udid, p, &[Sample { point: at, phase, delay: Duration::ZERO }], opt_edge(p)?)
        }
        "stylus" => {
            let samples = pencil_samples(p)?;
            null(input.run(udid, true, |hid, screen| {
                let screen = screen.ok_or("no screen size")?;
                paced(&samples, |s| hid.pencil(screen.normalize(s.point), s.phase).map_err(err))
            }))
        }
        "pointer" => pointer_on(input, udid, p),
        "rotary" => {
            let delta = crown_delta(p)?;
            input.run(udid, false, |hid, _| hid.crown(delta).map_err(err)).map(|()| json!({"delta": delta}))
        }
        "watch_gesture" => {
            let name = need_str(p, "gesture")?;
            let gesture = WatchGesture::parse(name).ok_or_else(|| format!("unknown gesture `{name}` (double-tap, wrist-flick)"))?;
            null(input.run(udid, false, |hid, _| hid.watch_gesture(gesture).map_err(err)))
        }
        "remote_swipe" => {
            let samples = remote_swipe_samples(p)?;
            null(input.run(udid, false, |hid, _| paced(&samples, |s| hid.remote_touch(s.point, s.phase).map_err(err))))
        }
        "button" => {
            let (button, given) = (button(p, Duration::ZERO)?.0, opt_u64(p, "hold_ms").map(Duration::from_millis));
            null(input.run(udid, false, |hid, _| {
                let hold = given.unwrap_or_else(|| button_hold(button, hid.form_factor()));
                hid.button(button, Direction::Down).map_err(err)?;
                sleep(hold);
                hid.button(button, Direction::Up).map_err(err)
            }))
        }
        "type" => {
            let text = need_str(p, "text")?;
            let keys = key_events(text).map_err(|c| format!("no key for {c:?} on the US layout; nothing was typed"))?;
            null(input.run(udid, false, |hid, _| send_keys(hid, &keys)))
        }
        "key" => {
            let usage = opt_u64(p, "usage").ok_or("missing number parameter `usage`")? as u32;
            let keys = [KeyEvent { usage, direction: Direction::Down }, KeyEvent { usage, direction: Direction::Up }];
            null(input.run(udid, false, |hid, _| send_keys(hid, &keys)))
        }
        other => Err(format!("`{other}` isn't an input method")),
    }
}

/// The CarPlay screen's size: the car display simee attaches.
fn carplay_screen() -> Size {
    Size::new(f64::from(sim_core::carplay::WIDTH), f64::from(sim_core::carplay::HEIGHT))
}

/// Plays samples on a built-in screen: the main digitizer, or the screen `screen_id` names (an
/// iPhone Duo's inner screen).
fn play(hid: &Hid, (screen, screen_id): Touched, samples: &[Sample], edge: Option<ScreenEdge>) -> Result<(), String> {
    paced(samples, |s| {
        let at = screen.normalize(s.point);
        match (screen_id, edge) {
            (Some(id), edge) => hid.screen_touch(id, at, s.phase, edge),
            (None, Some(edge)) => hid.edge_touch(at, s.phase, edge),
            (None, None) => hid.touch(at, s.phase),
        }
        .map_err(err)
    })
}

/// Sends `samples` at their deadlines from the first ([`pace_samples`]), so the time each send
/// takes and each oversleep on a busy host don't add up: a 300 ms swipe stays 300 ms.
fn paced(samples: &[Sample], send: impl FnMut(&Sample) -> Result<(), String>) -> Result<(), String> {
    let started = Instant::now();
    pace_samples(samples, || started.elapsed(), sleep, send)
}

/// Plays touch samples on the screen the `display` parameter names: the one in use (an iPhone
/// Duo's inner screen when it is unfolded), an iPhone Duo's `outer` or `inner` one, or the CarPlay
/// screen; with an `edge`, as a touch that comes in from that edge of the device's screen.
fn play_on(input: &mut Input, udid: &str, p: &Value, samples: &[Sample], edge: Option<ScreenEdge>) -> Result<Value, String> {
    let which = display(p)?;
    let carplay = which == duo::Display::CarPlay;
    if carplay && edge.is_some() {
        return Err("`edge` is for the device's own screen, not CarPlay's".into());
    }
    let played = input.run_on(udid, (!carplay).then_some(which), |hid, screen| match screen {
        Some(screen) => play(hid, screen, samples, edge),
        None => paced(samples, |s| hid.carplay_touch(carplay_screen().normalize(s.point), s.phase).map_err(err)),
    });
    played.map(|()| Value::Null)
}

/// `pointer`: brings the iPad's pointer to a point (pixels) and hovers, clicks, presses or releases
/// a button there, or drags from it to another. The pointer only moves relatively, so it moves on
/// from where the last call on this server in this boot left it, or from where a held button
/// (`down`) keeps it, and is homed against the screen's edges first (`sim_core::pointer`) when
/// neither is known: the first call, after a failure or a rotation, and every
/// [`pointer::REHOME_EVERY`] calls (touches, the only other input, don't move the pointer). A
/// `down` that no later call could see (an in-process run), an `up` with nothing held, and a
/// `move` or `up` after a failed call lost what was held are refused ([`pointer::check`]).
fn pointer_on(input: &mut Input, udid: &str, p: &Value) -> Result<Value, String> {
    let PointerRequest { at, action, button, hold, drag_to } = pointer_request(p)?;
    let boot = input.boot(udid)?;
    let held = input.pointer.get(boot);
    pointer::check(action, drag_to.is_some(), held, crate::serving())?;
    let turns = TURNS.load(Ordering::Acquire);
    let from = input.pointer.start(boot, turns);
    let result = input.run(udid, true, |hid, screen| {
        let screen = screen.ok_or("no screen size")?;
        let scale = if hid.scale() > 0.0 { hid.scale() } else { 1.0 };
        let points = Size::new(screen.width / scale, screen.height / scale);
        let in_points = |p: Point| Point::new(p.x.clamp(0.0, screen.width) / scale, p.y.clamp(0.0, screen.height) / scale);
        let to = in_points(at);
        let mut buttons = match held {
            Held::Down { buttons, .. } => buttons,
            Held::Nothing => 0,
            Held::Lost => {
                // Let go of whatever the guest may hold, where the pointer is, before homing: a
                // held button would drag along the edges.
                for b in [PointerButton::Primary, PointerButton::Secondary] {
                    hid.pointer_button(b, false, 0).map_err(err)?;
                }
                0
            }
        };
        // Against deadlines from the first report, so a busy host doesn't stretch the call; whether
        // a report went out late (a stall), after which where the pointer stopped isn't known.
        let glide = |from: Option<Point>, to: Point, buttons: u32| -> Result<bool, String> {
            let plan = pointer::plan(from, to, points);
            let started = Instant::now();
            let late =
                pointer::send_paced(&plan, || started.elapsed(), sleep, |(dx, dy)| hid.pointer_motion(dx, dy, buttons).map_err(err))?;
            sleep(pointer::INTERVAL);
            Ok(late)
        };
        // Where the pointer is known to be after the call: not after a stall, nor on an edge or in
        // a corner, where the guest may stop it short.
        let known = |at: Point, late: bool| (!late && pointer::lands_at(at, points)).then_some(at);
        let late = glide(from, to, buttons)?;
        let mask = button.mask();
        let left = |at: Point, buttons: u32| if buttons == 0 { Held::Nothing } else { Held::Down { at, buttons } };
        if let Some(end) = drag_to.map(in_points) {
            sleep(pointer::INTERVAL * 3);
            hid.pointer_button(button, true, buttons | mask).map_err(err)?;
            let late = glide(Some(to), end, buttons | mask)? || late;
            sleep(pointer::INTERVAL * 3);
            hid.pointer_button(button, false, buttons).map_err(err)?;
            return Ok((left(end, buttons), known(end, late)));
        }
        match action {
            PointerAction::Move => {}
            PointerAction::Click | PointerAction::Down => {
                // Let the last move land: a press in the same frame can be lost.
                sleep(pointer::INTERVAL * 3);
                buttons |= mask;
                hid.pointer_button(button, true, buttons).map_err(err)?;
                if action == PointerAction::Click {
                    sleep(hold);
                    buttons &= !mask;
                    hid.pointer_button(button, false, buttons).map_err(err)?;
                }
            }
            PointerAction::Up => {
                buttons &= !mask;
                hid.pointer_button(button, false, buttons).map_err(err)?;
            }
        }
        Ok((left(to, buttons), known(to, late)))
    });
    match result {
        Ok((next, at)) => input.pointer.set(boot, next, at, from.is_none(), turns),
        Err(e) => {
            input.pointer.failed(boot, held, drag_to.is_some() || matches!(action, PointerAction::Click | PointerAction::Down));
            return Err(e);
        }
    }
    Ok(Value::Null)
}

/// The orientation simee last set per device (UDID), so `rotate` without one turns 90° from there.
static ORIENTATIONS: std::sync::Mutex<Option<std::collections::HashMap<String, Orientation>>> = std::sync::Mutex::new(None);

fn rotate(device: &Device, wanted: Option<Orientation>) -> Result<Value, String> {
    let udid = device.info().udid;
    let mut guard = ORIENTATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let known = guard.get_or_insert_with(Default::default);
    let next = wanted.unwrap_or_else(|| known.get(&udid).copied().unwrap_or(Orientation::Portrait).clockwise());
    // Counted before and after: a pointer call that starts while the device turns, or reads the
    // count before it, doesn't move on from where the pointer was before.
    TURNS.fetch_add(1, Ordering::AcqRel);
    let sent = device.set_orientation(next);
    TURNS.fetch_add(1, Ordering::AcqRel);
    sent.map_err(err)?;
    known.insert(udid, next);
    Ok(json!({"orientation": next.as_str()}))
}

/// Sends key reports [`KEY_GAP`] apart, against deadlines from the first ([`pace`]), so a busy
/// host doesn't stretch a string (a late report goes at once and the next keeps its deadline).
fn send_keys(hid: &Hid, keys: &[KeyEvent]) -> Result<(), String> {
    let delays = key_delays(keys.len(), KEY_GAP);
    let started = Instant::now();
    pace(
        &delays,
        || started.elapsed(),
        sleep,
        |i| match keys.get(i) {
            Some(k) => hid.key(k.usage, k.direction).map_err(err),
            None => Ok(()),
        },
    )
}

/// The wait before each of `keys` reports and one more `gap` after the last, so the next call's
/// first report doesn't land on it.
fn key_delays(keys: usize, gap: Duration) -> Vec<Duration> {
    (0..=keys).map(|i| if i == 0 { Duration::ZERO } else { gap }).collect()
}

/// Boots (if needed) and waits until the device reports booted and its display has a frame.
fn boot(device: &Device, timeout: Duration) -> Result<(), String> {
    if device.state() != DeviceState::Booted {
        device.boot().map_err(err)?;
    }
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let finished = device.state() == DeviceState::Booted && device.boot_finished().unwrap_or(true);
        if finished && device.display().is_ok_and(|d| d.surface().is_some()) {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    Err(format!("device did not finish booting within {}s", timeout.as_secs()))
}

/// What a boot waits for and changes beyond the service's own "booted" (`boot` and `boot_many`).
#[derive(Clone, Debug)]
struct BootPlan {
    /// For the boot itself (until the service reports it booted and the display has a frame).
    timeout: Duration,
    env: Vec<(String, String)>,
    /// The network shapers to inject, one per simulator SDK among the devices (empty: unshaped),
    /// resolved once by the caller: `boot_many`'s workers must not each build one into the same
    /// cache.
    shapers: Shapers,
    /// Wait until the device is quiet too (see [`crate::settle`]), for at most `settle_timeout`
    /// after the boot.
    settle: Option<crate::settle::Rule>,
    settle_timeout: Duration,
    /// Daemons to turn off once booted (a named profile).
    profile: Option<&'static Profile>,
}

/// A named set of in-device launchd jobs a farm device runs without.
#[derive(Debug)]
pub struct Profile {
    pub name: &'static str,
    /// launchd labels (in the device's `system` domain) disabled and stopped.
    pub disabled: &'static [&'static str],
}

/// Profiles `boot {profile}` applies. Each job is disabled in the device's own launchd database
/// and booted out (stopped now). `boot` of a shut-down device writes the disables into that
/// database before the boot ([`preset_profile`], and the boot's `disabled_jobs` for a runtime
/// that reads them, [`boot_jobs`]), so the jobs never start, and when launchd read them all
/// ([`preset_held`]) nothing is spawned after the boot. A plain reboot keeps
/// the disables on runtimes that read `/private/var/tmp` (iOS 18.6 and newer); on iOS 18.0,
/// whose launchd reads the `/private/tmp` file CoreSimulator writes, any boot without the
/// `disabled_jobs` option deletes it. A clone of the device keeps none (a new UDID, a new database): apply the
/// profile on every boot. Again it touches only the jobs that came back, and `profile {undo}`
/// enables the jobs.
///
/// The paired-watch daemons (the runtime's `NanoLaunchDaemons`: companionappd, NPKCompanionAgent,
/// brookcompaniond, addressbooksyncd, bulletindistributord, pairedsyncd and 11 more) are
/// `Disabled` in their own plists; nanoregistrylaunchd enables all 17 each time it runs (one to
/// three minutes after a boot, measured on iOS 27.2), which is why they came back on every boot.
/// The profile disables nanoregistrylaunchd itself, and the ones that run once enabled.
pub const PROFILES: &[Profile] = &[Profile { name: "slim", disabled: SLIM }];

/// The `slim` profile: daemons that burn CPU or memory on an idle booted device and that no
/// AutoMobile tool needs (measured on iOS 27.2, iPhone 17; see docs/farm.md).
///
/// Kept on purpose: SpringBoard, backboardd, InputUI (the keyboard), AccessibilityUIServer and
/// the accessibility daemons (AutoMobile reads the screen through accessibility), installd,
/// lsd and runningboardd (install, launch), tccd (privacy), usernotificationsd, locationd
/// (`location`), sharingd, webbookmarksd (Safari, `open_url`), assetsd (the photo library),
/// syslogd/logd (`logs`), configd_sim and nsurlsessiond (networking), and what simee's own
/// methods reach in the device: carkitd (`carplay`), biometrickitd (`biometric`), apsd (`push`),
/// securityd (`reset_keychain`), cfprefsd (`defaults_*`, `keyboard_language`), notifyd (`notify`,
/// `memory_warning`), mediaserverd (audio). Also kept though a slim clone spends 1-6 CPU-s on each
/// in its first two minutes: donotdisturbd and liveactivitiesd (notification delivery, Live
/// Activities), AuthenticationServicesAgent (password AutoFill, passkeys), callservicesd
/// (CallKit), calaccessd (EventKit), mobileassetd (keyboard and font assets), identityservicesd
/// and diagnosticd (the log stream).
const SLIM: &[&str] = &[
    // Lock and home screen wallpapers: PosterBoard and the ~15 poster extensions it keeps alive.
    "com.apple.PosterBoard",
    // Widgets.
    "com.apple.chronod",
    // Siri, dictation and suggestions.
    "com.apple.assistantd",
    "com.apple.assistant_cdmd",
    "com.apple.siriactionsd",
    "com.apple.siriinferenced",
    "com.apple.siriknowledged",
    "com.apple.sirittsd",
    "com.apple.siri.context.service",
    "com.apple.corespeechd",
    "com.apple.voicebankingd",
    "com.apple.suggestd",
    "com.apple.textunderstandingd",
    "com.apple.generativeexperiencesd",
    "com.apple.spotlightknowledged.updater",
    "com.apple.parsecd",
    // Photo and media analysis.
    "com.apple.photoanalysisd",
    "com.apple.mediaanalysisd",
    // Health, home, fitness. Not sleepd: without it SpringBoard and mobiletimerd retry
    // `com.apple.sleepd.sleepserver` in a tight loop on every later boot (~350 % of a core, and
    // the device never settles; measured on iOS 27.2).
    "com.apple.healthd",
    "com.apple.homed",
    "com.apple.homeeventsd",
    "com.apple.fitnesscoachingd",
    "com.apple.fitnessintelligenced",
    // Store, iCloud sync, mail, news, weather, wallet, games, finance.
    "com.apple.appstored",
    "com.apple.itunesstored",
    "com.apple.itunescloudd",
    "com.apple.amsengagementd",
    "com.apple.ap.adprivacyd",
    "com.apple.ap.promotedcontentd",
    "com.apple.cloudd",
    "com.apple.email.maild",
    "com.apple.icloudmailagent",
    "com.apple.newsd",
    "com.apple.weatherd",
    "com.apple.passd",
    "com.apple.financed",
    "com.apple.gamed",
    "com.apple.SafariBookmarksSyncAgent",
    "com.apple.followupd",
    "com.apple.remindd",
    // Location history, maps analytics, people and social. Not carkitd: `carplay` needs it.
    "com.apple.routined",
    "com.apple.geoanalyticsd",
    "com.apple.navd",
    "com.apple.peopled",
    "com.apple.sociallayerd",
    // Screen Time.
    "com.apple.ScreenTimeAgent",
    "com.apple.UsageTrackingAgent",
    "com.apple.FamilyControlsAgent",
    // Paired-watch and Find My companions: nanoregistrylaunchd enables the paired-watch daemons
    // (see `PROFILES`); the rest are those that then run on an idle phone.
    "com.apple.nanoregistrylaunchd",
    "com.apple.addressbooksyncd",
    "com.apple.bulletindistributord",
    "com.apple.pairedsyncd",
    "com.apple.NPKCompanionAgent",
    "com.apple.companionappd",
    "com.apple.wcd",
    "com.apple.brook.brookcompaniond",
    "com.apple.findmy.findmylocated",
    // Experiments, ML hosting and asset prefetch.
    "com.apple.triald",
    "com.apple.mlhostd",
    "com.apple.jetpackassetd",
    "com.apple.replicatord",
    "com.apple.diagnosticextensionsd",
    // What a slim clone still spent its first two minutes on (measured per job, iOS 27.2 and
    // 18.6; docs/farm.md). App Intents: linkd runs donotdisturbd's Focus-filter first-launch task,
    // which starts the Calendar, Safari and Messages intents extensions and Calendar itself
    // 13-38 s after the boot (~8 CPU-s, the burst that held the settle window open).
    "com.apple.linkd",
    // Watch faces and settings sync, with no watch paired.
    "com.apple.nanotimekitcompaniond",
    "com.apple.nanoprefsyncd.2",
    // Exchange/CalDAV/CardDAV account sync (no accounts) and Safari's breached-password lookups.
    "com.apple.dataaccess.dataaccessd",
    "com.apple.Safari.passwordbreachd",
    // Fitness: activity sharing, awards, Fitness+ (healthd is off already).
    "com.apple.activitysharingd",
    "com.apple.activityawardsd",
    "com.apple.fitcore",
    // Focus status sharing, Memoji, Apple Intelligence's model catalog, Family Sharing alerts.
    "com.apple.StatusKitAgent",
    "com.apple.avatarsd",
    "com.apple.modelcatalogd",
    "com.apple.familynotification",
];

fn profile(name: &str) -> Result<&'static Profile, String> {
    PROFILES.iter().find(|p| p.name == name).ok_or_else(|| {
        let names: Vec<&str> = PROFILES.iter().map(|p| p.name).collect();
        format!("unknown profile `{name}` ({})", names.join(", "))
    })
}

/// The settle rule from `prefix` + `cpu_pct`, `window_s` and `foreground` (the defaults where
/// absent): `settle_` for `boot` and `boot_many`, none for `settle`.
fn settle_rule(p: &Value, prefix: &str) -> Result<crate::settle::Rule, String> {
    let mut rule = crate::settle::Rule::default();
    let key = |name: &str| format!("{prefix}{name}");
    match p.get(key("cpu_pct")) {
        None | Some(Value::Null) => {}
        Some(v) => {
            rule.cpu_pct =
                v.as_f64().filter(|x| *x >= 0.0).ok_or_else(|| format!("{} must be a number of % of a core, not {v}", key("cpu_pct")))?
        }
    }
    match p.get(key("window_s")) {
        None | Some(Value::Null) => {}
        Some(v) => {
            rule.window = Duration::from_secs(v.as_u64().ok_or_else(|| format!("{} must be whole seconds, not {v}", key("window_s")))?)
        }
    }
    if let Some(fg) = opt_bool(p, &key("foreground"))? {
        rule.foreground = fg;
    }
    Ok(rule)
}

/// `launch`'s `restart`: true (the default) terminates a running instance and cold-starts the
/// app; false leaves a running app as it is.
fn launch_restart(p: &Value) -> Result<bool, String> {
    Ok(opt_bool(p, "restart")?.unwrap_or(true))
}

/// How long a launch SpringBoard refused as being updated is answered from memory before one goes
/// to the device again.
const BEING_UPDATED_RECHECK: Duration = Duration::from_secs(60);

/// Apps whose launch SpringBoard refused as being updated (`LaunchServicesDataMismatch`, #123), by
/// (UDID, bundle id): the boot it happened in (the device's `launchd_sim` pid), when, and the
/// error. SpringBoard retries for about 12.6 s before it refuses and only a reboot is known to clear
/// the state, so a launch of one of them fails at once with that error until the device reboots or
/// [`BEING_UPDATED_RECHECK`] passes.
#[derive(Default)]
struct BeingUpdated(std::collections::HashMap<(String, String), (i32, Instant, String)>);

impl BeingUpdated {
    /// The error to fail this launch with at once, if `bundle` on `udid` was refused in this boot
    /// less than [`BEING_UPDATED_RECHECK`] ago. `boot` (the device's `launchd_sim` pid now) is asked
    /// only for an app listed here, so other launches pay nothing.
    fn refusal(&mut self, udid: &str, bundle: &str, now: Instant, boot: impl FnOnce() -> Option<i32>) -> Option<String> {
        let key = (udid.to_owned(), bundle.to_owned());
        let (seen_boot, at, message) = self.0.get(&key)?;
        let age = now.saturating_duration_since(*at);
        if age < BEING_UPDATED_RECHECK && boot() == Some(*seen_boot) {
            return Some(format!(
                "{message} (simee answered from memory: SpringBoard refused this launch {} s ago in this boot; it asks the \
                 device again after a reboot or {} s)",
                age.as_secs(),
                BEING_UPDATED_RECHECK.as_secs()
            ));
        }
        self.0.remove(&key);
        None
    }

    /// Remembers `error` for `bundle` on `udid` when it is SpringBoard's being-updated refusal and
    /// the device's boot is known.
    fn note(&mut self, udid: &str, bundle: &str, error: &sim_coresim::Error, now: Instant, boot: impl FnOnce() -> Option<i32>) {
        if is_being_updated(error)
            && let Some(boot) = boot()
        {
            self.0.insert((udid.to_owned(), bundle.to_owned()), (boot, now, error.to_string()));
        }
    }
}

/// Whether a launch failed because SpringBoard holds the app as being updated (sim-coresim names
/// `LaunchServicesDataMismatch` in that error alone).
fn is_being_updated(error: &sim_coresim::Error) -> bool {
    matches!(error, sim_coresim::Error::Service { op: "launch", message } if message.contains("LaunchServicesDataMismatch"))
}

static BEING_UPDATED: std::sync::Mutex<Option<BeingUpdated>> = std::sync::Mutex::new(None);

fn with_being_updated<T>(f: impl FnOnce(&mut BeingUpdated) -> T) -> T {
    let mut guard = BEING_UPDATED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(Default::default))
}

/// The plan for `p`, booting `concurrent` devices at once (1 for `boot`); `settle_default` when
/// `settle` isn't given. `shapers` resolves the network shapers the devices need (built on first
/// use), called at most once, here on the caller's thread.
fn boot_plan(
    p: &Value,
    concurrent: usize,
    settle_default: bool,
    shapers: impl FnOnce() -> Result<Shapers, String>,
) -> Result<BootPlan, String> {
    let settle = match p.get("settle") {
        None | Some(Value::Null) => settle_default,
        Some(Value::Bool(b)) => *b,
        Some(other) => return Err(format!("settle must be true or false, not {other}")),
    };
    let shaped = p.get("shaped").and_then(Value::as_bool).unwrap_or(false);
    Ok(BootPlan {
        timeout: Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300)),
        env: pairs(p, "env"),
        shapers: if shaped { shapers()? } else { Vec::new() },
        settle: if settle { Some(settle_rule(p, "settle_")?) } else { None },
        settle_timeout: opt_u64(p, "settle_timeout_s")
            .map_or_else(|| crate::settle::default_settle_timeout(concurrent), Duration::from_secs),
        profile: str_param(p, "profile").map(profile).transpose()?,
    })
}

/// What `boot` (settle off unless asked) or `boot_many` (`many`: settle on unless asked) plans for
/// `p`, as text: what the schema's defaults are checked against.
#[cfg(test)]
pub(crate) fn boot_plan_text(p: &Value, many: bool) -> Result<String, String> {
    boot_plan(p, 1, many, || Ok(Vec::new())).map(|plan| format!("{plan:?}"))
}

/// `settle`'s rule and how long it waits (`timeout_s`, default 600 s).
pub(crate) fn settle_options(p: &Value) -> Result<(crate::settle::Rule, Duration), String> {
    let rule = settle_rule(p, "")?;
    Ok((rule, opt_u64(p, "timeout_s").map_or_else(|| crate::settle::default_settle_timeout(1), Duration::from_secs)))
}

/// `logs`' window, predicate and whether it answers from the device's stream (`stream`, default false).
pub(crate) fn log_options(p: &Value) -> Result<(&str, Option<&str>, bool), String> {
    Ok((str_param(p, "last").unwrap_or("1m"), str_param(p, "predicate"), opt_bool(p, "stream")?.unwrap_or(false)))
}

/// How long a memory tool may run (`timeout_s`, default [`crate::memtools::DEFAULT_TIMEOUT`]).
pub(crate) fn mem_deadline(p: &Value) -> Duration {
    opt_u64(p, "timeout_s").map_or(crate::memtools::DEFAULT_TIMEOUT, Duration::from_secs)
}

/// Boots `device` by `plan`: the boot, then the profile, then the settle wait, each timed. Only
/// a failed boot is an error: a profile or settle wait that fails after it is reported in the
/// timings (`profile.error`; `settled: false` with `settle_error`), the device left booted.
/// `launching` is asked just before the boot is issued for a device that is off at that instant
/// (not booted or booting): `Err` boots it not, and only a device it said yes to is this call's.
fn boot_device(device: &Device, plan: &BootPlan, launching: &dyn Fn() -> Result<(), String>) -> Result<Value, String> {
    let start = std::time::Instant::now();
    let info = device.info();
    let udid = info.udid;
    let mut env = plan.env.clone();
    if !plan.shapers.is_empty() {
        env.extend(sim_netshape::launch_env(&udid, shaper_for(&plan.shapers, &info.runtime_id)?));
    }
    let was_booted = device.state() == DeviceState::Booted;
    // Before the boot, so this boot never starts the profile's jobs (a clone starts with no
    // disables of its own): the preset for a runtime whose launchd reads `/private/var/tmp`, the
    // boot option for one that reads `/private/tmp` (see `sim_core::launchd`).
    let preset = plan.profile.filter(|_| !was_booted).map(|profile| preset_profile(&udid, profile));
    let jobs = plan.profile.filter(|_| !was_booted).map(|profile| boot_jobs(&udid, profile)).unwrap_or_default();
    let boot_started = std::time::SystemTime::now();
    // Read again now: another caller may have booted it since, and then this call only waits.
    if !matches!(device.state(), DeviceState::Booted | DeviceState::Booting) {
        launching()?;
    }
    if !was_booted && (!env.is_empty() || !jobs.is_empty()) {
        device.boot_with(&sim_coresim::BootOptions { env: &env, jobs: &jobs }).map_err(err)?;
    }
    boot(device, plan.timeout)?;
    let boot_s = start.elapsed().as_secs_f64();
    let mut result = json!({"device": udid, "boot_s": round(boot_s), "was_booted": was_booted});
    if let Some(profile) = plan.profile {
        let t = std::time::Instant::now();
        // launchd read every disable as it started: nothing of the profile's was loaded, and
        // nothing to spawn. Otherwise (a device booted already, a runtime that keeps its overrides
        // elsewhere, a label enabled since) launchctl does it.
        let held = if was_booted { None } else { preset_held(&udid, profile, boot_started) };
        let mut r = match held {
            Some(n) => json!({"disabled": n, "already": n, "failed": [], "not_loaded": [], "verified": "file"}),
            None => match apply_profile(device, profile, false) {
                Ok(applied) => applied.to_json(false),
                Err(e) => json!({"error": e}),
            },
        };
        r["name"] = json!(profile.name);
        r["took_s"] = json!(round(t.elapsed().as_secs_f64()));
        match preset {
            Some(Ok(n)) => r["preset"] = json!(n),
            Some(Err(e)) => r["preset_error"] = json!(e),
            None => {}
        }
        result["profile"] = r;
    }
    if let Some(rule) = plan.settle {
        // From the start of the boot call, the profile included.
        let Settled { at, settler } = wait_settled(&udid, rule, plan.settle_timeout, !was_booted);
        if at.is_some() {
            result["settled"] = json!(true);
            result["settle_s"] = json!(round(start.elapsed().as_secs_f64()));
        } else {
            result["settled"] = json!(false);
            result["settle_error"] = json!(not_settled(&settler, rule, plan.settle_timeout));
            result["waited_s"] = json!(round(start.elapsed().as_secs_f64()));
        }
        put_load(&mut result, &settler);
    }
    Ok(result)
}

/// Why a wait that ran out of time didn't settle.
fn not_settled(settler: &crate::settle::Settler, rule: crate::settle::Rule, timeout: Duration) -> String {
    let what = if rule.foreground { " in the foreground" } else { "" };
    let pct = settler.load().map_or_else(|| "no process tree".to_owned(), |l| format!("{:.0} % of a core{what}", l.judged_pct(&rule)));
    format!("device did not settle within {}s ({pct} over the last {}s)", timeout.as_secs(), rule.window.as_secs())
}

/// A settle wait's last figures: processes, and CPU over the window (all of it, and the
/// background-QoS and foreground parts); and the CPU seconds the tree used over the whole wait
/// (from the boot when the wait followed one).
fn put_load(result: &mut Value, settler: &crate::settle::Settler) {
    let load = settler.load();
    let used = settler.used();
    result["cpu_s"] = json!(round(used.total_s));
    result["background_cpu_s"] = json!(round(used.background_s));
    result["procs"] = json!(settler.procs());
    result["cpu_pct"] = json!(load.map(|l| round(l.total_pct)));
    result["background_cpu_pct"] = json!(load.map(|l| round(l.background_pct)));
    result["foreground_cpu_pct"] = json!(load.map(|l| round(l.foreground_pct())));
}

fn round(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// What applying (or undoing) a profile did: the labels now in the wanted state, those that
/// aren't (a `launchctl` that failed, or a label the device's launchd doesn't report so), and
/// those that weren't loaded when it was applied (stopped already, or not in this runtime).
#[derive(Debug, Default, PartialEq)]
struct Applied {
    changed: usize,
    failed: Vec<String>,
    not_loaded: Vec<String>,
    /// Labels that were disabled and not loaded before, so left alone (counted in `changed`).
    already: usize,
}

impl Applied {
    fn to_json(&self, undo: bool) -> Value {
        let key = if undo { "enabled" } else { "disabled" };
        let mut v = json!({(key): self.changed, "failed": self.failed});
        if !undo {
            v["not_loaded"] = json!(self.not_loaded);
            v["already"] = json!(self.already);
        }
        v
    }
}

/// The labels `launchctl print-disabled` reports disabled (`"label" => disabled` or `=> true`).
fn disabled_labels(text: &str) -> std::collections::HashSet<&str> {
    text.lines()
        .filter_map(|l| {
            let (label, state) = l.split_once("=>")?;
            let state = state.trim();
            (state == "disabled" || state == "true").then(|| label.trim().trim_matches('"'))
        })
        .collect()
}

/// Whether launchctl's stderr reports an error. A running job's `disable`/`enable` warns
/// "Warning: Please switch to user/foreground/<label> service identifier" and still works.
fn launchctl_refused(stderr: &str) -> bool {
    stderr.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with("Warning:"))
}

/// What launchctl said while a profile was applied to `labels`: `loaded` is `launchctl list`
/// before (PID, status, label; empty when it couldn't run), `refused` the labels whose
/// `disable`/`enable` failed or printed an error, `disabled` is `launchctl print-disabled system`
/// after. With that listing each label is judged by it, else by `refused` alone.
/// `already` are the labels that were off before and left alone (see [`still_on`]).
fn profile_outcome(labels: &[&str], loaded: &str, already: &[&str], refused: &[&str], disabled: &str, undo: bool) -> Applied {
    let loaded = loaded_labels(loaded);
    let listed = disabled.contains("=>");
    let disabled = disabled_labels(disabled);
    let mut out = Applied { already: already.len(), ..Applied::default() };
    for l in labels {
        if !loaded.is_empty() && !loaded.contains(l) && !already.contains(l) {
            out.not_loaded.push((*l).to_owned());
        }
        if !refused.contains(l) && (!listed || disabled.contains(l) != undo) {
            out.changed += 1;
        } else {
            out.failed.push((*l).to_owned());
        }
    }
    out
}

/// The labels in a `launchctl list` (PID, status, label).
fn loaded_labels(text: &str) -> std::collections::HashSet<&str> {
    text.lines().filter_map(|l| l.split_whitespace().nth(2)).collect()
}

/// The labels of `labels` a profile still has to disable and stop: those `print-disabled`
/// (`disabled`) doesn't list disabled, and those `launchctl list` (`loaded`) shows loaded. The
/// rest are off already and are left alone, so applying a profile again on a later boot costs a
/// spawn or two per job that came back, not two per job. Without either listing, all of them.
/// A job in [`LOADED_WHEN_DISABLED`] that is disabled and loaded but not running is left alone too.
fn still_on<'a>(labels: &[&'a str], loaded: &str, disabled: &str) -> Vec<&'a str> {
    let loaded_set = loaded_labels(loaded);
    if loaded_set.is_empty() || !disabled.contains("=>") {
        return labels.to_vec();
    }
    let running = running_labels(loaded);
    let disabled = disabled_labels(disabled);
    labels
        .iter()
        .copied()
        .filter(|l| !disabled.contains(l) || (loaded_set.contains(l) && (running.contains(l) || !LOADED_WHEN_DISABLED.contains(l))))
        .collect()
}

/// Jobs launchd_sim loads whatever the overrides say ("created the implicit endpoint upfront")
/// and re-creates within 0.25 s of a bootout (measured, iOS 27.2): disabled and not running, they
/// are as off as they get, and stopping them again costs two spawns on every boot for nothing.
const LOADED_WHEN_DISABLED: &[&str] = &["com.apple.siri.context.service"];

/// The labels a `launchctl list` shows running (a PID, not `-`).
fn running_labels(text: &str) -> std::collections::HashSet<&str> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?;
            let label = f.nth(1)?;
            pid.parse::<i64>().is_ok().then_some(label)
        })
        .collect()
}

/// Disables and stops each of the profile's jobs, or with `undo` enables them again (they run
/// from the next boot: an enabled job isn't loaded until launchd bootstraps it). The disables
/// live in the device's launchd database, so a plain reboot keeps most of them; a few jobs come
/// back enabled on every boot (measured, docs/farm.md), so a farm applies the profile on every
/// boot, and a job that is still off costs nothing here.
fn apply_profile(device: &Device, profile: &Profile, undo: bool) -> Result<Applied, String> {
    // The runtime's launchctl (a bare `launchctl` would be the host's, see AGENTS.md). The runtime
    // has no shell, so each call is its own spawn (~0.1-0.4 s), and `exec` reports no exit
    // status: a failed call is one that printed an error, and the effect is read back from
    // launchd's own lists.
    let run = |args: &[&str]| device.exec("launchctl", &args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(), &[]);
    let loaded = if undo { String::new() } else { run(&["list"]).map(|o| o.stdout).unwrap_or_default() };
    let before = if undo { String::new() } else { run(&["print-disabled", "system"]).map(|o| o.stdout).unwrap_or_default() };
    let todo = if undo { profile.disabled.to_vec() } else { still_on(profile.disabled, &loaded, &before) };
    let already: Vec<&str> = profile.disabled.iter().copied().filter(|l| !todo.contains(l)).collect();
    let mut refused = Vec::new();
    for label in &todo {
        let target = format!("system/{label}");
        let changed = run(&[if undo { "enable" } else { "disable" }, &target]);
        if changed.as_ref().map_or(true, |o| launchctl_refused(&o.stderr)) {
            refused.push(*label);
        }
        if !undo {
            // Reports failure in a simulator even when it stops the job: not read.
            let _ = run(&["bootout", &target]);
        }
    }
    // Nothing changed (a device whose disables were preset before its boot): the listing read
    // before is the one after.
    let disabled = if todo.is_empty() { before } else { run(&["print-disabled", "system"]).map_err(err)?.stdout };
    Ok(profile_outcome(profile.disabled, &loaded, &already, &refused, &disabled, undo))
}

/// Writes `profile`'s disables into the shut-down device `udid`'s launchd overrides file (see
/// [`sim_core::launchd`]) before it boots, so that boot never loads the profile's jobs: a clone's
/// first boot then skips starting ~60 daemons and the disable and bootout spawns that would stop
/// them (measured, docs/farm.md). Keeps the file's other overrides; returns how many labels it
/// newly disabled. After the boot [`preset_held`] reads back what launchd used, and
/// [`apply_profile`] does whatever neither this nor the boot option did.
fn preset_profile(udid: &str, profile: &Profile) -> Result<usize, String> {
    preset_disables(&sim_core::launchd::disabled_plist_path(udid), profile.disabled)
}

/// The `disabled_jobs` for a boot of the shut-down device `udid` with `profile`: every label
/// disabled, over the overrides CoreSimulator's file for the device holds from its last boot (on
/// a runtime whose launchd_sim reads that file, iOS 18.0, they are launchd's own, and the boot
/// replaces the file with what it is given). Passed on every such boot: CoreSimulator deletes the
/// file on a boot without it.
fn boot_jobs(udid: &str, profile: &Profile) -> Vec<(String, bool)> {
    boot_jobs_from(&sim_core::launchd::boot_plist_path(udid), profile.disabled)
}

fn boot_jobs_from(path: &Path, labels: &[&str]) -> Vec<(String, bool)> {
    let entries = std::fs::read_to_string(path).ok().and_then(|t| sim_core::launchd::parse(&t)).unwrap_or_default();
    sim_core::launchd::with(&entries, labels, true)
}

/// Whether this boot's launchd_sim (started after `boot_started`) read every one of `profile`'s
/// labels disabled: then none of its jobs was loaded and [`apply_profile`]'s spawns can be
/// skipped. Reads the overrides file in the directory launchd_sim used, the preset's
/// (`/private/var/tmp`) or the boot option's (`/private/tmp`), found by where it logged since the
/// boot. `Some(labels)` when it holds them all; `None` otherwise (a label enabled, no such file
/// or log: a runtime that keeps them elsewhere), and the caller spawns as before.
fn preset_held(udid: &str, profile: &Profile, boot_started: std::time::SystemTime) -> Option<usize> {
    let var_tmp = sim_core::launchd::disabled_plist_path(udid);
    let tmp = sim_core::launchd::boot_plist_path(udid);
    preset_held_in(&var_tmp, &tmp, profile.disabled, boot_started)
}

fn preset_held_in(var_tmp: &Path, tmp: &Path, labels: &[&str], boot_started: std::time::SystemTime) -> Option<usize> {
    let logged = |plist: &Path| std::fs::metadata(plist.with_file_name("launchd.log")).and_then(|m| m.modified()).ok();
    let read = match sim_core::launchd::data_dir_in_use(logged(var_tmp), logged(tmp), boot_started)? {
        sim_core::launchd::DataDir::VarTmp => var_tmp,
        sim_core::launchd::DataDir::Tmp => tmp,
    };
    let entries = sim_core::launchd::parse(&std::fs::read_to_string(read).ok()?)?;
    sim_core::launchd::all_disabled(&entries, labels).then_some(labels.len())
}

/// Disables `labels` in the launchd overrides file at `path` (made, with its directory, when
/// missing); returns how many weren't disabled there already.
fn preset_disables(path: &Path, labels: &[&str]) -> Result<usize, String> {
    use std::os::unix::fs::DirBuilderExt as _;
    let entries = match std::fs::read_to_string(path) {
        Ok(text) => sim_core::launchd::parse(&text).ok_or_else(|| format!("{} isn't a property list simee reads", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let off: std::collections::HashSet<&str> = sim_core::launchd::disabled(&entries).collect();
    let new = labels.iter().filter(|l| !off.contains(*l)).count();
    if new == 0 {
        return Ok(0);
    }
    let dir = path.parent().ok_or("no directory")?;
    // launchd_sim makes it 0700 when it doesn't exist.
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // The directory is under world-writable /private/var/tmp: a unique name, created exclusively.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = dir.join(format!("disabled.plist.simee.{}.{nanos}", std::process::id()));
    write_new(&tmp, dir, sim_core::launchd::render(&entries, labels, true).as_bytes())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })?;
    Ok(new)
}

/// Writes `bytes` to a file that must not exist yet in `dir` (`create_new` is `O_CREAT|O_EXCL`,
/// which never follows a symlink planted at the name), and refuses a `dir` owned by another user:
/// the file just made is ours, so its owner is this process's uid (no libc needed).
fn write_new(path: &Path, dir: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt as _;
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| fail(&e))?;
    let checked = file.metadata().and_then(|m| Ok((m.uid(), std::fs::symlink_metadata(dir)?))).map_err(|e| fail(&e));
    let result = checked.and_then(|(uid, d)| {
        if d.file_type().is_symlink() || d.uid() != uid {
            Err(format!("{} isn't a directory owned by the current user; refusing to write the launchd overrides there", dir.display()))
        } else {
            file.write_all(bytes).map_err(|e| fail(&e))
        }
    });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// A device's processes read through libproc (see [`crate::settle::Procs`]).
struct LibProc;

impl crate::settle::Procs for LibProc {
    fn list(&mut self) -> Vec<crate::settle::Entry> {
        sim_coresim::procs::list()
            .iter()
            .map(|p| crate::settle::Entry { pid: p.pid, ppid: p.ppid, springboard: p.name() == "SpringBoard" })
            .collect()
    }

    fn cpu(&mut self, pid: i32) -> Option<crate::settle::Cpu> {
        sim_coresim::procs::usage(pid)
            .map(|u| crate::settle::Cpu { total_s: u.cpu_ns as f64 / 1e9, background_s: u.background_ns as f64 / 1e9 })
    }
}

/// How a settle wait ended: `at` seconds after it started, or `None` when it ran out of time;
/// the settler holds the last CPU figure and process count either way.
struct Settled {
    at: Option<f64>,
    settler: crate::settle::Settler,
}

/// Waits until the booted device `udid` is settled by `rule`, reading its process tree (its
/// `launchd_sim` and descendants, nothing else on the host) once a second. `since_boot`: the
/// device was just booted, so the CPU its tree used before the first sample counts too.
fn wait_settled(udid: &str, rule: crate::settle::Rule, timeout: Duration, since_boot: bool) -> Settled {
    let start = std::time::Instant::now();
    let mut settler = if since_boot { crate::settle::Settler::since_boot(rule) } else { crate::settle::Settler::new(rule) };
    // Found by a scan of the host's processes, then reused while it is still a launchd_sim.
    let mut root: Option<i32> = None;
    loop {
        if root.is_none_or(|r| !sim_coresim::procs::is_launchd_sim(r)) {
            root = sim_coresim::procs::launchd_sim(udid);
        }
        let t = start.elapsed().as_secs_f64();
        if let Some(tree) = root.and_then(|r| crate::settle::tree(&mut LibProc, r))
            && settler.push(t, tree)
        {
            return Settled { at: Some(t), settler };
        }
        if start.elapsed() >= timeout {
            return Settled { at: None, settler };
        }
        sleep(Duration::from_secs(1));
    }
}

/// A device set and how to connect to it, for threads of a call's own.
type SetOpener = (Option<String>, Opener);

/// Boots `udids` with at most `concurrency` boots (and settle waits) in flight; one result per
/// device, in order: the `boot_device` timings, or `{device, error}`. `hook` is asked before each
/// device starts (one it stops gets its reason as the error) and told as each finishes.
fn boot_many(udids: &[String], concurrency: usize, plan: &BootPlan, hook: &dyn crate::progress::BootHook, on: &SetOpener) -> Vec<Value> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(vec![Value::Null; udids.len()]);
    let start = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency.clamp(1, udids.len().max(1)) {
            scope.spawn(|| {
                // Each thread has its own service connection: the service's objects stay on the
                // thread that made them.
                let ctx = (on.1)(on.0.as_deref());
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(udid) = udids.get(i) else { break };
                    if let Err(why) = hook.start(i) {
                        results.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[i] = json!({"device": udid, "error": why});
                        continue;
                    }
                    let queued_s = start.elapsed().as_secs_f64();
                    let result = sim_coresim::autoreleasepool(|| {
                        let device = ctx.as_ref().map_err(Clone::clone)?.device(udid).map_err(err)?;
                        boot_device(&device, plan, &|| hook.launching(i))
                    });
                    let mut value = result.unwrap_or_else(|e| json!({"device": udid, "error": e}));
                    value["started_s"] = json!(round(queued_s));
                    value["done_s"] = json!(round(start.elapsed().as_secs_f64()));
                    hook.finished(i, &value);
                    results.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[i] = value;
                }
            });
        }
    });
    results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `boot_many` of `p`'s `devices`, with `hook` asked before each device starts and told as each
/// finishes.
pub(crate) fn boot_many_hooked(s: &mut Session, p: &Value, hook: &dyn crate::progress::BootHook) -> Result<Value, String> {
    let udids = strings(p, "devices");
    if udids.is_empty() {
        return Err("give `devices`: a list of UDIDs or names".into());
    }
    let concurrency = crate::params::concurrency(p)?;
    // Settles by default: the slot is held through the settle wait, so K bounds the boot
    // storms in flight (what the K sweep measured), not just the service's boot calls.
    let mut exact = Vec::with_capacity(udids.len());
    let mut runtimes = Vec::with_capacity(udids.len());
    for q in &udids {
        let (_, info) = s.ctx()?.resolve(q).map_err(err)?;
        exact.push(info.udid);
        runtimes.push(info.runtime_id);
    }
    let plan = boot_plan(p, concurrency.min(udids.len()), true, || build_shapers(&runtimes))?;
    let start = std::time::Instant::now();
    let results = boot_many(&exact, concurrency, &plan, hook, &(s.set()?, s.opener()));
    let failed = results.iter().filter(|r| r.get("error").is_some()).count();
    let unsettled = results.iter().filter(|r| r["settled"] == json!(false)).count();
    Ok(json!({"concurrency": concurrency, "all_s": round(start.elapsed().as_secs_f64()), "failed": failed,
        "unsettled": unsettled, "devices": results}))
}

/// Rejects a `create` whose device type isn't the requested form factor. CarPlay (auto) is valid
/// for iOS but isn't a device type of its own.
fn check_form_factor(ctx: &Context, device_type: &str, form_factor: FormFactor) -> Result<(), String> {
    if form_factor == FormFactor::Auto {
        return Err("CarPlay is a display attached to an iPhone simulator, not a device type: create an iPhone, boot it, then call \
                    `carplay` with on=true (iOS 26 runtimes)"
            .into());
    }
    let types = ctx.device_types().map_err(err)?;
    let actual = types.iter().find(|t| t.name == device_type || t.identifier == device_type).and_then(|t| t.form_factor);
    match actual {
        Some(actual) if actual != form_factor => {
            Err(format!("device type `{device_type}` is a {}, not a {}", actual.as_str(), form_factor.as_str()))
        }
        _ => Ok(()),
    }
}

/// Dispatches one device method.
pub fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let ok = |r: sim_coresim::Result<()>| r.map(|()| Value::Null).map_err(err);
    match method {
        "devices" => {
            let booted = p.get("booted").and_then(Value::as_bool).unwrap_or(false);
            let list: Vec<_> =
                s.ctx()?.devices().map_err(err)?.iter().map(Device::info).filter(|d| !booted || d.state == DeviceState::Booted).collect();
            serde_json::to_value(list).map_err(err)
        }
        "runtimes" => serde_json::to_value(s.ctx()?.runtimes().map_err(err)?).map_err(err),
        "device_types" => serde_json::to_value(s.ctx()?.device_types().map_err(err)?).map_err(err),
        "create" => {
            let device_type = need_str(p, "device_type")?;
            if let Some(form_factor) = opt_form_factor(p)? {
                check_form_factor(s.ctx()?, device_type, form_factor)?;
            }
            let d = s.ctx()?.create(need_str(p, "name")?, device_type, need_str(p, "runtime")?).map_err(err)?;
            serde_json::to_value(d.info()).map_err(err)
        }
        "delete" => {
            let d = s.device(p)?;
            let udid = d.info().udid;
            s.ctx()?.delete(&d).map_err(err)?;
            crate::camera::ios_stopped(&udid);
            // The service leaves launchd_sim's directory (its log, ~1-3 MB, and the disables)
            // behind, and the one it wrote a boot's `disabled_jobs` to (launchd's own on iOS
            // 18.0); both are keyed by the UDID, so nothing reads them again.
            if !udid.is_empty() {
                for plist in [sim_core::launchd::disabled_plist_path(&udid), sim_core::launchd::boot_plist_path(&udid)] {
                    if let Some(dir) = plist.parent() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
            }
            Ok(Value::Null)
        }
        "boot" => {
            let d = s.device(p)?;
            let plan = boot_plan(p, 1, false, || build_shapers(&[d.info().runtime_id]))?;
            let timed = boot_device(&d, &plan, &crate::progress::launching)?;
            // The device's saved camera links (#323): its feed and the device-wide injection.
            crate::camera::ios_booted(s, &d.info().udid);
            // Without settle or a profile, `boot` answers as it always has.
            Ok(if plan.settle.is_some() || plan.profile.is_some() { timed } else { Value::Null })
        }
        "boot_many" => boot_many_hooked(s, p, &crate::progress::Unwatched),
        "settle" => {
            let d = s.device(p)?;
            if d.state() != DeviceState::Booted {
                return Err(format!("{} isn't booted", d.info().name));
            }
            let (rule, timeout) = settle_options(p)?;
            let Settled { at, settler } = wait_settled(&d.info().udid, rule, timeout, false);
            let settle_s = at.ok_or_else(|| not_settled(&settler, rule, timeout))?;
            let mut result = json!({"settle_s": round(settle_s)});
            put_load(&mut result, &settler);
            Ok(result)
        }
        "profile" => {
            let d = s.device(p)?;
            if d.state() != DeviceState::Booted {
                return Err(format!("{} isn't booted", d.info().name));
            }
            let profile = profile(need_str(p, "name")?)?;
            let undo = p.get("undo").and_then(Value::as_bool).unwrap_or(false);
            let mut r = apply_profile(&d, profile, undo)?.to_json(undo);
            r["name"] = json!(profile.name);
            Ok(r)
        }
        "clone" => {
            let template = s.device(p)?;
            if template.state() != DeviceState::Shutdown {
                return Err(format!("{} must be shut down to clone it", template.info().name));
            }
            let d = s.ctx()?.clone_device(&template, need_str(p, "name")?).map_err(err)?;
            serde_json::to_value(d.info()).map_err(err)
        }
        // A device that stops (or is erased, which removes its socket) takes its camera feed with it.
        "shutdown" | "erase" => {
            let d = s.device(p)?;
            ok(if method == "shutdown" { d.shutdown() } else { d.erase() })?;
            crate::camera::ios_stopped(&d.info().udid);
            Ok(Value::Null)
        }
        "screen" => {
            let (w, h) = screen_size(s, p)?;
            Ok(json!({"width": w, "height": h}))
        }
        // Captured on the device's worker, encoded here: the worker is free for input meanwhile.
        "screenshot" => screenshot_result(&capture(s, p)?, p),
        "contrast_check" => crate::contrast::run(&mut ContrastTarget { s, p }, p),
        m if INPUT_METHODS.contains(&m) => input(s, m, p),
        "rotate" => rotate(&s.device(p)?, opt_orientation(p)?),
        "fold" => {
            let device = s.device(p)?;
            TURNS.fetch_add(1, Ordering::AcqRel);
            let folded = crate::duo::fold(&device, p);
            TURNS.fetch_add(1, Ordering::AcqRel);
            folded
        }
        "carplay" => {
            let on = need_bool(p, "on")?;
            s.device(p)?.set_carplay(on).map_err(err)?;
            let size = carplay_screen();
            Ok(if on { json!({"display": "carplay", "width": size.width, "height": size.height}) } else { Value::Null })
        }
        "open_url" => ok(s.device(p)?.open_url(need_str(p, "url")?)),
        "install" => ok(s.device(p)?.install(need_str(p, "path")?)),
        "uninstall" => ok(s.device(p)?.uninstall(need_str(p, "bundle_id")?)),
        "launch" => {
            let d = s.device(p)?;
            let bundle = need_str(p, "bundle_id")?;
            let udid = d.udid();
            let boot = || sim_coresim::procs::launchd_sim(&udid);
            if let Some(refused) = with_being_updated(|b| b.refusal(&udid, bundle, Instant::now(), boot)) {
                return Err(refused);
            }
            let mut env = pairs(p, "env");
            if p.get("shaped").and_then(Value::as_bool).unwrap_or(false) {
                env.extend(shaper_env(&d)?);
            }
            if opt_bool(p, "malloc_stack_logging")?.unwrap_or(false) {
                env.push(("MallocStackLogging".into(), "1".into()));
            }
            let pid = d
                .launch_with(bundle, &strings(p, "args"), &env, launch_restart(p)?)
                .inspect_err(|e| with_being_updated(|b| b.note(&udid, bundle, e, Instant::now(), boot)))
                .map_err(err)?;
            Ok(json!({"pid": pid}))
        }
        "terminate" => ok(s.device(p)?.terminate(need_str(p, "bundle_id")?)),
        "apps" => serde_json::to_value(s.device(p)?.installed_apps().map_err(err)?).map_err(err),
        "appearance" => {
            let mode = match need_str(p, "mode")? {
                "light" => Appearance::Light,
                "dark" => Appearance::Dark,
                other => return Err(format!("unknown mode `{other}` (light or dark)")),
            };
            ok(s.device(p)?.set_appearance(mode))
        }
        "location" => ok(s.device(p)?.set_location(need_f64(p, "latitude")?, need_f64(p, "longitude")?)),
        "privacy" => {
            let granted = match need_str(p, "action")? {
                "grant" => true,
                "revoke" => false,
                other => return Err(format!("unknown action `{other}`")),
            };
            let service = privacy_service(need_str(p, "service")?).ok_or("unknown privacy service")?;
            ok(s.device(p)?.set_privacy(&service, need_str(p, "bundle_id")?, granted))
        }
        "push" => {
            let payload = p.get("payload").ok_or("missing parameter `payload`")?;
            ok(s.device(p)?.push(need_str(p, "bundle_id")?, &payload.to_string()))
        }
        "status_bar" => {
            let pick = |key: &str, table: &[&str]| -> Result<Option<u64>, String> {
                match str_param(p, key) {
                    Some(name) => debug::index_of(table, name).map(Some).ok_or_else(|| format!("unknown {key} `{name}`")),
                    None => Ok(None),
                }
            };
            let bars = |key: &str| opt_u64(p, key).unwrap_or(3);
            let overrides = StatusBar {
                time: str_param(p, "time").map(str::to_owned),
                data_network: pick("data_network", debug::DATA_NETWORKS)?,
                wifi: pick("wifi_mode", debug::RADIO_MODES)?.map(|m| (m, bars("wifi_bars"))),
                cellular: pick("cellular_mode", debug::RADIO_MODES)?.map(|m| (m, bars("cellular_bars"))),
                operator: str_param(p, "operator").map(str::to_owned),
                battery: pick("battery_state", debug::BATTERY_STATES)?.map(|st| (st, opt_u64(p, "battery_level").unwrap_or(100))),
            };
            ok(s.device(p)?.override_status_bar(&overrides))
        }
        "clear_status_bar" => ok(s.device(p)?.clear_status_bar()),
        "content_size" => {
            let size = p.get("size").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).ok_or("missing parameter `size`")?;
            let category = debug::content_size(&size).ok_or_else(|| format!("unknown size `{size}`"))?;
            ok(s.device(p)?.set_content_size(category))
        }
        "increase_contrast" => ok(s.device(p)?.set_increase_contrast(need_bool(p, "on")?)),
        "dynamic_island" => ok(s.device(p)?.set_dynamic_island_suppressed(need_bool(p, "suppressed")?)),
        "backlight" => ok(s.device(p)?.set_backlight(need_bool(p, "on")?)),
        "keyboard_language" => ok(s.device(p)?.set_keyboard_language(need_str(p, "language")?)),
        "hardware_keyboard" => ok(s.device(p)?.set_hardware_keyboard(need_bool(p, "on")?)),
        "ca_debug" => {
            let option =
                p.get("option").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).ok_or("missing parameter `option`")?;
            let id = debug::ca_option(&option).ok_or_else(|| format!("unknown option `{option}`"))?;
            let on = need_bool(p, "on")?;
            ok(s.device(p)?.set_ca_debug(id, on))
        }
        "ca_debug_state" => {
            let d = s.device(p)?;
            Ok(debug::CA_OPTIONS
                .iter()
                .map(|(id, key, label)| json!({"id": id, "option": key, "label": label, "on": d.ca_debug(*id)}))
                .collect())
        }
        "reset_keychain" => ok(s.device(p)?.reset_keychain()),
        "memory_warning" => ok(s.device(p)?.memory_warning()),
        "notify" => {
            let d = s.device(p)?;
            let name = need_str(p, "name")?;
            if let Some(state) = opt_u64(p, "state") {
                d.set_notification_state(name, state).map_err(err)?;
            }
            ok(d.post_notification(name))
        }
        "biometric" => {
            let d = s.device(p)?;
            let face = p.get("face").and_then(Value::as_bool).unwrap_or(true);
            match need_str(p, "action")? {
                "match" => ok(d.post_notification(debug::biometric_notification(face, true))),
                "nomatch" => ok(d.post_notification(debug::biometric_notification(face, false))),
                action @ ("enroll" | "unenroll") => {
                    d.set_notification_state(debug::BIOMETRIC_ENROLLMENT, u64::from(action == "enroll")).map_err(err)?;
                    ok(d.post_notification(debug::BIOMETRIC_ENROLLMENT))
                }
                other => Err(format!("unknown action `{other}`")),
            }
        }
        "exec" => {
            let out = s.device(p)?.exec(need_str(p, "program")?, &strings(p, "args"), &pairs(p, "env")).map_err(err)?;
            Ok(json!({"pid": out.pid, "stdout": out.stdout, "stderr": out.stderr}))
        }
        "logs" => logs(s, p),
        "processes" => serde_json::to_value(processes_in(&device_tree(&s.device(p)?.info().udid)?)).map_err(err),
        "kill" => {
            let pid = p.get("pid").and_then(Value::as_i64).ok_or("missing number parameter `pid`")?;
            let signal = p.get("signal").and_then(Value::as_i64).unwrap_or(15);
            kill_in(&device_tree(&s.device(p)?.info().udid)?, pid, signal)
        }
        "connections" => serde_json::to_value(connections_in(&device_tree(&s.device(p)?.info().udid)?)).map_err(err),
        "traffic" => serde_json::to_value(sim_debug::net::traffic(&device_tree(&s.device(p)?.info().udid)?)?).map_err(err),
        "mem_leaks" | "mem_heap" | "mem_vmmap" | "mem_history" => mem_tool(s, method, p),
        "containers" => {
            let root = s.device(p)?.data_path();
            serde_json::to_value(sim_debug::storage::apps(Path::new(&root), p.get("sizes").and_then(Value::as_bool).unwrap_or(false)))
                .map_err(err)
        }
        "ls" => {
            let root = s.device(p)?.data_path();
            serde_json::to_value(sim_debug::storage::list(Path::new(&root), str_param(p, "path").unwrap_or(""))?).map_err(err)
        }
        "read" => {
            let root = s.device(p)?.data_path();
            let bytes = sim_debug::storage::read(Path::new(&root), need_str(p, "path")?)?;
            Ok(
                json!({"bytes": bytes.len(), "base64": base64::engine::general_purpose::STANDARD.encode(&bytes), "text": String::from_utf8(bytes).ok()}),
            )
        }
        "write" => {
            let root = s.device(p)?.data_path();
            let bytes = match (str_param(p, "text"), str_param(p, "base64")) {
                (Some(t), _) => t.as_bytes().to_vec(),
                (None, Some(b)) => base64::engine::general_purpose::STANDARD.decode(b).map_err(err)?,
                (None, None) => return Err("give `text` or `base64`".into()),
            };
            sim_debug::storage::write(Path::new(&root), need_str(p, "path")?, &bytes).map(|()| Value::Null)
        }
        "rm" => {
            let root = s.device(p)?.data_path();
            sim_debug::storage::remove(Path::new(&root), need_str(p, "path")?).map(|()| Value::Null)
        }
        "clear_container" => {
            let root = s.device(p)?.data_path();
            let rel = need_str(p, "path")?;
            if !rel.trim_start_matches('/').starts_with("Containers/") {
                return Err("only paths under Containers/ can be cleared".into());
            }
            for entry in sim_debug::storage::list(Path::new(&root), rel)? {
                sim_debug::storage::remove(Path::new(&root), &format!("{}/{}", rel.trim_end_matches('/'), entry.name))?;
            }
            Ok(Value::Null)
        }
        "du" => {
            let root = s.device(p)?.data_path();
            let path = sim_debug::storage::confine(Path::new(&root), str_param(p, "path").unwrap_or(""))?;
            Ok(json!({"bytes": sim_debug::storage::size(&path)}))
        }
        "defaults_read" => {
            let out = s.device(p)?.exec("defaults", &["read".to_owned(), need_str(p, "domain")?.to_owned()], &[]).map_err(err)?;
            if out.stdout.is_empty() { Err(out.stderr.trim().to_owned()) } else { Ok(Value::String(out.stdout)) }
        }
        "defaults_write" => {
            let ty = match str_param(p, "type").unwrap_or("string") {
                "string" => "-string",
                "int" => "-int",
                "float" => "-float",
                "bool" => "-bool",
                other => return Err(format!("unknown type `{other}`")),
            };
            let value =
                p.get("value").map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).ok_or("missing parameter `value`")?;
            let args = ["write".to_owned(), need_str(p, "domain")?.to_owned(), need_str(p, "key")?.to_owned(), ty.to_owned(), value];
            let out = s.device(p)?.exec("defaults", &args, &[]).map_err(err)?;
            if out.stderr.trim().is_empty() || !out.stderr.contains("rror") { Ok(Value::Null) } else { Err(out.stderr.trim().to_owned()) }
        }
        "crash_reports" => serde_json::to_value(sim_debug::storage::crash_reports(&s.device(p)?.info().udid)).map_err(err),
        "info" => {
            let d = s.device(p)?;
            let info = d.info();
            Ok(json!({"device": info, "data_path": d.data_path(), "runtime_root": d.runtime_root().ok()}))
        }
        "network_shape" => {
            let d = s.device(p)?;
            let udid = d.info().udid;
            let profile = profile_from(p, sim_netshape::read_profile(&udid))?;
            let use_proxy = p.get("proxy").and_then(Value::as_bool).unwrap_or(true);
            // inject: false | true (launchd setenv: new processes) | "boot" (reboot: every process).
            let inject_mode = match p.get("inject") {
                Some(Value::Bool(true)) => "launchd",
                Some(Value::String(m)) if m == "boot" || m == "launchd" => m.as_str(),
                Some(Value::String(m)) if m == "none" => "none",
                None | Some(Value::Bool(false)) => "none",
                Some(other) => return Err(format!("inject must be true, false, \"launchd\" or \"boot\", not {other}")),
            };
            let inject = inject_mode != "none";
            let (mut path, mut port) = (std::path::PathBuf::new(), None);
            for step in shape_steps(inject_mode, use_proxy, d.state() == DeviceState::Booted) {
                match step {
                    ShapeStep::Preflight => shaper_loads(&d, &shaper_env(&d)?)?,
                    ShapeStep::WriteProfile => path = sim_netshape::write_profile(&udid, &profile)?,
                    ShapeStep::Proxy => {
                        let started = with_proxies(|m| -> Result<u16, String> {
                            if let Some(px) = m.get(&udid) {
                                return Ok(px.port);
                            }
                            let px = sim_netshape::proxy::Proxy::start(path.clone()).map_err(err)?;
                            let port = px.port;
                            m.insert(udid.clone(), px);
                            Ok(port)
                        })?;
                        // Offline points the device at a closed port: clients then fail exactly as
                        // with no network ("could not connect to the server"), not a proxy error.
                        let target = if profile.offline { closed_port()? } else { started };
                        device_proxy(&d, &["set", "127.0.0.1", &target.to_string()])?;
                        port = Some(started);
                    }
                    ShapeStep::Inject if inject_mode == "boot" => boot_inject(&d)?,
                    ShapeStep::Inject => device_inject(&d, true)?,
                }
            }
            Ok(json!({"profile": profile, "proxy_port": port, "injected": inject, "inject_mode": inject_mode, "profile_path": path}))
        }
        "network_state" => {
            let udid = s.device(p)?.info().udid;
            let port = with_proxies(|m| m.get(&udid).map(|px| px.port));
            let inject = s
                .device(p)?
                .exec("launchctl", &["getenv".to_owned(), "DYLD_INSERT_LIBRARIES".to_owned()], &[])
                .map(|o| o.stdout.contains("netshape"))
                .unwrap_or(false);
            Ok(
                json!({"profile": sim_netshape::read_profile(&udid), "proxy_port": port, "injected": inject, "boot_injected": boot_injected(&s.device(p)?)}),
            )
        }
        "network_flows" => {
            let udid = s.device(p)?.info().udid;
            with_proxies(|m| m.get(&udid).map(|px| serde_json::to_value(px.flows()).map_err(err)))
                .unwrap_or_else(|| Err("no shaping proxy for this device; call network_shape".into()))
        }
        "network_clear" => {
            let d = s.device(p)?;
            let udid = d.info().udid;
            sim_netshape::write_profile(&udid, &sim_netshape::Profile::default())?;
            let proxy = device_proxy(&d, &["clear"]);
            let inject = device_inject(&d, false);
            with_proxies(|m| m.remove(&udid));
            proxy.and(inject).map(|()| Value::Null)
        }
        "audio_state" => {
            let d = s.device(p)?;
            let settings = sim_audio::settings(Path::new(&d.data_path())).unwrap_or(Value::Null);
            let udid = d.info().udid;
            let muted = with_captures(|m| m.contains_key(&format!("mute-{udid}")));
            Ok(json!({"settings": settings, "muted": muted, "host_devices": sim_audio::host_devices().unwrap_or(Value::Null)}))
        }
        "audio_volume" => {
            let level = opt_u64(p, "level").ok_or("missing number parameter `level`")?.min(100) as u8;
            sim_audio::set_volume(Path::new(&s.device(p)?.data_path()), level).map(|()| Value::Null)
        }
        "audio_route" => {
            let output = match need_str(p, "scope")? {
                "output" => true,
                "input" => false,
                other => return Err(format!("scope must be input or output, not `{other}`")),
            };
            sim_audio::set_route(Path::new(&s.device(p)?.data_path()), output, need_str(p, "uid")?).map(|()| Value::Null)
        }
        "audio_capture_start" => {
            let udid = s.device(p)?.info().udid;
            let pid = launchd_sim(&udid)?;
            let path = str_param(p, "path").map(std::path::PathBuf::from);
            let mute = p.get("mute").and_then(Value::as_bool).unwrap_or(false);
            let cap = sim_audio::Capture::start(pid, path.as_deref(), mute)?;
            let id = format!("cap-{}", NEXT_CAPTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            let started = cap.started.clone();
            with_captures(|m| m.insert(id.clone(), cap));
            Ok(json!({"capture_id": id, "started": started}))
        }
        "audio_capture_stop" => {
            let id = need_str(p, "capture_id")?;
            let cap = with_captures(|m| m.remove(id)).ok_or_else(|| format!("no capture `{id}`"))?;
            cap.stop()
        }
        "audio_mute" => {
            let udid = s.device(p)?.info().udid;
            let key = format!("mute-{udid}");
            if need_bool(p, "on")? {
                if with_captures(|m| m.contains_key(&key)) {
                    return Ok(json!({"muted": true}));
                }
                let pid = launchd_sim(&udid)?;
                let cap = sim_audio::Capture::start(pid, None, true)?;
                with_captures(|m| m.insert(key, cap));
            } else if let Some(cap) = with_captures(|m| m.remove(&key)) {
                cap.stop()?;
            }
            Ok(json!({"muted": with_captures(|m| m.contains_key(&format!("mute-{udid}")))}))
        }
        "network_env" => {
            Ok(shaper_env(&s.device(p)?)?.into_iter().map(|(k, v)| (k, Value::String(v))).collect::<serde_json::Map<_, _>>().into())
        }
        other => Err(format!("unknown method `{other}`; call `help`")),
    }
}

/// `logs`: the device's unified log for the window `last`, as `log show --style compact` prints it.
///
/// With `stream`, the first call starts one in-device `log stream --style compact` for the
/// device's boot and later calls answer from what it has read ([`LogStreams`]), without spawning:
/// a window that reaches back before the stream started (or past what its ring still holds), a
/// `last` other than `<n>s|m|h|d`, a `predicate` and a device whose boots can't be told apart
/// are answered by `log show`, as without `stream`. The stream doesn't carry everything `log show`
/// does (iOS 27.2): launchd_sim's lines (0 of 271 over an app launch and terminate; 48 of 396
/// lines, 12%, in 28 s of a settled device), signpost state dumps (`Sd`) and `Last log repeated`
/// lines (with launchd_sim's, 195 of 18,476 lines, 1%, in 12 s of a just-booted device); and its
/// timestamps are often 1 ms later (within ±1 ms on 99% of lines).
fn logs(s: &mut Session, p: &Value) -> Result<Value, String> {
    let (last, predicate, stream) = log_options(p)?;
    let device = s.device(p)?;
    let source = DeviceLog(&device);
    let streamed = if stream {
        let key = BootKey::of(device.state() == DeviceState::Booted, None, device.last_booted_at());
        let udid = device.udid();
        with_log_streams(|streams| streams.window(&udid, key, last, predicate, &source))
    } else {
        None
    };
    let lines = match streamed {
        Some(lines) => lines,
        None => source.show(last, predicate)?,
    };
    Ok(lines.into_iter().map(Value::String).collect())
}

/// The bytes of log lines a device's stream keeps: about half a minute of a device that has just
/// booted (~3,000 lines/s), an hour or more of a settled one.
const LOG_RING_BYTES: usize = 16 << 20;

/// How long a log stream nobody reads keeps running.
const LOG_IDLE: Duration = Duration::from_secs(300);

/// Each device's log stream (by UDID), owned by this (long-running) process.
static LOG_STREAMS: std::sync::Mutex<Option<LogStreams>> = std::sync::Mutex::new(None);

fn with_log_streams<T>(f: impl FnOnce(&mut LogStreams) -> T) -> T {
    let mut guard = LOG_STREAMS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(|| LogStreams::new(LOG_RING_BYTES, LOG_IDLE, Arc::new(std::time::Instant::now))))
}

/// Where a device's log comes from: a running stream, or one window read from the log store.
trait LogSource {
    /// Starts `log stream --style compact`.
    fn stream(&self) -> Result<LogPipe, String>;
    /// `log show --style compact --last <last>`, filtered by `predicate`.
    fn show(&self, last: &str, predicate: Option<&str>) -> Result<Vec<String>, String>;
}

/// A running stream: its output, and how to stop it.
struct LogPipe {
    reader: Box<dyn std::io::Read + Send>,
    stop: Box<dyn FnOnce() + Send>,
}

/// The log of a device of this host.
struct DeviceLog<'a>(&'a Device);

impl LogSource for DeviceLog<'_> {
    fn stream(&self) -> Result<LogPipe, String> {
        let args = ["stream", "--style", "compact"].map(str::to_owned);
        let (pid, reader) = self.0.exec_stream("log", &args, &[]).map_err(err)?;
        // The device's programs are host processes; a stopped stream is only signalled while its
        // output is open (it hasn't exited), so the pid is still its own.
        let stop = move || {
            let _ = std::process::Command::new("/bin/kill").arg(pid.to_string()).status();
        };
        Ok(LogPipe { reader: Box::new(reader), stop: Box::new(stop) })
    }

    fn show(&self, last: &str, predicate: Option<&str>) -> Result<Vec<String>, String> {
        let mut args = vec!["show".to_owned(), "--style".into(), "compact".into(), "--last".into(), last.to_owned()];
        if let Some(pred) = predicate {
            args.extend(["--predicate".to_owned(), pred.to_owned()]);
        }
        let out = self.0.exec("log", &args, &[]).map_err(err)?;
        Ok(out.stdout.lines().map(str::to_owned).collect())
    }
}

/// The `last` windows a stream can answer: `<n>s`, `<n>m`, `<n>h` or `<n>d`.
fn log_window(last: &str) -> Option<Duration> {
    let unit = match last.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return None,
    };
    // `log show --last 0s` prints the whole store, not an empty window.
    let n: u64 = last[..last.len() - 1].parse().ok().filter(|&n| n > 0)?;
    Some(Duration::from_secs(n.checked_mul(unit)?))
}

type Clock = Arc<dyn Fn() -> std::time::Instant + Send + Sync>;

/// The lines a stream has read, each with when it arrived, up to a byte budget: the oldest go
/// first, and the window it can answer starts after the last one dropped.
struct LogRing {
    lines: std::collections::VecDeque<(std::time::Instant, String)>,
    bytes: usize,
    capacity: usize,
    /// The earliest time from which every line is kept: when the stream started, then just after
    /// the last line dropped.
    complete_from: std::time::Instant,
}

impl LogRing {
    fn new(capacity: usize, started: std::time::Instant) -> Self {
        Self { lines: Default::default(), bytes: 0, capacity, complete_from: started }
    }

    fn push(&mut self, at: std::time::Instant, line: String) {
        self.bytes += line.len();
        self.lines.push_back((at, line));
        while self.bytes > self.capacity {
            let Some((dropped, line)) = self.lines.pop_front() else { break };
            self.bytes -= line.len();
            self.complete_from = self.complete_from.max(dropped + Duration::from_nanos(1));
        }
    }

    /// The lines that arrived in the `last` before `now`, oldest first; `None` when the window
    /// reaches back past what the ring holds.
    fn window(&self, now: std::time::Instant, last: Duration) -> Option<Vec<String>> {
        let since = now.checked_sub(last)?;
        if since < self.complete_from {
            return None;
        }
        let first = self.lines.partition_point(|(at, _)| *at < since);
        Some(self.lines.range(first..).filter(|(at, _)| *at <= now).map(|(_, l)| l.clone()).collect())
    }
}

/// A stream's state, shared with the thread reading it and the one stopping it when idle.
struct Tail {
    ring: LogRing,
    /// The stream's column header (`log show` prints the same one first).
    header: Option<String>,
    /// Its output closed (the device shut down, or it was stopped).
    ended: bool,
    used: std::time::Instant,
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl Tail {
    fn end(&mut self) {
        if !self.ended
            && let Some(stop) = self.stop.take()
        {
            stop();
        }
        self.ended = true;
        self.ring.lines.clear();
        self.ring.bytes = 0;
    }
}

type SharedTail = Arc<std::sync::Mutex<Tail>>;

fn lock(tail: &SharedTail) -> std::sync::MutexGuard<'_, Tail> {
    tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One log stream per device, kept for the boot it started in, stopped when it is idle for `idle`.
struct LogStreams {
    kept: std::collections::HashMap<String, (BootKey, SharedTail)>,
    capacity: usize,
    idle: Duration,
    clock: Clock,
}

impl LogStreams {
    fn new(capacity: usize, idle: Duration, clock: Clock) -> Self {
        Self { kept: Default::default(), capacity, idle, clock }
    }

    /// The lines of the window `last` from `udid`'s stream, starting it (and stopping one from an
    /// earlier boot) as needed; `None` when `log show` answers instead.
    fn window(
        &mut self,
        udid: &str,
        key: Option<BootKey>,
        last: &str,
        predicate: Option<&str>,
        source: &impl LogSource,
    ) -> Option<Vec<String>> {
        let Some(key) = key else {
            self.stop(udid);
            return None;
        };
        // A predicate is NSPredicate over fields the compact lines don't all carry: `log show`
        // filters, and no stream starts for it.
        if predicate.is_some() {
            return None;
        }
        let tail = match self.kept.get(udid) {
            Some((kept, tail)) if *kept == key && !lock(tail).ended => tail.clone(),
            _ => {
                self.stop(udid);
                let tail = self.start(source).ok()?;
                self.kept.insert(udid.to_owned(), (key, tail.clone()));
                tail
            }
        };
        let now = (self.clock)();
        let mut tail = lock(&tail);
        // It may have ended (idle, or the device shut down) since the check above released the
        // lock: its ring is empty then, so `log show` answers rather than a bare header.
        if tail.ended {
            return None;
        }
        tail.used = now;
        let header = tail.header.clone()?;
        let lines = tail.ring.window(now, log_window(last)?)?;
        Some(std::iter::once(header).chain(lines).collect())
    }

    fn stop(&mut self, udid: &str) {
        if let Some((_, tail)) = self.kept.remove(udid) {
            lock(&tail).end();
        }
    }

    fn start(&self, source: &impl LogSource) -> Result<SharedTail, String> {
        let LogPipe { reader, stop } = source.stream()?;
        let now = (self.clock)();
        let tail = Arc::new(std::sync::Mutex::new(Tail {
            ring: LogRing::new(self.capacity, now),
            header: None,
            ended: false,
            used: now,
            stop: Some(stop),
        }));
        let (reading, clock) = (tail.clone(), self.clock.clone());
        std::thread::Builder::new().name("simee-logs".into()).spawn(move || read_log(reader, &reading, &*clock)).map_err(err)?;
        let (watched, clock, idle) = (tail.clone(), self.clock.clone(), self.idle);
        std::thread::Builder::new()
            .name("simee-logs-idle".into())
            .spawn(move || {
                let every = (idle / 4).min(Duration::from_secs(15));
                loop {
                    sleep(every);
                    let mut tail = lock(&watched);
                    if tail.ended {
                        return;
                    }
                    if clock().saturating_duration_since(tail.used) >= idle {
                        tail.end();
                        return;
                    }
                }
            })
            .map_err(err)?;
        Ok(tail)
    }
}

/// Reads a stream's lines into its ring until its output closes.
fn read_log(reader: Box<dyn std::io::Read + Send>, tail: &SharedTail, clock: &dyn Fn() -> std::time::Instant) {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim_end_matches(['\n', '\r']).to_owned();
        let now = clock();
        let mut tail = lock(tail);
        if tail.ended {
            return;
        }
        if tail.header.is_none() && line.starts_with("Timestamp ") {
            tail.header = Some(line);
        } else {
            tail.ring.push(now, line);
        }
    }
    // It exited by itself: nothing to stop.
    let mut tail = lock(tail);
    tail.stop = None;
    tail.end();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn keys_go_a_gap_apart_and_twenty_characters_type_in_under_a_fifth_of_a_second() {
        let gap = Duration::from_millis(3);
        assert_eq!(super::key_delays(2, gap), [Duration::ZERO, gap, gap], "down at once, up a gap later, then a gap");
        assert_eq!(super::key_delays(0, gap), [Duration::ZERO]);
        let typed =
            |text: &str| super::key_delays(sim_core::hid::key_events(text).expect("keys").len(), super::KEY_GAP).iter().sum::<Duration>();
        assert!(typed("abcdefghijklmnopqrst") <= Duration::from_millis(200), "{:?}", typed("abcdefghijklmnopqrst"));
        // Before: 30 ms a key, 20 more with shift; the same plan at 15 ms a report would be 600 ms.
        assert!(typed("Hello, World! Ok 42?") <= Duration::from_millis(200), "{:?}", typed("Hello, World! Ok 42?"));
    }

    #[test]
    fn only_home_and_lock_hold_the_measured_default_and_only_on_an_iphone_or_ipad() {
        use sim_core::form_factor::FormFactor;
        use sim_core::hid::Button;
        for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Foldable] {
            for b in [Button::Home, Button::Lock] {
                assert_eq!(super::button_hold(b, Some(f)), super::BUTTON_HOLD, "{b:?} on {f:?}");
            }
            for b in [Button::Side, Button::Siri, Button::ApplePay, Button::Action, Button::VolumeUp] {
                assert_eq!(super::button_hold(b, Some(f)), Duration::from_millis(80), "{b:?} on {f:?}: unmeasured, as before");
            }
        }
        for f in [Some(FormFactor::Tv), Some(FormFactor::Watch), Some(FormFactor::Vision), None] {
            assert_eq!(super::button_hold(Button::Home, f), Duration::from_millis(80), "{f:?}: unmeasured, as before");
        }
        assert!(super::TAP_HOLD <= Duration::from_millis(20) && super::BUTTON_HOLD <= Duration::from_millis(20));
    }

    #[test]
    fn a_second_built_in_screen_is_an_inner_screen_only_on_the_duo() {
        let duo = "com.apple.CoreSimulator.SimDeviceType.iPhone-Duo";
        assert_eq!(super::inner_for(duo, Some(3)), Some(3));
        assert_eq!(super::inner_for(duo, None::<u32>), None);
        assert_eq!(super::inner_for("com.apple.CoreSimulator.SimDeviceType.iPad-Pro", Some(3)), None);
        assert_eq!(super::inner_for("", Some(3)), None);
    }

    use super::*;

    /// A fake simulator service: it records the device set each connection asks for and refuses
    /// it, so every method stops at its first connection and nothing reaches the real service.
    fn fake_service() -> (Opener, Arc<std::sync::Mutex<Vec<Option<String>>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        let open: Opener = Arc::new(move |set: Option<&str>| {
            record.lock().expect("seen").push(set.map(str::to_owned));
            Err("the fake simulator service refuses every connection".to_owned())
        });
        (open, seen)
    }

    /// Every iOS method on a session whose service is [`fake_service`]: the sets each one opened.
    fn opened_by_every_method(p: &Value, env: Option<&str>) -> Vec<(&'static str, Vec<Option<String>>, String)> {
        let mut opened = Vec::new();
        for (method, ..) in crate::METHODS {
            if !crate::platforms(method).contains(&sim_core::platform::Platform::Ios) {
                continue;
            }
            let (open, seen) = fake_service();
            let mut s = crate::methods::Session {
                ios: Session::with_opener(open),
                fake_env_set: Some(env.map(str::to_owned)),
                ..Default::default()
            };
            let mut p = p.clone();
            // Where a name means something else to one method.
            match *method {
                "stylus" => p["button"] = json!(false),
                "pointer" => {
                    p["action"] = json!("move");
                    p["button"] = json!("primary");
                    p.as_object_mut().map(|o| (o.remove("to_x"), o.remove("to_y")));
                }
                "contrast_check" => p["appearance"] = json!("both"),
                "watch_gesture" => p["gesture"] = json!("double-tap"),
                "privacy" => p["action"] = json!("grant"),
                _ => {}
            }
            let result = crate::methods::call(&mut s, method, &p);
            let seen = seen.lock().expect("seen").clone();
            opened.push((*method, seen, result.err().unwrap_or_default()));
        }
        opened
    }

    /// Methods that never connect to the simulator service, so no device set reaches them: host
    /// and catalogue methods, and streams and captures named by their id.
    const NO_SERVICE: &[&str] = &[
        "help",
        "identity",
        "host",
        "form_factors",
        "admission_check",
        "stream_status",
        "stream_stop",
        "audio_capture_stop",
        "camera_sources",
    ];

    /// Methods that connect only when the host runs a simulator (`devices_usage` names the running
    /// ones): either way is right, as long as a connection is on the call's set.
    const WHEN_SIMULATORS_RUN: &[&str] = &["devices_usage"];

    /// The parameters every method needs to get as far as its device: a UDID nothing has, so even a
    /// method that reached the real service would find no device.
    fn every_param(set: Option<&str>) -> Value {
        const UDID: &str = "409409A0-0000-4000-8000-000000000409";
        let strings: &[(&str, &str)] = &[
            ("device", UDID),
            ("name", "simee-409"),
            ("device_type", "iPhone 17"),
            ("runtime", "iOS-27-2"),
            ("url", "https://example.com"),
            ("path", "/nonexistent/simee-409"),
            ("bundle_id", "dev.simee.none"),
            ("text", "a"),
            ("key", "a"),
            ("button", "home"),
            ("whip_url", "http://127.0.0.1:9/whip"),
            ("store", "dev.simee.none"),
            ("program", "/usr/bin/true"),
            ("service", "photos"),
            ("orientation", "portrait"),
            ("direction", "up"),
            ("appearance", "light"),
            ("mode", "light"),
            ("language", "en"),
            ("domain", "dev.simee.none"),
            ("gesture", "swipe"),
            ("phase", "down"),
            ("action", "click"),
            ("facing", "back"),
            ("source", "test"),
            ("preset", "wifi"),
            ("inject", "launch"),
            ("capture_id", "c"),
            ("uid", "u"),
            ("admission", "ignore"),
            ("property", "p"),
            ("pattern", "p"),
            ("predicate", "1"),
            ("last", "1s"),
            ("command", "true"),
            ("type", "string"),
            ("scope", "output"),
            ("address", "0x1"),
            ("size", "large"),
        ];
        let numbers: &[(&str, i64)] = &[
            ("x", 1),
            ("x1", 1),
            ("y1", 1),
            ("x2", 2),
            ("y2", 2),
            ("usage", 4),
            ("y", 1),
            ("to_x", 2),
            ("to_y", 2),
            ("level", 1),
            ("latitude", 1),
            ("longitude", 1),
            ("timeout_s", 1),
            ("wait_ms", 0),
            ("pid", 1),
            ("signal", 0),
            ("category", 1),
            ("option", 1),
            ("duration_ms", 1),
            ("ticks", 1),
            ("delta", 1),
            ("latency_ms", 0),
            ("value", 1),
            ("state", 1),
            ("scale", 1),
            ("angle", 0),
        ];
        let flags: &[(&str, bool)] = &[("on", true), ("settle", false), ("granted", true), ("suppressed", true), ("folded", true)];
        let mut p = serde_json::Map::new();
        p.extend(strings.iter().map(|(k, v)| ((*k).to_owned(), json!(v))));
        p.extend(numbers.iter().map(|(k, v)| ((*k).to_owned(), json!(v))));
        p.extend(flags.iter().map(|(k, v)| ((*k).to_owned(), json!(v))));
        p.insert("devices".into(), json!([UDID]));
        p.insert("region".into(), json!({"x": 0, "y": 0, "width": 1, "height": 1}));
        for empty in ["payload", "env"] {
            p.insert(empty.into(), json!({}));
        }
        for empty in ["ops", "commands", "args"] {
            p.insert(empty.into(), json!([]));
        }
        let mut p = Value::Object(p);
        if let Some(set) = set {
            p["set"] = json!(set);
        }
        p
    }

    /// #409: every iOS method connects on the device set the call names (`set`, over the
    /// environment's), never on the default set, whatever thread it connects on (the session's,
    /// a device's input worker, a boot thread).
    #[test]
    fn every_ios_method_connects_on_the_set_the_call_names() {
        const SET: &str = "/nonexistent/simee-409/set";
        let mut quiet = Vec::new();
        for env in [None, Some("/nonexistent/simee-409/env-set")] {
            for (method, seen, why) in opened_by_every_method(&every_param(Some(SET)), env) {
                assert!(seen.iter().all(|s| s.as_deref() == Some(SET)), "`{method}` (environment {env:?}) connected on {seen:?}");
                if seen.is_empty() && env.is_none() && !WHEN_SIMULATORS_RUN.contains(&method) {
                    quiet.push((method, why));
                }
            }
        }
        let names: Vec<&str> = quiet.iter().map(|(m, _)| *m).collect();
        assert_eq!(names, NO_SERVICE, "these methods connected to no service; is one missing its device set? {quiet:#?}");
    }

    /// #409: with no `set`, a method connects on the environment's set, and with neither on the
    /// default set, exactly as before device sets.
    #[test]
    fn without_a_set_param_methods_connect_on_the_environment_set_else_the_default() {
        const ENV: &str = "/nonexistent/simee-409/env-set";
        for (method, seen, _) in opened_by_every_method(&every_param(None), Some(ENV)) {
            assert!(seen.iter().all(|s| s.as_deref() == Some(ENV)), "`{method}` connected on {seen:?}, not the environment's set");
        }
        for (method, seen, _) in opened_by_every_method(&every_param(None), None) {
            assert!(seen.iter().all(Option::is_none), "`{method}` connected on {seen:?}, not the default set");
            if !WHEN_SIMULATORS_RUN.contains(&method) {
                assert_eq!(seen.is_empty(), NO_SERVICE.contains(&method), "`{method}` connected {seen:?}");
            }
        }
        // An empty `set` is the default set, whatever the environment says.
        for (method, seen, _) in opened_by_every_method(&every_param(Some("")), Some(ENV)) {
            assert!(seen.iter().all(Option::is_none), "`{method}` connected on {seen:?}, not the default set");
        }
    }

    /// A set the call can't use (a relative path) fails the methods that connect, with the reason,
    /// and connects nowhere.
    #[test]
    fn a_relative_set_fails_and_connects_nowhere() {
        let (open, seen) = fake_service();
        let mut s = crate::methods::Session { ios: Session::with_opener(open), fake_env_set: Some(None), ..Default::default() };
        let err = crate::methods::call(&mut s, "devices", &json!({"set": "sets/a", "platform": "ios"})).expect_err("relative");
        assert!(err.contains("absolute"), "{err}");
        let err = crate::methods::call(
            &mut s,
            "tap",
            &json!({"set": "sets/a", "device": "409409A0-0000-4000-8000-000000000409", "x": 1, "y": 1}),
        )
        .expect_err("relative");
        assert!(err.contains("absolute"), "{err}");
        assert!(seen.lock().expect("seen").is_empty());
    }

    /// The default set is the one the service has always used: `Context::open(None)` is
    /// `Context::new()`'s set, `~/Library/Developer/CoreSimulator/Devices`, and a set's path is
    /// the one asked for. Opening a set starts nothing (it reads or makes the set's plist).
    #[test]
    fn the_default_set_is_unchanged_and_a_custom_set_is_the_path_asked_for() {
        let Ok(default) = Context::new() else {
            eprintln!("skipped: no simulator service on this host");
            return;
        };
        let home = std::env::var("HOME").expect("HOME");
        assert_eq!(default.set_path(), format!("{home}/Library/Developer/CoreSimulator/Devices"));
        assert_eq!(Context::open(None).expect("default set").set_path(), default.set_path());
        let dir = std::env::temp_dir().join(format!("simee-409-set-{}", std::process::id()));
        let custom = Context::open(Some(dir.to_str().expect("utf-8"))).expect("custom set");
        // The service names the set by its resolved path (`/var` is `/private/var` on macOS).
        assert_eq!(std::path::PathBuf::from(custom.set_path()), dir.canonicalize().expect("the set's directory, made"));
        assert!(custom.devices().expect("devices").is_empty(), "a new set has no devices");
        drop(custom);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session connects on the set of the call in hand, not the one of the call before.
    #[test]
    fn a_session_connects_on_each_calls_set() {
        let (open, seen) = fake_service();
        let mut s = Session::with_opener(open);
        s.use_set(Ok(Some("/a".into())));
        assert!(s.ctx().is_err());
        s.use_set(Ok(None));
        assert!(s.ctx().is_err());
        assert_eq!(*seen.lock().expect("seen"), [Some("/a".to_owned()), None]);
    }

    /// A session that stays open (as `simee serve`'s does) finds a device by UDID and name, then
    /// misses it under its old name once another process renames it and under its UDID once that
    /// deletes it: the lookup reads the service's live device set, never a copy of its own.
    /// `SIMEE_E2E_CREATE=1 cargo test -p simee-api renamed_or_deleted -- --ignored --nocapture`.
    #[test]
    #[ignore = "creates, renames and deletes a simulator (SIMEE_E2E_CREATE=1)"]
    fn a_renamed_or_deleted_device_misses_the_session_lookup() {
        if std::env::var_os("SIMEE_E2E_CREATE").is_none() {
            return;
        }
        /// Deletes the test's simulator however the test ends.
        struct Spare(String);
        impl Drop for Spare {
            fn drop(&mut self) {
                let _ = std::process::Command::new("xcrun").args(["simctl", "delete", &self.0]).output();
            }
        }
        let simctl = |args: &[&str]| {
            let out = std::process::Command::new("xcrun").arg("simctl").args(args).output().expect("simctl");
            assert!(out.status.success(), "simctl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        /// How long `ok` held after the change, polling: the service tells this process about
        /// another's change asynchronously.
        fn settles(mut ok: impl FnMut() -> bool) -> Duration {
            let start = std::time::Instant::now();
            while !ok() {
                assert!(start.elapsed() < Duration::from_secs(10), "still stale after 10 s");
                std::thread::sleep(Duration::from_millis(5));
            }
            start.elapsed()
        }
        let mut s = Session::default();
        let ctx = s.ctx().expect("service");
        let ty = ctx.device_types().expect("types").into_iter().find(|t| t.product_family == "iPhone").expect("an iPhone type");
        let runtime = ctx.runtimes().expect("runtimes").into_iter().rfind(|r| r.available && r.identifier.contains(".iOS-")).expect("iOS");
        let (name, renamed) = (format!("simee-lookup-{}", std::process::id()), format!("simee-lookup-{}-renamed", std::process::id()));
        let udid = ctx.create(&name, &ty.identifier, &runtime.identifier).expect("create").udid();
        let _spare = Spare(udid.clone());
        let lookup = |s: &mut Session, q: &str| s.device(&json!({"device": q})).map(|d| d.info());
        let as_named = |s: &mut Session, q: &str| lookup(s, q).map(|i| (i.udid, i.name));
        assert_eq!(as_named(&mut s, &udid), Ok((udid.clone(), name.clone())));
        assert_eq!(as_named(&mut s, &udid.to_ascii_lowercase()), Ok((udid.clone(), name.clone())), "any case");
        assert_eq!(as_named(&mut s, &name), Ok((udid.clone(), name.clone())));

        simctl(&["rename", &udid, &renamed]);
        let took = settles(|| lookup(&mut s, &name).is_err());
        assert_eq!(lookup(&mut s, &name).map(|i| i.udid), Err(format!("no device matches `{name}`")));
        assert_eq!(as_named(&mut s, &renamed), Ok((udid.clone(), renamed.clone())));
        assert_eq!(as_named(&mut s, &udid), Ok((udid.clone(), renamed.clone())), "by UDID, the new name");
        eprintln!("rename seen after {took:?}");

        simctl(&["delete", &udid]);
        let took = settles(|| lookup(&mut s, &udid).is_err());
        assert_eq!(lookup(&mut s, &udid).map(|i| i.udid), Err(format!("no device matches `{udid}`")));
        assert!(lookup(&mut s, &renamed).is_err());
        eprintln!("delete seen after {took:?}");
    }

    /// `processes`, `connections`, `kill`, `network_state`'s `boot_injected` check and the audio
    /// settings spawn nothing on the host. Measured in a process of its own (every child any test
    /// thread reaps counts), by the summed lifetime of the children it waited for.
    #[test]
    fn device_introspection_spawns_nothing() {
        let out = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args(["ios::tests::device_introspection_spawns_nothing_here", "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env("SIMEE_SPAWN_CHECK", "1")
            .output()
            .expect("run the check");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && stdout.contains("1 passed"), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    #[ignore = "run by device_introspection_spawns_nothing in a process of its own"]
    fn device_introspection_spawns_nothing_here() {
        if std::env::var_os("SIMEE_SPAWN_CHECK").is_none() {
            return;
        }
        let me = std::process::id() as i32;
        let reaped = || sim_coresim::procs::usage(me).expect("own usage").reaped_children_ns;
        let before = reaped();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let port = listener.local_addr().expect("addr").port();
        // This process as a device's tree: it is its root and has no children.
        let tree = sim_coresim::procs::tree(me);
        assert_eq!(tree, vec![me]);

        let rows = processes_in(&tree);
        assert_eq!(rows.len(), 1);
        let exe = std::env::current_exe().expect("exe").canonicalize().expect("canonical");
        assert_eq!((rows[0].pid, rows[0].ppid, Path::new(&rows[0].path)), (me, std::os::unix::process::parent_id() as i32, exe.as_path()));
        assert!(rows[0].rss_kb > 0);
        let conns = connections_in(&tree);
        assert!(
            conns.iter().any(|c| c.pid == me && c.protocol == "TCP" && c.local == format!("127.0.0.1:{port}") && c.state == "LISTEN"),
            "{conns:?}"
        );
        assert_eq!(kill_in(&tree, i64::from(me), 0), Ok(Value::Null));
        assert_eq!(kill_in(&tree, 1, 0), Err("pid 1 is not a process of this device".into()));
        assert_eq!(kill_in(&tree, 1 << 40, 0), Err(format!("pid {} is not a process of this device", 1i64 << 40)));
        assert!(kill_in(&tree, i64::from(me), 1 << 40).expect_err("bad signal").starts_with(&format!("kill {me} failed")));
        assert!(!springboard_inserts(&tree, "/x/netshape.dylib"));
        assert!(device_tree("00000000-0000-0000-0000-000000000000").expect_err("no device").contains("is the device booted?"));
        let data = std::env::temp_dir().join(format!("simee-spawn-check-{me}"));
        let settings = sim_audio::settings_path(&data);
        std::fs::create_dir_all(settings.parent().expect("parent")).expect("dir");
        std::fs::write(&settings, "<plist version=\"1.0\"><dict><key>sim_volume</key><integer>60</integer></dict></plist>").expect("write");
        sim_audio::set_volume(&data, 30).expect("volume");
        assert_eq!(sim_audio::settings(&data).expect("read")["sim_volume"], 30);
        std::fs::remove_dir_all(&data).ok();

        assert_eq!(reaped(), before, "a child was spawned and waited for");
        assert!(sim_coresim::procs::children(me).is_empty(), "a child is still running");
        // The check sees a spawn: one short child moves it.
        std::process::Command::new("/usr/bin/true").status().expect("true");
        assert!(reaped() > before);
    }

    /// The kernel's own records, for executable paths of every length mod 8 and an empty argv[0]:
    /// the padding after the path is skipped exactly, so "" stays argv[0] and FOO stays in the
    /// environment.
    #[test]
    fn procargs_of_live_processes_keep_an_empty_argv0() {
        use std::os::unix::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("simee-procargs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        for n in 1..=8 {
            let link = dir.join("s".repeat(n));
            std::os::unix::fs::symlink("/bin/sleep", &link).expect("symlink");
            let mut child = std::process::Command::new(&link).arg0("").arg("30").env_clear().env("FOO", "bar").spawn().expect("spawn");
            let pid = child.id() as i32;
            // Read once exec has replaced the forked image (its path is the link's).
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let args = loop {
                let a = sim_coresim::procs::procargs(pid).and_then(|b| sim_debug::process::ProcArgs::parse(&b));
                if a.as_ref().is_some_and(|a| a.exec_path == link.to_str().expect("utf-8")) || std::time::Instant::now() > deadline {
                    break a.expect("procargs");
                }
                sleep(Duration::from_millis(5));
            };
            child.kill().ok();
            child.wait().ok();
            assert_eq!(args.exec_path, link.to_str().expect("utf-8"));
            assert_eq!(args.argv, vec![String::new(), "30".into()], "path of {} bytes", args.exec_path.len());
            assert_eq!(args.var("FOO"), Some("bar"), "{args:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Records sim-debug's libproc fixtures from a booted device: its tree read through libproc
    /// with `ps` (processes), and its sockets with `lsof` (connections), each between two reads
    /// that agree. The user's home and every non-loopback address are rewritten.
    /// `SIMEE_FIXTURE_UDID=<udid> SIMEE_FIXTURE_DIR=crates/sim-debug/fixtures cargo test -p simee-api
    /// record_libproc_fixtures -- --ignored`; with `SIMEE_FIXTURE_INJECTED=1`, SpringBoard's
    /// `KERN_PROCARGS2` record too (on a device booted with `network_shape {inject:"boot"}`).
    #[test]
    #[ignore = "records fixtures from a booted device (SIMEE_FIXTURE_UDID, SIMEE_FIXTURE_DIR)"]
    fn record_libproc_fixtures() {
        let (Ok(udid), Ok(dir)) = (std::env::var("SIMEE_FIXTURE_UDID"), std::env::var("SIMEE_FIXTURE_DIR")) else { return };
        let dir = Path::new(&dir);
        let home = std::env::var("HOME").expect("HOME");
        let private = |s: &str| s.replace(&format!("{home}/"), "/Users/user/");
        let run = |program: &str, args: &[&str]| {
            String::from_utf8(std::process::Command::new(program).args(args).output().expect(program).stdout).expect("utf-8")
        };

        let read = || {
            let tree = device_tree(&udid).expect("tree");
            let samples: Vec<_> = tree.iter().filter_map(|&pid| sample(pid)).collect();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("now").as_secs();
            (now, samples)
        };
        let pids = |s: &[sim_debug::process::Sample]| s.iter().map(|x| x.pid).collect::<std::collections::BTreeSet<_>>();
        let (now, samples, ps) = (0..20)
            .find_map(|_| {
                let (now, a) = read();
                let ps = run("/bin/ps", &["-Ao", "pid=,ppid=,pcpu=,rss=,etime=,comm="]);
                let (_, b) = read();
                (pids(&a) == pids(&b)).then_some((now, a, ps))
            })
            .expect("two reads agreed");
        let tree = pids(&samples);
        let mut tsv = format!("now {now}\n");
        for s in &samples {
            tsv.push_str(&format!("{}\t{}\t{}\t{}\t{}\t{}\n", s.pid, s.ppid, s.cpu_usage, s.resident_bytes, s.start_s, private(&s.path)));
        }
        let ps: String = ps
            .lines()
            .filter(|l| l.split_whitespace().next().and_then(|p| p.parse().ok()).is_some_and(|p: i32| tree.contains(&p)))
            .map(|l| format!("{}\n", private(l)))
            .collect();
        std::fs::write(dir.join("ios27.2-processes.libproc.tsv"), tsv).expect("write");
        std::fs::write(dir.join("ios27.2-processes.ps.txt"), ps).expect("write");

        let raw = || {
            let tree = device_tree(&udid).expect("tree");
            let mut out = Vec::new();
            for &pid in &tree {
                let name = sim_coresim::procs::name(pid).unwrap_or_default();
                out.extend(sim_coresim::procs::sockets(pid).into_iter().map(|s| (pid, name.clone(), s)));
            }
            (tree, out)
        };
        let (sockets, lsof) = (0..20)
            .find_map(|_| {
                let (tree, a) = raw();
                let list = tree.iter().map(i32::to_string).collect::<Vec<_>>().join(",");
                let lsof = run("/usr/sbin/lsof", &["-nP", "-a", "-i", "-p", &list, "-F", "pcPnT"]);
                let (_, b) = raw();
                (a == b).then_some((a, lsof))
            })
            .expect("two reads agreed");
        // Every address but loopback and unspecified ones, to documentation ranges, the same way
        // in both files.
        let mut map: std::collections::HashMap<std::net::IpAddr, std::net::IpAddr> = std::collections::HashMap::new();
        let mut hide = |ip: std::net::IpAddr| {
            if ip.is_loopback() || ip.is_unspecified() {
                return ip;
            }
            let n = map.len() as u16 + 1;
            *map.entry(ip).or_insert_with(|| match ip {
                std::net::IpAddr::V4(_) => std::net::IpAddr::from([192, 0, 2, n as u8]),
                std::net::IpAddr::V6(_) => std::net::IpAddr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, n]),
            })
        };
        let mut hide_addr = |a: std::net::SocketAddr| std::net::SocketAddr::new(hide(a.ip()), a.port());
        let mut tsv = String::new();
        for (pid, name, s) in &sockets {
            let (local, remote) = (hide_addr(s.local), hide_addr(s.remote));
            tsv.push_str(&format!("{pid}\t{name}\t{}\t{}\t{local}\t{remote}\t{}\n", s.fd, s.protocol, s.tcp_state.unwrap_or(-1)));
        }
        let hide_endpoint = |e: &str, map: &std::collections::HashMap<std::net::IpAddr, std::net::IpAddr>| {
            let (host, port) = e.rsplit_once(':').expect("host:port");
            let ip: Option<std::net::IpAddr> = host.trim_start_matches('[').trim_end_matches(']').parse().ok();
            match ip.and_then(|ip| map.get(&ip)) {
                Some(std::net::IpAddr::V6(v6)) => format!("[{v6}]:{port}"),
                Some(v4) => format!("{v4}:{port}"),
                None => e.to_owned(),
            }
        };
        let lsof: String = lsof
            .lines()
            .map(|l| match l.strip_prefix('n') {
                Some(n) => format!("n{}\n", n.split("->").map(|e| hide_endpoint(e, &map)).collect::<Vec<_>>().join("->")),
                None => format!("{l}\n"),
            })
            .collect();
        std::fs::write(dir.join("ios27.2-connections.libproc.tsv"), tsv).expect("write");
        std::fs::write(dir.join("ios27.2-connections.lsof.txt"), lsof).expect("write");

        if std::env::var_os("SIMEE_FIXTURE_INJECTED").is_some() {
            let tree = device_tree(&udid).expect("tree");
            let sb = tree
                .iter()
                .copied()
                .find(|&p| sim_coresim::procs::proc(p).is_some_and(|p| p.name() == "SpringBoard"))
                .expect("SpringBoard");
            let buf = sim_coresim::procs::procargs(sb).expect("procargs");
            let args = sim_debug::process::ProcArgs::parse(&buf).expect("parse");
            // The record's own layout, with only the variables the check reads (and the user's home
            // rewritten): argc, the path, padding, argv, the environment, then apple[] dropped.
            let mut out = (args.argv.len() as i32).to_ne_bytes().to_vec();
            let path = private(&args.exec_path);
            out.extend_from_slice(path.as_bytes());
            // Its NUL and the kernel's padding to a multiple of 8 bytes.
            out.resize(4 + (path.len() + 1).next_multiple_of(8), 0);
            let keep = ["DYLD_INSERT_LIBRARIES=", "SIMEE_NETSHAPE=", "HOME=", "CFFIXED_USER_HOME=", "SIMULATOR_UDID="];
            let env = args.env.iter().filter(|e| keep.iter().any(|k| e.starts_with(k)));
            for s in args.argv.iter().chain(env) {
                out.extend_from_slice(private(s).as_bytes());
                out.push(0);
            }
            out.push(0);
            std::fs::write(dir.join("ios27.2-springboard-injected.procargs"), out).expect("write");
            eprintln!("SpringBoard {sb}: {:?}", args.var("DYLD_INSERT_LIBRARIES"));
        }
        eprintln!("{} processes, {} sockets recorded", samples.len(), sockets.len());
    }

    mod log_streams {
        use std::cell::{Cell, RefCell};
        use std::io::Write;
        use std::sync::Mutex;
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;

        use super::*;

        const HEADER: &str = "Timestamp               Ty Process[PID:TID]";

        fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
            m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        /// One stream the fake started: where its lines are written, and whether it was stopped.
        struct Started {
            writer: Arc<Mutex<Option<std::io::PipeWriter>>>,
            stopped: Arc<AtomicBool>,
        }

        impl Started {
            fn write(&self, line: &str) {
                let mut w = held(&self.writer);
                writeln!(w.as_mut().expect("stream open"), "{line}").expect("write");
            }
            fn close(&self) {
                held(&self.writer).take();
            }
            fn stopped(&self) -> bool {
                self.stopped.load(Ordering::Acquire)
            }
        }

        /// A device log: streams the test writes, and a `log show` that says it was asked.
        #[derive(Default)]
        struct FakeLog {
            started: RefCell<Vec<Started>>,
            shown: Cell<u32>,
        }

        impl LogSource for FakeLog {
            fn stream(&self) -> Result<LogPipe, String> {
                let (reader, writer) = std::io::pipe().map_err(err)?;
                let writer = Arc::new(Mutex::new(Some(writer)));
                let stopped = Arc::new(AtomicBool::new(false));
                let (w, s) = (writer.clone(), stopped.clone());
                let stop = move || {
                    s.store(true, Ordering::Release);
                    held(&w).take();
                };
                self.started.borrow_mut().push(Started { writer, stopped });
                Ok(LogPipe { reader: Box::new(reader), stop: Box::new(stop) })
            }

            fn show(&self, last: &str, _: Option<&str>) -> Result<Vec<String>, String> {
                self.shown.set(self.shown.get() + 1);
                Ok(vec![format!("shown {last}")])
            }
        }

        /// A clock the test moves.
        fn clock() -> (Arc<Mutex<Instant>>, Clock) {
            let at = Arc::new(Mutex::new(Instant::now()));
            let read = at.clone();
            (at, Arc::new(move || *held(&read)))
        }

        fn advance(at: &Mutex<Instant>, by: Duration) {
            *held(at) += by;
        }

        fn key(n: u64) -> Option<BootKey> {
            BootKey::of(true, Some(n), None)
        }

        /// What `logs` answers: the stream's window, else `log show`.
        fn logs(streams: &mut LogStreams, source: &FakeLog, key: Option<BootKey>, last: &str, predicate: Option<&str>) -> Vec<String> {
            streams.window("UDID", key, last, predicate, source).unwrap_or_else(|| source.show(last, predicate).expect("shown"))
        }

        /// Waits (up to 2 s) until `done`.
        fn until(mut done: impl FnMut() -> bool) {
            for _ in 0..200 {
                if done() {
                    return;
                }
                sleep(Duration::from_millis(10));
            }
            panic!("timed out");
        }

        fn kept_lines(streams: &LogStreams) -> usize {
            streams.kept.get("UDID").map_or(0, |(_, t)| lock(t).ring.lines.len())
        }

        fn ring(capacity: usize) -> (Instant, LogRing) {
            let t0 = Instant::now();
            (t0, LogRing::new(capacity, t0))
        }

        #[test]
        fn the_ring_answers_windows_since_it_started() {
            let (t0, mut ring) = ring(1 << 20);
            for (s, line) in [(1, "a"), (2, "b"), (3, "c")] {
                ring.push(t0 + Duration::from_secs(s), line.into());
            }
            let now = t0 + Duration::from_secs(3);
            assert_eq!(ring.window(now, Duration::from_millis(1_500)), Some(vec!["b".into(), "c".into()]));
            assert_eq!(ring.window(now, Duration::from_secs(3)), Some(vec!["a".into(), "b".into(), "c".into()]));
            assert_eq!(ring.window(now, Duration::ZERO), Some(vec!["c".into()]));
            assert_eq!(ring.window(now, Duration::from_secs(4)), None, "reaches back before the stream started");
            assert_eq!(ring.window(t0 + Duration::from_secs(2), Duration::from_secs(1)), Some(vec!["a".into(), "b".into()]));
        }

        #[test]
        fn the_ring_drops_its_oldest_lines_past_its_budget_and_no_longer_answers_their_window() {
            let (t0, mut ring) = ring(6);
            for (s, line) in [(1, "aa"), (2, "bb"), (3, "cc"), (4, "dd")] {
                ring.push(t0 + Duration::from_secs(s), line.into());
            }
            assert_eq!(ring.bytes, 6);
            assert_eq!(ring.lines.len(), 3, "the oldest went");
            let now = t0 + Duration::from_secs(4);
            assert_eq!(ring.window(now, Duration::from_secs(3)), None, "would include the dropped line's time");
            assert_eq!(ring.window(now, Duration::from_secs(2)), Some(vec!["bb".into(), "cc".into(), "dd".into()]));
            assert_eq!(ring.window(now, Duration::from_millis(1_500)), Some(vec!["cc".into(), "dd".into()]));
        }

        #[test]
        fn a_stream_window_is_a_count_and_a_unit() {
            assert_eq!(log_window("10s"), Some(Duration::from_secs(10)));
            assert_eq!(log_window("1m"), Some(Duration::from_secs(60)));
            assert_eq!(log_window("2h"), Some(Duration::from_secs(7_200)));
            assert_eq!(log_window("1d"), Some(Duration::from_secs(86_400)));
            for other in ["boot", "10", "", "m", "0s", "1.5m", "-1s", "99999999999999999999d"] {
                assert_eq!(log_window(other), None, "{other}: log show answers");
            }
        }

        #[test]
        fn later_calls_answer_from_the_stream_the_first_started() {
            let (at, clock) = clock();
            let (source, mut streams) = (FakeLog::default(), LogStreams::new(1 << 20, Duration::from_secs(300), clock));
            assert_eq!(logs(&mut streams, &source, key(1), "10s", None), ["shown 10s"], "nothing streamed before the first call");
            assert_eq!(source.started.borrow().len(), 1);
            advance(&at, Duration::from_secs(20));
            for line in [HEADER, "2026-11-26 21:35:04.478 Df a[1:2] one", "  continued", "2026-11-26 21:35:04.479 Df a[1:2] two"] {
                source.started.borrow()[0].write(line);
            }
            until(|| kept_lines(&streams) == 3);
            advance(&at, Duration::from_secs(1));
            let lines = logs(&mut streams, &source, key(1), "10s", None);
            assert_eq!(lines, [HEADER, "2026-11-26 21:35:04.478 Df a[1:2] one", "  continued", "2026-11-26 21:35:04.479 Df a[1:2] two"]);
            advance(&at, Duration::from_secs(2));
            assert_eq!(logs(&mut streams, &source, key(1), "1s", None), [HEADER], "an empty window keeps the header, as log show does");
            assert_eq!((source.shown.get(), source.started.borrow().len()), (1, 1), "answered without spawning");
        }

        #[test]
        fn windows_before_the_stream_and_filtered_calls_go_to_log_show() {
            let (at, clock) = clock();
            let (source, mut streams) = (FakeLog::default(), LogStreams::new(1 << 20, Duration::from_secs(300), clock));
            logs(&mut streams, &source, key(1), "1m", None);
            source.started.borrow()[0].write(HEADER);
            advance(&at, Duration::from_secs(30));
            assert_eq!(logs(&mut streams, &source, key(1), "1m", None), ["shown 1m"], "the stream has run for 30 s of the minute");
            assert_eq!(logs(&mut streams, &source, key(1), "boot", None), ["shown boot"]);
            assert_eq!(logs(&mut streams, &source, key(1), "10s", Some("process == \"a\"")), ["shown 10s"]);
            assert_eq!(logs(&mut streams, &source, None, "10s", None), ["shown 10s"], "boots can't be told apart");
            assert!(source.started.borrow()[0].stopped(), "and the stream from before is stopped");
            assert_eq!(source.shown.get(), 5);
        }

        /// A subscription's reads: `seconds` of one read a second of `last`, with a line arriving
        /// each second; how many of them `log show` answered. The stream's ring holds `keep` lines.
        fn shown_by_a_subscription(last: &str, keep: usize, seconds: u64) -> (u32, usize) {
            let (at, clock) = clock();
            // Each line is 10 bytes, so the ring holds `keep` of them (the heavy-log case: a boot
            // fills it in half a minute).
            let (source, mut streams) = (FakeLog::default(), LogStreams::new(keep * 10, Duration::from_secs(300), clock));
            logs(&mut streams, &source, key(1), last, None);
            source.started.borrow()[0].write(HEADER);
            for second in 1..=seconds {
                advance(&at, Duration::from_secs(1));
                let line = format!("line {second:04}");
                source.started.borrow()[0].write(&line);
                until(|| streams.kept.get("UDID").is_some_and(|(_, t)| lock(t).ring.lines.back().is_some_and(|(_, l)| *l == line)));
                logs(&mut streams, &source, key(1), last, None);
            }
            (source.shown.get(), source.started.borrow().len())
        }

        #[test]
        fn a_short_window_read_every_second_stops_spawning_log_show_once_the_stream_is_that_old() {
            // The logs resource asks for a 5 s window (`sim-mcp`'s LOG_WINDOW) once a second. A ring
            // that holds a boot's 10 s of lines answers every read after the first 5 s from the
            // stream: `log show` ran for the first read, then for the 5 reads that reach back
            // before the stream started, and never after.
            let (shown, streams) = shown_by_a_subscription("5s", 10, 120);
            assert_eq!(streams, 1, "one stream the whole time");
            assert_eq!(shown, 1 + 4, "the first read and the four before the stream is 5 s old, then none for 115 more");
            // A window the ring can't hold falls back on every read: the cost of a 30 s window on
            // a just-booted device.
            let (shown, _) = shown_by_a_subscription("30s", 10, 120);
            assert_eq!(shown, 1 + 120);
        }

        #[test]
        fn a_new_boot_or_a_closed_stream_starts_another() {
            let (at, clock) = clock();
            let (source, mut streams) = (FakeLog::default(), LogStreams::new(1 << 20, Duration::from_secs(300), clock));
            logs(&mut streams, &source, key(1), "10s", None);
            logs(&mut streams, &source, key(2), "10s", None);
            assert!(source.started.borrow()[0].stopped(), "the earlier boot's stream is stopped");
            assert_eq!(source.started.borrow().len(), 2);
            source.started.borrow()[1].close();
            until(|| lock(&streams.kept["UDID"].1).ended);
            advance(&at, Duration::from_secs(1));
            logs(&mut streams, &source, key(2), "10s", None);
            assert_eq!(source.started.borrow().len(), 3);
            assert!(!source.started.borrow()[1].stopped(), "a stream that exited isn't signalled");
        }

        #[test]
        fn an_idle_stream_stops() {
            let (at, clock) = clock();
            let (source, mut streams) = (FakeLog::default(), LogStreams::new(1 << 20, Duration::from_millis(40), clock));
            logs(&mut streams, &source, key(1), "10s", None);
            sleep(Duration::from_millis(100));
            assert!(!source.started.borrow()[0].stopped(), "no time passed on its clock");
            advance(&at, Duration::from_millis(40));
            until(|| source.started.borrow()[0].stopped());
            logs(&mut streams, &source, key(1), "10s", None);
            assert_eq!(source.started.borrow().len(), 2, "the next call starts another");
        }
    }

    #[test]
    fn launch_restarts_unless_restart_is_false() {
        assert_eq!(launch_restart(&json!({"bundle_id": "com.example"})), Ok(true));
        assert_eq!(launch_restart(&json!({"restart": null})), Ok(true));
        assert_eq!(launch_restart(&json!({"restart": true})), Ok(true));
        assert_eq!(launch_restart(&json!({"restart": false})), Ok(false));
        assert_eq!(launch_restart(&json!({"restart": "no"})), Err("`restart` must be true or false, not \"no\"".to_owned()));
        assert!(launch_restart(&json!({"restart": 0})).is_err());
    }

    fn being_updated_error() -> sim_coresim::Error {
        sim_coresim::Error::Service {
            op: "launch",
            message: "SpringBoard holds dev.simee.tiny as being updated (LaunchServicesDataMismatch), ... Reboot the device".into(),
        }
    }

    #[test]
    fn a_launch_refused_as_being_updated_fails_at_once_until_the_device_reboots() {
        let mut b = BeingUpdated::default();
        let t0 = Instant::now();
        b.note("U", "dev.simee.tiny", &being_updated_error(), t0, || Some(41));
        let refused = b.refusal("U", "dev.simee.tiny", t0 + Duration::from_secs(5), || Some(41)).expect("remembered");
        assert!(refused.starts_with("launch failed: SpringBoard holds dev.simee.tiny as being updated"), "{refused}");
        assert!(refused.contains("refused this launch 5 s ago in this boot"), "{refused}");
        assert!(b.refusal("U", "dev.simee.other", t0, || panic!("an app not listed never asks for the boot")).is_none());
        assert!(b.refusal("V", "dev.simee.tiny", t0, || panic!("nor does another device")).is_none());
        assert!(b.refusal("U", "dev.simee.tiny", t0 + Duration::from_secs(6), || Some(77)).is_none(), "rebooted: ask the device");
        assert!(b.refusal("U", "dev.simee.tiny", t0 + Duration::from_secs(7), || Some(41)).is_none(), "and forgotten");
    }

    #[test]
    fn a_remembered_refusal_is_asked_again_after_a_minute_or_when_the_device_is_down() {
        let mut b = BeingUpdated::default();
        let t0 = Instant::now();
        b.note("U", "dev.simee.tiny", &being_updated_error(), t0, || Some(41));
        assert!(b.refusal("U", "dev.simee.tiny", t0 + BEING_UPDATED_RECHECK, || Some(41)).is_none());
        b.note("U", "dev.simee.tiny", &being_updated_error(), t0, || Some(41));
        assert!(b.refusal("U", "dev.simee.tiny", t0, || None).is_none(), "shut down");
        b.note("U", "dev.simee.tiny", &being_updated_error(), t0, || None);
        assert!(b.0.is_empty(), "no boot to tie it to: nothing remembered");
    }

    #[test]
    fn only_springboards_being_updated_refusal_is_remembered() {
        let mut b = BeingUpdated::default();
        let t0 = Instant::now();
        let other = sim_coresim::Error::Service { op: "launch", message: "Simulator device failed to launch dev.simee.tiny.".into() };
        b.note("U", "dev.simee.tiny", &other, t0, || panic!("not asked for another error"));
        let install = sim_coresim::Error::Service { op: "install", message: "LaunchServicesDataMismatch".into() };
        b.note("U", "dev.simee.tiny", &install, t0, || Some(41));
        assert!(b.0.is_empty());
        assert!(is_being_updated(&being_updated_error()));
        assert!(!is_being_updated(&sim_coresim::Error::Invalid("LaunchServicesDataMismatch".into())));
    }

    #[test]
    fn the_slim_profile_keeps_what_automobile_needs() {
        let slim = profile("slim").expect("slim");
        let mut seen = std::collections::HashSet::new();
        for label in slim.disabled {
            assert!(seen.insert(label), "{label} twice");
            assert!(label.starts_with("com.apple.") && !label.contains(['\'', ' ', ';']), "{label}");
        }
        for kept in [
            "com.apple.SpringBoard",
            "com.apple.backboardd",
            "com.apple.InputUI",
            "com.apple.AccessibilityUIServer",
            "com.apple.mobile.installd",
            "com.apple.lsd",
            "com.apple.runningboardd",
            "com.apple.tccd",
            "com.apple.locationd",
            "com.apple.sharingd",
            "com.apple.WebBookmarks.webbookmarksd",
            "com.apple.assetsd",
            "com.apple.syslogd",
            "com.apple.usernotificationsd",
        ] {
            assert!(!slim.disabled.contains(&kept), "{kept} is needed");
        }
        assert_eq!(profile("tiny").expect_err("unknown"), "unknown profile `tiny` (slim)");
    }

    /// The in-device jobs simee's own methods reach (launchd labels of the iOS 27.2 runtime's
    /// LaunchDaemons and LaunchAngels), by method.
    const NEEDED_BY_METHODS: &[(&str, &[&str])] = &[
        ("carplay", &["com.apple.carkitd", "com.apple.CarPlayApp", "com.apple.SpringBoard"]),
        ("location", &["com.apple.locationd"]),
        ("privacy", &["com.apple.tccd"]),
        ("push", &["com.apple.apsd", "com.apple.usernotificationsd", "com.apple.SpringBoard"]),
        ("appearance", &["com.apple.SpringBoard", "com.apple.backboardd", "com.apple.cfprefsd.xpc.daemon"]),
        ("keyboard_language", &["com.apple.TextInput.kbd", "com.apple.InputUI", "com.apple.cfprefsd.xpc.daemon"]),
        ("hardware_keyboard", &["com.apple.InputUI", "com.apple.backboardd"]),
        ("type", &["com.apple.InputUI", "com.apple.TextInput.kbd", "com.apple.backboardd"]),
        ("open_url", &["com.apple.lsd", "com.apple.WebBookmarks.webbookmarksd", "com.apple.SpringBoard"]),
        ("biometric", &["com.apple.notifyd", "com.apple.CoreAuthentication.daemon", "com.apple.SpringBoard"]),
        ("status_bar", &["com.apple.SpringBoard", "com.apple.systemstatusd"]),
        ("clear_status_bar", &["com.apple.SpringBoard", "com.apple.systemstatusd"]),
        ("notify", &["com.apple.notifyd"]),
        ("memory_warning", &["com.apple.notifyd"]),
        ("reset_keychain", &["com.apple.securityd"]),
        ("defaults_read", &["com.apple.cfprefsd.xpc.daemon"]),
        ("defaults_write", &["com.apple.cfprefsd.xpc.daemon"]),
        ("install", &["com.apple.mobile.installd", "com.apple.lsd"]),
        ("launch", &["com.apple.runningboardd", "com.apple.SpringBoard"]),
        ("apps", &["com.apple.mobile.installd", "com.apple.lsd"]),
        ("logs", &["com.apple.syslogd", "com.apple.logd"]),
        ("network_shape", &["com.apple.configd_sim", "com.apple.nsurlsessiond"]),
        ("audio_volume", &["com.apple.audio.systemsoundserver-simd"]),
        ("content_size", &["com.apple.cfprefsd.xpc.daemon", "com.apple.SpringBoard"]),
    ];

    #[test]
    fn the_slim_profile_keeps_what_simee_methods_need() {
        let slim = profile("slim").expect("slim");
        for (method, jobs) in NEEDED_BY_METHODS {
            assert!(crate::methods::METHODS.iter().any(|(m, ..)| m == method), "`{method}` is a method");
            for job in *jobs {
                assert!(!slim.disabled.contains(job), "`{method}` needs {job}");
            }
        }
    }

    #[test]
    fn the_shaper_is_resolved_once_for_the_whole_plan() {
        let calls = std::cell::Cell::new(0);
        let resolve = || {
            calls.set(calls.get() + 1);
            Ok(vec![(sim_netshape::Sdk::Ios, std::path::PathBuf::from("/cache/netshape.dylib"))])
        };
        let plan = boot_plan(&json!({"shaped": true}), 8, true, resolve).expect("plan");
        assert_eq!(calls.get(), 1);
        let ios = "com.apple.CoreSimulator.SimRuntime.iOS-26-5";
        assert_eq!(shaper_for(&plan.shapers, ios), Ok(Path::new("/cache/netshape.dylib")), "every worker uses this path");
        let plan = boot_plan(&json!({}), 1, false, || Err("not built".into())).expect("unshaped plans don't build it");
        assert!(plan.shapers.is_empty());
        assert_eq!(boot_plan(&json!({"shaped": true}), 1, false, || Err("clang failed".into())).expect_err("build error"), "clang failed");
    }

    #[test]
    fn a_mixed_boot_many_gets_one_shaper_per_runtime_sdk() {
        use sim_netshape::Sdk;
        let rt = "com.apple.CoreSimulator.SimRuntime.";
        let (ios, tv, ios18) = (format!("{rt}iOS-26-5"), format!("{rt}tvOS-26-5"), format!("{rt}iOS-18-0"));
        let sdks = shaper_sdks([ios.as_str(), tv.as_str(), ios18.as_str()]).expect("sdks");
        assert_eq!(sdks, [Sdk::Ios, Sdk::Tvos], "each SDK once, the Apple TV's included");
        let shapers: Shapers = sdks.iter().map(|&s| (s, std::path::PathBuf::from(format!("/cache/{s:?}.dylib")))).collect();
        assert_eq!(shaper_for(&shapers, &tv), Ok(Path::new("/cache/Tvos.dylib")), "the Apple TV gets the tvOS build");
        assert_eq!(shaper_for(&shapers, &ios18), Ok(Path::new("/cache/Ios.dylib")));
        // A set with a runtime that has no shaper is refused before anything boots.
        assert!(shaper_sdks([ios.as_str(), &format!("{rt}watchOS-26-5")]).is_err());
        assert!(shaper_for(&shapers[..1], &tv).is_err(), "a device whose SDK wasn't built fails, not given the wrong build");
    }

    #[test]
    fn network_shape_preflights_first_and_only_on_a_booted_device() {
        use ShapeStep::{Inject, Preflight, Proxy, WriteProfile};
        // A shut-down device given inject "boot" is booted with the shaper: nothing runs in it first.
        assert_eq!(shape_steps("boot", false, false), [WriteProfile, Inject]);
        assert_eq!(shape_steps("boot", true, false), [WriteProfile, Proxy, Inject]);
        // On a booted device a shaper that won't load is refused before anything changes.
        assert_eq!(shape_steps("boot", true, true), [Preflight, WriteProfile, Proxy, Inject]);
        assert_eq!(shape_steps("launchd", true, true), [Preflight, WriteProfile, Proxy, Inject]);
        assert_eq!(shape_steps("launchd", false, true), [Preflight, WriteProfile, Inject]);
        // Without injection nothing is preflighted.
        assert_eq!(shape_steps("none", true, true), [WriteProfile, Proxy]);
        assert_eq!(shape_steps("none", false, false), [WriteProfile]);
    }

    #[test]
    fn boot_many_settles_by_default_with_a_timeout_for_its_concurrency() {
        let none = || Err("unused".to_owned());
        let plan = boot_plan(&json!({}), 8, true, none).expect("boot_many");
        assert!(plan.settle.is_some(), "boot_many settles unless told not to");
        assert_eq!(plan.settle_timeout, Duration::from_secs(1200));
        assert!(boot_plan(&json!({"settle": false}), 8, true, none).expect("no settle").settle.is_none());
        let plan = boot_plan(&json!({"timeout_s": 120, "settle": true}), 1, false, none).expect("boot");
        assert_eq!((plan.timeout, plan.settle_timeout), (Duration::from_secs(120), Duration::from_secs(600)), "separate budgets");
        assert_eq!(
            boot_plan(&json!({"settle": true, "settle_timeout_s": 30}), 1, false, none).expect("given").settle_timeout,
            Duration::from_secs(30)
        );
        assert!(boot_plan(&json!({}), 1, false, none).expect("boot").settle.is_none(), "boot settles only when asked");
    }

    #[test]
    fn a_profile_reports_what_launchctl_did() {
        let labels = ["com.apple.a", "com.apple.b", "com.apple.c", "com.apple.gone"];
        // As the iOS 27.2 runtime's launchctl prints them.
        let loaded = "PID\tStatus\tLabel\n12\t0\tcom.apple.a\n-\t0\tcom.apple.b\n13\t0\tcom.apple.c\n";
        let disabled = "\n\tdisabled services = {\n\t\t\"com.apple.a\" => disabled\n\t\t\"com.apple.b\" => enabled\n\
                        \t\t\"com.apple.gone\" => true\n\t\t\"com.apple.other\" => disabled\n\t}\n";
        // b: launchctl refused; c: it said nothing but launchd doesn't list it disabled; gone:
        // disabled (launchctl disables any label), but there was no such job loaded.
        assert_eq!(
            profile_outcome(&labels, loaded, &[], &["com.apple.b"], disabled, false),
            Applied {
                changed: 2,
                failed: vec!["com.apple.b".into(), "com.apple.c".into()],
                not_loaded: vec!["com.apple.gone".into()],
                already: 0
            }
        );
        // Undone: a label is enabled once launchd no longer lists it disabled.
        let undone = "\t\"com.apple.a\" => enabled\n\t\"com.apple.gone\" => disabled\n";
        let r = profile_outcome(&labels, "", &[], &[], undone, true);
        assert_eq!((r.changed, &r.failed), (3, &vec!["com.apple.gone".to_owned()]));
        assert_eq!(r.to_json(true), json!({"enabled": 3, "failed": ["com.apple.gone"]}));
        // No listings from launchctl: judged by what it refused alone.
        let r = profile_outcome(&labels, "", &[], &["com.apple.c"], "", false);
        assert_eq!((r.changed, &r.failed, r.not_loaded.len()), (3, &vec!["com.apple.c".to_owned()], 0));
        assert_eq!(r.to_json(false), json!({"disabled": 3, "failed": ["com.apple.c"], "not_loaded": [], "already": 0}));
        // What the runtime's launchctl prints when disabling a running job: a warning, not a failure.
        assert!(!launchctl_refused(
            "Warning: Please switch to user/foreground/com.apple.translationd service identifier (rdar://78126471)\n"
        ));
        assert!(!launchctl_refused(""));
        assert!(launchctl_refused("Could not find service \"x\" in domain for system\n"));
    }

    #[test]
    fn a_reapplied_profile_touches_only_the_jobs_that_came_back() {
        let labels = ["com.apple.a", "com.apple.companionappd", "com.apple.NPKCompanionAgent", "com.apple.new"];
        // A slimmed device after a reboot (as slim-launchctl-list / print-disabled read on iOS
        // 27.2): a is still off; the companions came back enabled and running; new was never
        // disabled (added to the profile since) and isn't running.
        let loaded = "PID\tStatus\tLabel\n66382\t0\tcom.apple.companionappd\n66387\t0\tcom.apple.NPKCompanionAgent\n\
                      12\t0\tcom.apple.SpringBoard\n";
        let disabled = "\tdisabled services = {\n\t\t\"com.apple.a\" => disabled\n\
                        \t\t\"com.apple.companionappd\" => enabled\n\t\t\"com.apple.NPKCompanionAgent\" => disabled\n\t}\n";
        assert_eq!(
            still_on(&labels, loaded, disabled),
            ["com.apple.companionappd", "com.apple.NPKCompanionAgent", "com.apple.new"],
            "re-enabled, loaded though disabled, and never disabled"
        );
        // A fresh device (nothing disabled) or a listing that couldn't be read: every job.
        assert_eq!(still_on(&labels, loaded, "\tdisabled services = {\n\t}\n"), labels);
        assert_eq!(still_on(&labels, "", disabled), labels);
        assert_eq!(still_on(&labels, loaded, ""), labels);
        // What was left alone counts as disabled and isn't reported as not loaded.
        let after = "\t\"com.apple.a\" => disabled\n\t\"com.apple.companionappd\" => disabled\n\
                     \t\"com.apple.NPKCompanionAgent\" => disabled\n\t\"com.apple.new\" => disabled\n";
        let r = profile_outcome(&labels, loaded, &["com.apple.a"], &[], after, false);
        assert_eq!(r, Applied { changed: 4, failed: vec![], not_loaded: vec!["com.apple.new".into()], already: 1 });
        assert_eq!(r.to_json(false)["already"], json!(1));
        assert_eq!(r.to_json(true).get("already"), None, "undo touches every job");
    }

    #[test]
    fn the_slim_profile_keeps_sleepd() {
        // Without sleepd, SpringBoard and mobiletimerd spin retrying its mach service on every
        // later boot and the device never settles (docs/farm.md).
        let slim = profile("slim").expect("slim");
        assert!(!slim.disabled.contains(&"com.apple.sleepd"));
        assert_eq!(slim.disabled.len(), 74);
        for companion in ["com.apple.companionappd", "com.apple.NPKCompanionAgent", "com.apple.brook.brookcompaniond"] {
            assert!(slim.disabled.contains(&companion), "{companion} stays in: re-applied on every boot");
        }
    }

    #[test]
    fn the_slim_profile_turns_off_what_a_fresh_clone_still_burns_cpu_on() {
        // Measured per launchd job in a slim clone's first 120 s (iOS 27.2 and 18.6, docs/farm.md):
        // linkd runs donotdisturbd's Focus-filter first-launch task, which starts the Calendar,
        // Safari and Messages App Intents extensions and Calendar itself 13-38 s after the boot,
        // the burst that held the settle window open; the rest are watch, account-sync, fitness
        // and Focus-status daemons with nothing to do on a test device.
        let slim = profile("slim").expect("slim");
        for label in [
            "com.apple.linkd",
            "com.apple.nanotimekitcompaniond",
            "com.apple.nanoprefsyncd.2",
            "com.apple.dataaccess.dataaccessd",
            "com.apple.Safari.passwordbreachd",
            "com.apple.activitysharingd",
            "com.apple.activityawardsd",
            "com.apple.fitcore",
            "com.apple.StatusKitAgent",
            "com.apple.avatarsd",
            "com.apple.modelcatalogd",
            "com.apple.familynotification",
        ] {
            assert!(slim.disabled.contains(&label), "{label}");
        }
        // Heavy too, but app flows reach them: notifications and Focus (push), Live Activities,
        // password AutoFill and passkeys, CallKit, EventKit, assets the keyboard and fonts load,
        // and the log stream `logs` reads.
        for kept in [
            "com.apple.donotdisturbd",
            "com.apple.liveactivitiesd",
            "com.apple.AuthenticationServicesCore.AuthenticationServicesAgent",
            "com.apple.telephonyutilities.callservicesd",
            "com.apple.calaccessd",
            "com.apple.mobileassetd",
            "com.apple.identityservicesd",
            "com.apple.diagnosticd",
        ] {
            assert!(!slim.disabled.contains(&kept), "{kept} is needed");
        }
    }

    #[test]
    fn the_slim_profile_turns_off_what_reenables_the_watch_daemons() {
        // nanoregistrylaunchd enables the 17 NanoLaunchDaemons whenever it runs; without it off,
        // the companions come back one to three minutes after every boot (docs/farm.md).
        let slim = profile("slim").expect("slim");
        for label in
            ["com.apple.nanoregistrylaunchd", "com.apple.addressbooksyncd", "com.apple.bulletindistributord", "com.apple.pairedsyncd"]
        {
            assert!(slim.disabled.contains(&label), "{label}");
        }
        let unique: std::collections::HashSet<_> = slim.disabled.iter().collect();
        assert_eq!(unique.len(), slim.disabled.len(), "no label twice");
    }

    #[test]
    fn a_preset_disables_the_profile_before_boot_and_keeps_other_overrides() {
        let dir = std::env::temp_dir().join(format!("simee-preset-{}", std::process::id()));
        let path = dir.join("com.apple.CoreSimulator.SimDevice.X").join("disabled.plist");
        let _ = std::fs::remove_dir_all(&dir);
        // A clone's first boot: no directory yet.
        assert_eq!(preset_disables(&path, &["a", "b"]), Ok(2));
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(path.parent().expect("dir")).expect("made").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "as launchd_sim makes it");
        }
        // A later boot: launchd (or nanoregistrylaunchd) enabled `b` and kept its own `c`.
        std::fs::write(&path, sim_core::launchd::render(&[("b".into(), false), ("c".into(), false)], &["a"], true)).expect("write");
        assert_eq!(preset_disables(&path, &["a", "b"]), Ok(1), "only b is new");
        let back = sim_core::launchd::parse(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back, [("b".to_owned(), true), ("c".to_owned(), false), ("a".to_owned(), true)]);
        assert_eq!(preset_disables(&path, &["a", "b"]), Ok(0), "nothing to write");
        // A file simee can't read is left alone.
        std::fs::write(&path, "bplist00").expect("write");
        assert!(preset_disables(&path, &["a"]).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "bplist00");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_temp_file_for_the_disables_is_never_a_planted_symlink() {
        let dir = std::env::temp_dir().join(format!("simee-write-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").expect("victim");
        let tmp = dir.join("disabled.plist.simee");
        std::os::unix::fs::symlink(&victim, &tmp).expect("plant");
        assert!(write_new(&tmp, &dir, b"x").is_err(), "an existing name, symlink or not, is refused");
        assert_eq!(std::fs::read_to_string(&victim).expect("read"), "keep", "not written through the link");
        // A dangling link too (O_EXCL fails on the link itself).
        std::fs::remove_file(&tmp).expect("rm");
        std::os::unix::fs::symlink(dir.join("missing"), &tmp).expect("plant dangling");
        assert!(write_new(&tmp, &dir, b"x").is_err());
        assert!(!dir.join("missing").exists());
        std::fs::remove_file(&tmp).expect("rm");
        // A free name works and has the bytes.
        write_new(&tmp, &dir, b"ok").expect("free name");
        assert_eq!(std::fs::read_to_string(&tmp).expect("read"), "ok");
        // A directory that is itself a symlink is refused, and leaves no file behind.
        let link = std::env::temp_dir().join(format!("simee-write-new-link-{}", std::process::id()));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).expect("dir link");
        let via = link.join("new");
        assert!(write_new(&via, &link, b"x").is_err());
        assert!(!dir.join("new").exists());
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_disables_are_not_written_into_another_users_directory() {
        // The sticky, world-writable /private/tmp (macOS) or /tmp is root's: anyone can create a
        // file in it, which is what a hostile directory looks like. Skipped as root (the owner).
        let dir = std::path::Path::new(if std::path::Path::new("/private/tmp").is_dir() { "/private/tmp" } else { "/tmp" });
        let tmp = dir.join(format!("simee-foreign-dir-{}", std::process::id()));
        match write_new(&tmp, dir, b"x") {
            Err(e) => {
                assert!(e.contains("isn't a directory owned by the current user"), "{e}");
                assert!(!tmp.exists(), "nothing left behind");
            }
            Ok(()) => {
                eprintln!("skipped: running as the directory's owner (root), no other uid to test with");
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// The two overrides directories of one device under a scratch root: (`/private/var/tmp`'s
    /// file, `/private/tmp`'s file).
    fn overrides_dirs(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("simee-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let var = root.join("var").join("disabled.plist");
        let tmp = root.join("tmp").join("disabled.plist");
        for f in [&var, &tmp] {
            std::fs::create_dir_all(f.parent().expect("dir")).expect("dir");
        }
        (root, var, tmp)
    }

    /// launchd_sim's log beside `plist`, last written `at`.
    fn launchd_log(plist: &Path, at: std::time::SystemTime) {
        let f = std::fs::File::create(plist.with_file_name("launchd.log")).expect("log");
        f.set_modified(at).expect("mtime");
    }

    #[test]
    fn a_boot_whose_launchd_read_every_disable_is_held_without_a_spawn() {
        let (root, var, tmp) = overrides_dirs("held");
        let labels = ["com.apple.a", "com.apple.b"];
        let started = std::time::SystemTime::now() - Duration::from_secs(2);
        let earlier = started - Duration::from_secs(60);
        let file = |entries: &[(&str, bool)]| {
            sim_core::launchd::render(&entries.iter().map(|(l, d)| ((*l).to_owned(), *d)).collect::<Vec<_>>(), &[], true)
        };
        // A runtime that logs in neither directory: the spawns, as before.
        std::fs::write(&var, file(&[("com.apple.a", true), ("com.apple.b", true)])).expect("write");
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "no log: can't tell which file launchd read");
        launchd_log(&var, earlier);
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "only an earlier boot's log");
        // iOS 18.6-27.2: launchd logged beside the preset this boot.
        launchd_log(&var, std::time::SystemTime::now());
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), Some(2), "all disabled");
        std::fs::write(&var, file(&[("com.apple.a", true), ("com.apple.b", false), ("com.apple.c", true)])).expect("write");
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "some: b enabled (nanoregistrylaunchd)");
        std::fs::write(&var, file(&[("com.apple.a", false)])).expect("write");
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "none");
        std::fs::write(&var, "bplist00").expect("write");
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "a file simee can't read");
        std::fs::remove_file(&var).expect("rm");
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "missing");
        // iOS 18.0: launchd read the boot option's file; an all-disabled preset beside an earlier
        // log is the file it ignored.
        std::fs::write(&var, file(&[("com.apple.a", true), ("com.apple.b", true)])).expect("write");
        launchd_log(&var, earlier);
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "no log this boot, and no option file");
        launchd_log(&tmp, std::time::SystemTime::now());
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None, "the option's file is missing");
        std::fs::write(&tmp, file(&[("com.apple.b", true), ("com.apple.a", true), ("com.apple.nano", false)])).expect("write");
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), Some(2));
        // Both logged this boot: neither file is trusted.
        launchd_log(&var, std::time::SystemTime::now());
        assert_eq!(preset_held_in(&var, &tmp, &labels, started), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_boot_option_keeps_the_last_boots_overrides_and_disables_the_profile() {
        let (root, _, tmp) = overrides_dirs("boot-jobs");
        // A clone: no file.
        assert_eq!(boot_jobs_from(&tmp, &["a", "b"]), [("a".to_owned(), true), ("b".to_owned(), true)]);
        // An iOS 18.0 reboot: the file launchd wrote, with an `enable` of its own and of a profile label.
        std::fs::write(&tmp, sim_core::launchd::render(&[("nano".into(), false), ("b".into(), false)], &["a"], true)).expect("write");
        assert_eq!(boot_jobs_from(&tmp, &["a", "b"]), [("nano".to_owned(), false), ("b".to_owned(), true), ("a".to_owned(), true)]);
        // Unreadable: the profile alone.
        std::fs::write(&tmp, "bplist00").expect("write");
        assert_eq!(boot_jobs_from(&tmp, &["a"]), [("a".to_owned(), true)]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_disabled_job_launchd_loads_anyway_is_left_alone_unless_it_runs() {
        // launchd_sim loads siri.context.service though it's disabled and re-creates it 0.25 s
        // after a bootout (iOS 27.2): stopping it again is two spawns for nothing.
        let labels = ["com.apple.siri.context.service", "com.apple.a"];
        let disabled = "\t\"com.apple.siri.context.service\" => disabled\n\t\"com.apple.a\" => disabled\n";
        let idle = "PID\tStatus\tLabel\n-\t0\tcom.apple.siri.context.service\n-\t0\tcom.apple.a\n12\t0\tcom.apple.SpringBoard\n";
        assert_eq!(still_on(&labels, idle, disabled), ["com.apple.a"], "another loaded job is still stopped");
        let running = "PID\tStatus\tLabel\n77\t0\tcom.apple.siri.context.service\n12\t0\tcom.apple.SpringBoard\n";
        assert_eq!(still_on(&labels, running, disabled), ["com.apple.siri.context.service"], "running: stopped");
        let enabled = "\t\"com.apple.siri.context.service\" => enabled\n\t\"com.apple.a\" => disabled\n";
        assert_eq!(still_on(&labels, idle, enabled), labels, "not disabled yet: disabled");
        let r = profile_outcome(&labels, idle, &["com.apple.siri.context.service"], &[], disabled, false);
        assert_eq!(r, Applied { changed: 2, failed: vec![], not_loaded: vec![], already: 1 });
    }

    #[test]
    fn settle_rules_read_their_keys() {
        let none = || -> Result<Shapers, String> { Err("unused".into()) };
        let plan = boot_plan(&json!({"settle": true}), 1, false, none).expect("boot");
        assert_eq!(plan.settle, Some(crate::settle::Rule::default()), "foreground CPU by default");
        let p = json!({"settle": true, "settle_cpu_pct": 5.0, "settle_window_s": 20, "settle_foreground": false});
        let rule = boot_plan(&p, 1, false, none).expect("boot").settle.expect("settle");
        assert_eq!(rule, crate::settle::Rule { cpu_pct: 5.0, window: Duration::from_secs(20), foreground: false });
        let rule = settle_rule(&json!({"cpu_pct": 7.0, "window_s": 3, "foreground": false}), "").expect("settle");
        assert_eq!(rule, crate::settle::Rule { cpu_pct: 7.0, window: Duration::from_secs(3), foreground: false });
        assert!(settle_rule(&json!({"settle_foreground": false}), "").expect("settle").foreground, "settle's own keys have no prefix");
        // A mistyped option is refused, not silently replaced by the default.
        for bad in [
            json!({"settle": true, "settle_foreground": "true"}),
            json!({"settle": true, "settle_foreground": 1}),
            json!({"settle": true, "settle_cpu_pct": "5"}),
            json!({"settle": true, "settle_window_s": -1}),
        ] {
            assert!(boot_plan(&bad, 1, false, none).is_err(), "{bad}");
        }
        assert!(settle_rule(&json!({"foreground": "yes"}), "").is_err());
    }

    #[test]
    fn a_settle_result_reports_background_and_foreground_cpu() {
        let mut settler = crate::settle::Settler::new(crate::settle::Rule::default());
        let tree = |total_s, background_s| crate::settle::Tree {
            cpu: [(1, crate::settle::Cpu { total_s, background_s })].into(),
            springboard: true,
        };
        settler.push(0.0, tree(0.0, 0.0));
        settler.push(10.0, tree(3.0, 2.0));
        let mut r = json!({});
        put_load(&mut r, &settler);
        assert_eq!(
            r,
            json!({"procs": 1, "cpu_pct": 30.0, "background_cpu_pct": 20.0, "foreground_cpu_pct": 10.0, "cpu_s": 3.0,
                   "background_cpu_s": 2.0})
        );
        let rule = crate::settle::Rule { foreground: true, ..Default::default() };
        assert_eq!(
            not_settled(&settler, rule, Duration::from_secs(60)),
            "device did not settle within 60s (10 % of a core in the foreground over the last 10s)"
        );
        assert_eq!(
            not_settled(&settler, crate::settle::Rule { foreground: false, ..Default::default() }, Duration::from_secs(60)),
            "device did not settle within 60s (30 % of a core over the last 10s)"
        );
    }

    #[test]
    fn a_missing_udid_is_reported_as_typed() {
        let lower = "0000aaaa-bbbb-cccc-dddd-eeeeffff0000";
        let upper = lower.to_ascii_uppercase();
        let missing = || format!("no device matches `{upper}`");
        assert_eq!(as_typed(missing(), &json!({"device": lower})), format!("no device matches `{lower}`"));
        assert_eq!(as_typed(missing(), &json!({"device": upper})), missing());
        assert_eq!(as_typed("other".into(), &json!({"device": "booted"})), "other");
        assert_eq!(as_typed(missing(), &json!({})), missing());
    }

    #[test]
    fn a_pid_must_be_an_integer_and_excludes_bundle_id() {
        let tree = [10, 20];
        assert_eq!(app_pid(&tree, &json!({"pid": 20})), Ok(20));
        assert_eq!(app_pid(&tree, &json!({"pid": "20"})), Err("pid must be an integer".into()));
        assert_eq!(app_pid(&tree, &json!({"pid": 2.5})), Err("pid must be an integer".into()));
        assert_eq!(app_pid(&tree, &json!({"bundle_id": "a.b", "pid": "x"})), Err("pid must be an integer".into()));
        assert_eq!(app_pid(&tree, &json!({"bundle_id": "a.b", "pid": 20})), Err("give `bundle_id` or `pid`, not both".into()));
        assert_eq!(app_pid(&tree, &json!({})), Err("give `bundle_id` or `pid`".into()));
    }
}
