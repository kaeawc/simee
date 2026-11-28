//! The Android backend: device methods on AVDs and running simee instances, through
//! `sim-android`. Builds on every host; booting needs an Apple silicon Mac (the emulator does).

use std::thread::sleep;
use std::time::Duration;

use serde_json::{Value, json};
use sim_android::Android;
use sim_core::device::DeviceInfo;
use sim_core::fold::StockPosture;
use sim_core::form_factor::Posture;
use sim_core::hid::Direction;
use sim_core::xr::XrCommand;

use crate::params::{
    button, need_str, opt_bool, opt_u64, png_result, reading_json, str_param, swipe_samples, tap_samples, touch_sample, vehicle_area,
    vehicle_value,
};

/// The SDK and AVD home, read from the environment on first use.
#[derive(Default)]
pub struct Session {
    android: Option<Android>,
}

impl Session {
    pub fn with(android: Android) -> Session {
        Session { android: Some(android) }
    }

    pub fn backend(&mut self) -> &Android {
        self.android.get_or_insert_with(Android::from_env)
    }

    pub fn devices(&mut self) -> Vec<DeviceInfo> {
        self.backend().devices()
    }
}

/// How long `tap` and `button` hold without `hold_ms` (iOS has its own, measured: `ios.rs`).
const TAP_HOLD: Duration = Duration::from_millis(60);
const BUTTON_HOLD: Duration = Duration::from_millis(80);

/// `boot` and `boot_many` parameters only iOS devices take, and why.
const IOS_BOOT_PARAMS: &[(&str, &str)] = &[
    ("settle", "a settle wait reads a simulator's process tree; an Android boot waits for sys.boot_completed"),
    ("settle_cpu_pct", "it tunes the settle wait"),
    ("settle_window_s", "it tunes the settle wait"),
    ("settle_timeout_s", "it tunes the settle wait"),
    ("profile", "profiles are sets of a simulator's launchd jobs"),
    ("shaped", "the network shaper is injected into a simulator's processes"),
    ("env", "it is a simulator launchd's environment"),
];

/// `launch` parameters only iOS devices take, and why.
const IOS_LAUNCH_PARAMS: &[(&str, &str)] = &[
    ("args", "an Android app starts from an intent, not a command line"),
    ("env", "an Android app starts from an intent, with the environment its zygote gives it"),
    ("shaped", "the network shaper is injected into a simulator's processes"),
    ("malloc_stack_logging", "MallocStackLogging is a simulator process's environment"),
];

/// Rejects iOS-only boot parameters that ask for something (a `false`, `null`, empty or absent
/// one asks for nothing), naming the platform that has them.
fn reject_ios_boot_params(method: &str, p: &Value) -> Result<(), String> {
    reject_ios_params(method, p, IOS_BOOT_PARAMS)
}

/// [`reject_ios_boot_params`] for any table of iOS-only parameters.
fn reject_ios_params(method: &str, p: &Value, params: &[(&str, &str)]) -> Result<(), String> {
    for (key, why) in params {
        let asks = match p.get(*key) {
            None | Some(Value::Null | Value::Bool(false)) => false,
            Some(Value::Object(m)) => !m.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(_) => true,
        };
        if asks {
            return Err(format!("`{method}` takes `{key}` for iOS devices only ({why})"));
        }
    }
    Ok(())
}

/// The app methods, over adb: the parameters are read before anything runs, so a missing one is
/// reported whether or not the device is up. Results are iOS's: nothing for `install`,
/// `uninstall`, `terminate` and `open_url`, `{pid}` for `launch` (null when the app's process
/// isn't named for its package) with the launcher `activity` it started, the package names for `apps`.
fn app(android: &Android, d: &sim_android::Device, method: &str, p: &Value) -> Result<Value, String> {
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "install" => {
            let path = need_str(p, "path")?;
            null(d.adb(android)?.install(std::path::Path::new(path)))
        }
        "uninstall" => {
            let package = need_str(p, "bundle_id")?;
            null(d.adb(android)?.uninstall(package))
        }
        "launch" => {
            let package = need_str(p, "bundle_id")?;
            reject_ios_params(method, p, IOS_LAUNCH_PARAMS)?;
            let restart = opt_bool(p, "restart")?.unwrap_or(true);
            d.adb(android)?.launch(package, restart).map(|l| json!({"pid": l.pid, "activity": l.activity}))
        }
        "terminate" => {
            let package = need_str(p, "bundle_id")?;
            null(d.adb(android)?.terminate(package))
        }
        "apps" => {
            let all = opt_bool(p, "all")?.unwrap_or(false);
            d.adb(android)?.apps(all).map(|names| json!(names))
        }
        _ => {
            let url = need_str(p, "url")?;
            null(d.adb(android)?.open_url(url))
        }
    }
}

/// The longest `exec` `timeout_s`: a day.
const MAX_EXEC_TIMEOUT_S: u64 = 86_400;

/// `processes`, `kill` and `exec`, over adb, in iOS's shapes: `processes` rows with each app
/// process's `package` too, nothing for `kill` (a signal to `pid`, or with `bundle_id` an app stop:
/// `am force-stop`), and `exec`'s `{pid, stdout, stderr}` with `exit_code` and `truncated`.
fn debug(android: &Android, d: &sim_android::Device, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "processes" => Ok(d
            .adb(android)?
            .processes()?
            .into_iter()
            .map(|r| {
                json!({"pid": r.pid, "ppid": r.ppid, "cpu": r.cpu, "rss_kb": r.rss_kb, "elapsed": r.elapsed, "name": r.name,
                    "path": r.path, "package": r.package})
            })
            .collect()),
        "kill" => {
            let pid = match p.get("pid") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64().ok_or("pid must be an integer")?),
            };
            let force = opt_bool(p, "force")?.unwrap_or(false);
            match (pid, str_param(p, "bundle_id")) {
                (Some(_), Some(_)) => Err("give `pid` or `bundle_id`, not both".into()),
                (None, Some(package)) => d.adb(android)?.terminate(package).map(|()| Value::Null),
                (Some(pid), None) => {
                    let signal = p.get("signal").and_then(Value::as_i64).unwrap_or(15);
                    d.adb(android)?.kill(pid, signal, force).map(|()| Value::Null)
                }
                (None, None) => Err("give `pid` (a signal) or `bundle_id` (an app stop)".into()),
            }
        }
        _ => {
            let program = need_str(p, "program")?;
            // Unlike iOS's, a value that isn't a string is refused: dropping it would change the argv.
            let args = match p.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(v) => v
                    .as_array()
                    .and_then(|a| a.iter().map(|w| w.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>())
                    .ok_or("`args` must be a list of strings")?,
            };
            let env = match p.get("env") {
                None | Some(Value::Null) => Vec::new(),
                Some(v) => v
                    .as_object()
                    .and_then(|o| o.iter().map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect::<Option<Vec<_>>>())
                    .ok_or("`env` must be an object of strings (names to values)")?,
            };
            let defaults = sim_android::procs::ExecLimits::default();
            let at_least_1 = |key: &str| match opt_u64(p, key) {
                Some(0) => Err(format!("{key} must be at least 1")),
                n => Ok(n),
            };
            let timeout = match at_least_1("timeout_s")? {
                Some(s) if s > MAX_EXEC_TIMEOUT_S => return Err(format!("timeout_s must be at most {MAX_EXEC_TIMEOUT_S} (a day)")),
                s => s,
            };
            let limits = sim_android::procs::ExecLimits {
                timeout: timeout.map_or(defaults.timeout, Duration::from_secs),
                max_bytes: at_least_1("max_bytes")?.map_or(defaults.max_bytes, |n| usize::try_from(n).unwrap_or(usize::MAX)),
            };
            let out = d.adb(android)?.exec(program, &args, &env, limits)?;
            Ok(json!({"pid": out.pid, "stdout": out.stdout, "stderr": out.stderr, "exit_code": out.exit_code, "truncated": out.truncated}))
        }
    }
}

/// `fold`: `folded` (the console's `fold` or `unfold`), a hinge `angle`, or a `posture` (stock's
/// names or ids: closed, half-opened, opened, flipped, tent); a `null` one is absent. Answers
/// with the hinge as simee's emulator then has it (angle, posture, whether folded, the display
/// size the guest lays out for), or, from one that doesn't say (the stock emulator), what was
/// asked.
fn fold(d: &sim_android::Device, p: &Value) -> Result<Value, String> {
    const POSTURES: &str = "closed, half-opened, opened, flipped, tent";
    let asked = match (opt_bool(p, "folded")?, p.get("angle").and_then(Value::as_f64), str_param(p, "posture")) {
        (Some(folded), ..) => {
            d.fold_display(folded)?;
            json!({"folded": folded})
        }
        (None, Some(a), _) => {
            d.fold(a as f32)?;
            json!({"angle": a as f32, "posture": Posture::from_angle(a as f32)})
        }
        (None, None, Some(name)) => {
            let posture = StockPosture::parse(name).ok_or_else(|| format!("unknown posture `{name}` ({POSTURES})"))?;
            d.posture(posture)?;
            json!({"angle": Posture::parse(name).map(Posture::angle), "posture": posture.name()})
        }
        (None, None, None) => {
            return Err(format!("give `folded` (true|false), `angle` (0-180; 0-360 on a dual-screen) or `posture` ({POSTURES})"));
        }
    };
    Ok(match d.fold_state()? {
        Some(s) => json!({
            "angle": s.angle,
            "posture": s.posture.name(),
            "folded": s.folded,
            "width": s.width,
            "height": s.height,
            "postures": s.postures.iter().map(|p| p.name()).collect::<Vec<_>>(),
        }),
        None => asked,
    })
}

/// The first console port `boot_many` hands out: `first_port` (default 5554), even and in the
/// emulators' range.
fn first_port(p: &Value) -> Result<u16, String> {
    let ports = sim_android::emu_ports();
    match opt_u64(p, "first_port") {
        None => Ok(*ports.start()),
        Some(n) => u16::try_from(n)
            .ok()
            .filter(|n| ports.contains(n))
            .ok_or_else(|| format!("first_port must be a console port from {} to {}", ports.start(), ports.end())),
    }
}

/// How long a boot waits for `sys.boot_completed` (`timeout_s`, default 300 s), and whether the
/// device plays on the host (`audio`, default false: a farm booting N devices would open N host
/// streams).
pub(crate) fn boot_options(p: &Value) -> Result<(Duration, bool), String> {
    Ok((Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300)), opt_bool(p, "audio")?.unwrap_or(false)))
}

/// `boot_many`'s boot timeout and first console port.
pub(crate) fn boot_many_options(p: &Value) -> Result<(Duration, u16), String> {
    Ok((Duration::from_secs(opt_u64(p, "timeout_s").unwrap_or(300)), first_port(p)?))
}

/// `import`'s `allow_mismatch` (default false: only the exported revision of the system image).
pub(crate) fn import_allows_mismatch(p: &Value) -> Result<bool, String> {
    Ok(opt_bool(p, "allow_mismatch")?.unwrap_or(false))
}

/// One device's [`sim_android::BootOutcome`] in `boot_many`'s result: the same shape as an iOS
/// device's (`device`, `boot_s`, or `error`), with the console port and adb serial.
fn outcome_json(o: &sim_android::BootOutcome) -> Value {
    let serial = format!("emulator-{}", o.console_port);
    match &o.result {
        Ok(took) => {
            json!({"device": o.avd, "boot_s": (took.as_secs_f64() * 10.0).round() / 10.0, "console_port": o.console_port, "serial": serial})
        }
        Err(e) => json!({"device": o.avd, "console_port": o.console_port, "error": e}),
    }
}

/// A [`crate::progress::BootHook`] as the Android window's hook: each outcome as its `devices` entry.
struct Outcomes<'a>(&'a dyn crate::progress::BootHook);

impl sim_android::BootHook for Outcomes<'_> {
    fn start(&self, i: usize) -> Result<(), String> {
        self.0.start(i)
    }

    fn launching(&self, i: usize) -> Result<(), String> {
        self.0.launching(i)
    }

    fn finished(&self, i: usize, outcome: &sim_android::BootOutcome) {
        self.0.finished(i, &outcome_json(outcome));
    }
}

/// `boot_many` for AVDs (`udids`, already resolved): [`Android::boot_many_hooked`], at most
/// `concurrency` emulators starting at once, each on its own console port, with `hook` asked and
/// told about each.
pub(crate) fn boot_many(s: &mut Session, p: &Value, udids: &[String], hook: &dyn crate::progress::BootHook) -> Result<Value, String> {
    reject_ios_boot_params("boot_many", p)?;
    if udids.is_empty() {
        return Err("give `devices`: a list of AVD names".into());
    }
    let concurrency = crate::params::concurrency(p)?;
    let (timeout, first) = boot_many_options(p)?;
    let start = std::time::Instant::now();
    let outcomes = s.backend().boot_many_hooked(udids, concurrency, timeout, first, &Outcomes(hook))?;
    let devices: Vec<Value> = outcomes.iter().map(outcome_json).collect();
    let failed = outcomes.iter().filter(|o| o.result.is_err()).count();
    Ok(json!({"concurrency": concurrency, "all_s": (start.elapsed().as_secs_f64() * 10.0).round() / 10.0, "failed": failed,
        "unsettled": 0, "devices": devices}))
}

/// `import`: the bundle at `from` as a new AVD `name`, its system image found by content (with
/// `allow_mismatch`, another revision of it). The result says that the import keeps the exported
/// device's guest identity, which `config.ini` keys it dropped, and any revision mismatch.
pub fn import(s: &mut Session, p: &Value) -> Result<Value, String> {
    let (from, name) = (need_str(p, "from")?, need_str(p, "name")?);
    let allow_mismatch = import_allows_mismatch(p)?;
    let (info, im) = s.backend().import_avd(std::path::Path::new(from), name, allow_mismatch)?;
    Ok(json!({
        "device": info,
        "system_image": im.sysdir,
        "mismatch": im.mismatch,
        "dropped": im.dropped,
        "extracted": im.extracted,
        "exported_as": im.manifest.device.name,
        "guest_identity": im.manifest.guest_identity,
        "note": sim_android::bundle::DUPLICATED_IDENTITY,
        "resume": im.manifest.resume,
    }))
}

/// `contrast_check` on an AVD: its screen and its density; the appearance isn't switchable yet.
struct ContrastTarget<'a>(&'a sim_android::Device);

impl crate::contrast::Target for ContrastTarget<'_> {
    fn capture(&mut self) -> Result<sim_core::frame::Frame, String> {
        self.0.screenshot()
    }

    fn scale(&mut self) -> Option<f64> {
        use sim_core::form_factor::Density;
        Some(match self.0.profile().ok()?.density {
            Density::Dpi(dpi) => f64::from(dpi) / 160.0,
            Density::Scale(n) => f64::from(n),
        })
    }

    fn set_appearance(&mut self, _: crate::contrast::Mode) -> Result<(), String> {
        Err("appearance switching isn't available on Android devices yet".into())
    }
}

/// Dispatches one device method for the AVD named `udid`.
pub fn call(s: &mut Session, method: &str, p: &Value, udid: &str) -> Result<Value, String> {
    let android = s.backend().clone();
    let d = android.device(udid)?;
    match crate::params::display(p)? {
        sim_core::duo::Display::Active => {}
        sim_core::duo::Display::CarPlay => {
            return Err("display=carplay is an iPhone's CarPlay screen; Android devices have one display".into());
        }
        sim_core::duo::Display::Builtin(s) => {
            return Err(format!("display={} is an iPhone Duo's screen; an Android foldable shows the screen `fold` sets", s.as_str()));
        }
    }
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "delete" => null(android.delete(&d)),
        "boot" => {
            reject_ios_boot_params(method, p)?;
            let (timeout, audio) = boot_options(p)?;
            null(d.boot_asking(&android, timeout, audio, &crate::progress::launching))
        }
        "clone" => {
            if d.running.is_some() {
                return Err(format!("{} must be shut down to clone it", d.info.name));
            }
            let info = android.clone_avd(&d.info.udid, need_str(p, "name")?)?;
            serde_json::to_value(info).map_err(|e| e.to_string())
        }
        "export" => {
            let to = need_str(p, "to")?;
            let ex = android.export_avd(&d, std::path::Path::new(to))?;
            let m = &ex.manifest;
            Ok(json!({
                "device": d.info.udid,
                "to": to,
                "summary": ex.summary,
                "identity": m.identity,
                "captures": m.captures,
                "resume": m.resume,
                "secrets": m.secrets,
                "guest_identity": m.guest_identity,
                "dropped": m.dropped,
            }))
        }
        "shutdown" => null(d.shutdown()),
        // The guest's layout, which tap, swipe and touch take (a folded foldable's outer screen).
        "screen" => d.layout_size().map(|(w, h)| json!({"width": w, "height": h})),
        // The emulator's own PNG, passed through: with `path` it writes the file itself.
        "screenshot" => match str_param(p, "path") {
            Some(path) => d.screenshot_png_to(std::path::Path::new(path)).map(|(w, h)| json!({"path": path, "width": w, "height": h})),
            None => d.screenshot_png().and_then(|(w, h, png)| png_result(w, h, png, p)),
        },
        "contrast_check" => crate::contrast::run(&mut ContrastTarget(&d), p),
        "tap" => null(d.play(&tap_samples(p, TAP_HOLD)?)),
        "swipe" => null(d.play(&swipe_samples(p)?)),
        "touch" => {
            let (at, phase) = touch_sample(p)?;
            null(d.touch(at.x, at.y, phase))
        }
        "button" => {
            let (button, hold) = button(p, BUTTON_HOLD)?;
            d.button(button, Direction::Down)?;
            sleep(hold);
            null(d.button(button, Direction::Up))
        }
        "type" => null(d.type_text(need_str(p, "text")?)),
        "key" => {
            let usage = opt_u64(p, "usage").ok_or("missing number parameter `usage`")? as u32;
            d.key(usage, Direction::Down)?;
            sleep(Duration::from_millis(15));
            null(d.key(usage, Direction::Up))
        }
        "install" | "uninstall" | "launch" | "terminate" | "apps" | "open_url" => app(&android, &d, method, p),
        "processes" | "kill" | "exec" => debug(&android, &d, method, p),
        "location" => {
            let (lat, lon) = (crate::params::need_f64(p, "latitude")?, crate::params::need_f64(p, "longitude")?);
            null(d.set_location(lat, lon))
        }
        "info" => {
            let avd = d.avd.as_ref();
            let running = d
                .running
                .as_ref()
                .map(|r| json!({"serial": r.serial(), "console_port": r.console_port, "adb_port": r.adb_port, "pid": r.pid, "emulator": r.emulator()}));
            Ok(json!({
                "device": d.info,
                "avd_dir": avd.map(|a| a.dir.display().to_string()),
                "system_image": avd.map(|a| a.sysdir.display().to_string()),
                "profile": d.profile().ok(),
                "running": running,
            }))
        }
        "console" => {
            let commands: Vec<String> = match (str_param(p, "command"), p.get("commands").and_then(Value::as_array)) {
                (Some(c), _) => vec![c.to_owned()],
                (None, Some(list)) => list.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
                (None, None) => return Err("give `command` or `commands`".into()),
            };
            d.console(&commands).map(|lines| json!(lines))
        }
        "rotate" => match crate::params::opt_orientation(p)? {
            None => null(d.rotate()),
            Some(_) => Err("Android's rotate turns 90° clockwise per call; `orientation` is iOS-only".into()),
        },
        "fold" => fold(&d, p),
        "rotary" => {
            let ticks = p.get("ticks").and_then(Value::as_i64).ok_or("missing number parameter `ticks`")?;
            null(d.rotary(i32::try_from(ticks).map_err(|e| e.to_string())?))
        }
        "touchpad" => {
            let coord = |k: &str| -> Result<Option<u32>, String> {
                p.get(k).map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or(format!("`{k}` must be a whole number"))).transpose()
            };
            let at = |k: &str| coord(k)?.ok_or(format!("missing number parameter `{k}`"));
            let (x, y) = (at("x")?, at("y")?);
            match (coord("to_x")?, coord("to_y")?) {
                (Some(tx), Some(ty)) => {
                    let ms = p.get("duration_ms").and_then(Value::as_u64).unwrap_or(300);
                    null(d.touchpad_swipe((x, y), (tx, ty), std::time::Duration::from_millis(ms)))
                }
                (None, None) => {
                    let phase = str_param(p, "phase").unwrap_or("tap");
                    if !["down", "move", "up", "tap"].contains(&phase) {
                        return Err(format!("unknown phase `{phase}` (down, move, up, tap)"));
                    }
                    null(d.touchpad(x, y, phase))
                }
                _ => Err("a swipe needs both `to_x` and `to_y`".into()),
            }
        }
        "stylus" => {
            let (samples, duration) = crate::params::stylus_samples(p)?;
            null(d.stylus(&samples, duration))
        }
        "xr" => {
            let command: XrCommand = serde_json::from_value(p.clone()).map_err(|e| format!("xr: {e} ({})", sim_core::xr::USAGE))?;
            d.xr(&command).map(|lines| json!(lines))
        }
        "vehicle" => {
            let area = vehicle_area(p)?;
            match (str_param(p, "property"), vehicle_value(p)?) {
                (None, None) => d.vehicle_list().map(|all| all.iter().map(reading_json).collect()),
                (None, Some(_)) => Err("give the `property` to set".into()),
                (Some(prop), None) => d.vehicle_get(prop, area).map(|r| reading_json(&r)),
                (Some(prop), Some(value)) => null(d.vehicle_set(prop, area, &value)),
            }
        }
        other => Err(format!("`{other}` isn't implemented for Android devices")),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::{Value, json};
    use sim_android::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator};

    use crate::methods::{Session, call};

    /// A session over a running AVD `phone` whose adb is the fake. Keep the returned emulator:
    /// it answers the console for as long as it lives.
    fn session(tag: &str) -> (Session, std::path::PathBuf, FakeAdb, FakeEmulator) {
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-app-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/x86_64");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;x86_64", None).expect("create");
        let adb = fake_adb(&android);
        let emulator = fake_emulator(&android, "phone");
        (Session::with_android(android), root, adb, emulator)
    }

    fn run(s: &mut Session, method: &str, p: Value) -> Result<Value, String> {
        let mut p = p;
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(s, method, &p)
    }

    #[test]
    fn help_lists_both_platforms_for_the_app_methods() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        for method in ["install", "uninstall", "launch", "terminate", "apps", "open_url"] {
            let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == method)).expect(method);
            assert_eq!(row["platforms"], json!(["android", "ios"]), "{method}");
        }
    }

    #[test]
    fn the_app_methods_answer_as_on_ios() {
        let (mut s, root, adb, _emulator) = session("shapes");
        let apk = root.join("app.apk");
        std::fs::write(&apk, b"PK").expect("apk");
        assert_eq!(run(&mut s, "install", json!({"path": apk})), Ok(Value::Null));
        assert_eq!(
            run(&mut s, "launch", json!({"bundle_id": "com.example.app"})),
            Ok(json!({"pid": 4321, "activity": "com.example.app/.MainActivity"}))
        );
        assert_eq!(run(&mut s, "apps", json!({})), Ok(json!(["com.example.app", "com.example.zeta"])));
        assert_eq!(
            run(&mut s, "apps", json!({"all": true})),
            Ok(json!(["android", "com.android.settings", "com.example.app", "com.example.zeta"]))
        );
        assert_eq!(run(&mut s, "open_url", json!({"url": "https://example.com/"})), Ok(Value::Null));
        assert_eq!(run(&mut s, "terminate", json!({"bundle_id": "com.example.app"})), Ok(Value::Null));
        assert_eq!(run(&mut s, "uninstall", json!({"bundle_id": "com.example.app"})), Ok(Value::Null));
        let calls = adb.calls();
        assert_eq!(calls[0], format!("install -r {}", apk.display()));
        assert_eq!(
            calls[2],
            "shell am start -W -S -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n 'com.example.app/.MainActivity'"
        );
        assert_eq!(
            calls[3..],
            [
                "shell pidof -s com.example.app",
                "shell pm list packages -3",
                "shell pm list packages",
                "shell am start -a android.intent.action.VIEW -d 'https://example.com/'",
                "shell am force-stop com.example.app",
                "shell pm uninstall com.example.app",
            ]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn launch_restarts_unless_restart_is_false() {
        let (mut s, root, adb, _emulator) = session("restart");
        run(&mut s, "launch", json!({"bundle_id": "com.example.app", "restart": false})).expect("front");
        run(&mut s, "launch", json!({"bundle_id": "com.example.app", "restart": null})).expect("default");
        let starts: Vec<String> = adb.calls().into_iter().filter(|c| c.starts_with("shell am start")).collect();
        assert!(starts[0].starts_with("shell am start -W -a "), "no -S when restart is false: {starts:?}");
        assert!(starts[1].starts_with("shell am start -W -S "), "-S by default, as iOS terminates first: {starts:?}");
        assert!(run(&mut s, "launch", json!({"bundle_id": "com.example.app", "restart": "no"})).is_err());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn launch_refuses_what_only_ios_has() {
        let (mut s, root, adb, _emulator) = session("iosonly");
        for (key, value) in
            [("args", json!(["-x"])), ("env", json!({"A": "b"})), ("shaped", json!(true)), ("malloc_stack_logging", json!(true))]
        {
            let err = run(&mut s, "launch", json!({"bundle_id": "com.example.app", key: value})).expect_err(key);
            assert!(err.starts_with(&format!("`launch` takes `{key}` for iOS devices only")), "{err}");
        }
        // Asking for nothing is fine.
        run(&mut s, "launch", json!({"bundle_id": "com.example.app", "args": [], "env": {}, "shaped": false})).expect("empty");
        assert_eq!(adb.calls().len(), 3, "only the last call ran adb");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn processes_kill_and_exec_work_on_both_platforms() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        for method in ["processes", "kill", "exec"] {
            let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == method)).expect(method);
            assert_eq!(row["platforms"], json!(["android", "ios"]), "{method}");
        }
    }

    #[test]
    fn processes_answers_with_ios_rows_and_each_apps_package() {
        let (mut s, root, _adb, _emulator) = session("procs");
        let rows = run(&mut s, "processes", json!({})).expect("processes");
        let rows = rows.as_array().expect("rows");
        assert_eq!(
            rows[0],
            json!({"pid": 4321, "ppid": 310, "cpu": 7.9, "rss_kb": 120_332, "elapsed": "00:42", "name": "com.example.app",
                "path": "com.example.app", "package": "com.example.app"})
        );
        let sf = rows.iter().find(|r| r["pid"] == 520).expect("surfaceflinger");
        assert_eq!((&sf["path"], &sf["package"]), (&json!("/system/bin/surfaceflinger"), &Value::Null));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn kill_signals_stops_an_app_and_guards_the_essential_processes() {
        let (mut s, root, adb, _emulator) = session("kill");
        assert_eq!(run(&mut s, "kill", json!({"pid": 4321})), Ok(Value::Null));
        assert_eq!(run(&mut s, "kill", json!({"pid": 4400, "signal": 9})), Ok(Value::Null));
        let err = run(&mut s, "kill", json!({"pid": 612})).expect_err("system_server");
        assert!(err.contains("system_server") && err.contains("force"), "{err}");
        assert_eq!(run(&mut s, "kill", json!({"pid": 612, "force": true})), Ok(Value::Null));
        // An app stop rather than a signal.
        assert_eq!(run(&mut s, "kill", json!({"bundle_id": "com.example.app"})), Ok(Value::Null));
        let signals: Vec<String> = adb.calls().into_iter().filter(|c| !c.starts_with("shell ps ")).collect();
        assert_eq!(signals, ["shell kill -15 4321", "shell kill -9 4400", "shell kill -15 612", "shell am force-stop com.example.app"]);
        assert_eq!(
            run(&mut s, "kill", json!({"pid": 4321, "bundle_id": "com.example.app"})),
            Err("give `pid` or `bundle_id`, not both".into())
        );
        assert_eq!(run(&mut s, "kill", json!({})), Err("give `pid` (a signal) or `bundle_id` (an app stop)".into()));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn exec_runs_an_argv_with_a_timeout_and_a_cap() {
        let (mut s, root, _adb, _emulator) = session("exec");
        let out =
            run(&mut s, "exec", json!({"program": "sh", "args": ["-c", "echo \"$1\"; echo e >&2; exit 4", "sh", "a 'b'"]})).expect("exec");
        assert!(out["pid"].as_i64().is_some_and(|p| p > 0), "{out}");
        assert_eq!(
            (&out["stdout"], &out["stderr"], &out["exit_code"], &out["truncated"]),
            (&json!("a 'b'\n"), &json!("e\n"), &json!(4), &json!(false))
        );
        let env = run(&mut s, "exec", json!({"program": "sh", "args": ["-c", "echo $SIMEE_Y"], "env": {"SIMEE_Y": "z"}})).expect("env");
        assert_eq!(env["stdout"], "z\n");
        let capped = run(&mut s, "exec", json!({"program": "yes", "max_bytes": 10})).expect("capped");
        assert_eq!(
            (&capped["stdout"], &capped["truncated"], &capped["exit_code"]),
            (&json!("y\ny\ny\ny\ny\n"), &json!(true), &Value::Null)
        );
        let err = run(&mut s, "exec", json!({"program": "sleep", "args": ["5"], "timeout_s": 1})).expect_err("timeout");
        assert!(err.starts_with("`sleep` timed out after 1s"), "{err}");
        assert_eq!(run(&mut s, "exec", json!({})), Err("missing string parameter `program`".into()));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn exec_refuses_parameters_it_cant_pass_through() {
        let (mut s, root, adb, _emulator) = session("execparams");
        for args in [json!(["a", 1]), json!("a"), json!([null])] {
            assert_eq!(run(&mut s, "exec", json!({"program": "echo", "args": args})), Err("`args` must be a list of strings".into()));
        }
        for env in [json!({"A": 1}), json!(["A=1"])] {
            assert_eq!(
                run(&mut s, "exec", json!({"program": "echo", "env": env})),
                Err("`env` must be an object of strings (names to values)".into())
            );
        }
        for timeout in [u64::MAX, 86_401] {
            assert_eq!(
                run(&mut s, "exec", json!({"program": "true", "timeout_s": timeout})),
                Err("timeout_s must be at most 86400 (a day)".into())
            );
        }
        assert!(adb.calls().is_empty(), "{:?}", adb.calls());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn failures_reach_the_caller() {
        let (mut s, root, _adb, _emulator) = session("fail");
        let err = run(&mut s, "uninstall", json!({"bundle_id": "com.missing.app"})).expect_err("not installed");
        assert_eq!(err, "adb failed: Failure [DELETE_FAILED_INTERNAL_ERROR]");
        let err = run(&mut s, "launch", json!({"bundle_id": "com.missing.app"})).expect_err("no launcher");
        assert!(err.contains("has no launcher activity"), "{err}");
        let err = run(&mut s, "install", json!({"path": root.join("absent.apk")})).expect_err("absent");
        assert!(err.ends_with("isn't a file"), "{err}");
        let err = run(&mut s, "open_url", json!({"url": "nowhere://x"})).expect_err("no handler");
        assert!(err.contains("Activity not started"), "{err}");
        for (method, param) in
            [("install", "path"), ("uninstall", "bundle_id"), ("launch", "bundle_id"), ("terminate", "bundle_id"), ("open_url", "url")]
        {
            assert_eq!(run(&mut s, method, json!({})), Err(format!("missing string parameter `{param}`")), "{method}");
        }
        std::fs::remove_dir_all(root).ok();
    }
}
