//! Android Auto projection on an Android phone (#437), through `sim_android::dhu`:
//!
//! - `car_start {device, input?, resolution?, dpi?, headless?}`: runs Google's Desktop Head Unit
//!   against the phone (an adb forward to the head unit server in its Android Auto app) and
//!   supervises it; `car_stop` ends it and removes the forward.
//! - `display: "car"` (or `"carplay"`, the name CarPlay's display has on iOS) on `screen`,
//!   `screenshot`, `tap` and `swipe`: the head unit's screen, in its pixels. The DHU's console has
//!   no hold or duration, so `hold_ms`, `duration_ms` and `settle` are ignored there.
//! - `vehicle` on a device with a head unit: `speed`, `gear`, `night` and `parking-brake` as the
//!   DHU's console commands (the same names Automotive's vehicle HAL takes).

use serde_json::{Value, json};
use sim_android::dhu::{self, Command, Config, InputMode, Options, Resolution};
use sim_android::{Android, Device};

use crate::params::{opt_bool, png_result, str_param};

/// The methods this module answers.
pub(crate) const METHODS: &[&str] = &["car_start", "car_stop"];

/// `car_start`'s options, read before any device is touched.
pub(crate) fn start_options(p: &Value) -> Result<Options, String> {
    let mut config = Config::default();
    if let Some(v) = str_param(p, "input") {
        config.input = InputMode::parse(v)?;
    } else if !matches!(p.get("input"), None | Some(Value::Null)) {
        return Err(format!("`input` must be one of {}", InputMode::NAMES));
    }
    if let Some(v) = str_param(p, "resolution") {
        config.resolution = Resolution::parse(v)?;
    } else if !matches!(p.get("resolution"), None | Some(Value::Null)) {
        return Err(format!("`resolution` must be one of {}", Resolution::NAMES));
    }
    match p.get("dpi") {
        None | Some(Value::Null) => {}
        Some(v) => config.dpi = v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or("`dpi` must be a whole number")?,
    }
    config.validate()?;
    Ok(Options { config, headless: opt_bool(p, "headless")?.unwrap_or(false), ..Options::default() })
}

fn projection_json(p: &dhu::Projection) -> Value {
    json!({
        "pid": p.pid,
        "serial": p.serial,
        "forward_port": p.forward_port,
        "resolution": p.config.resolution.as_str(),
        "dpi": p.config.dpi,
        "input": p.config.input.as_str(),
        "headless": p.headless,
    })
}

/// Answers one of [`METHODS`] for the running device `d`.
pub(crate) fn call(android: &Android, d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    if method == "car_start" {
        let options = start_options(p)?;
        return d.car_start(android, &options).map(|started| projection_json(&started));
    }
    d.car_stop(android).map(|s| json!({"pid": s.pid, "was_running": s.was_running, "forward_removed": s.forward_removed}))
}

/// A pixel coordinate `key` for the car display.
fn pixel(p: &Value, key: &str) -> Result<u32, String> {
    let v = crate::params::need_f64(p, key)?;
    if !v.is_finite() || v < 0.0 || v > f64::from(u32::MAX) {
        return Err(format!("`{key}` must be a pixel on the car display, not {v}"));
    }
    Ok(v.round() as u32)
}

/// `screen`, `screenshot`, `tap` and `swipe` with `display=car`.
pub(crate) fn display(d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    let projection = || d.head_unit()?.ok_or_else(dhu::no_head_unit);
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "screen" => {
            let (width, height) = projection()?.config.resolution.size();
            Ok(json!({"width": width, "height": height}))
        }
        "screenshot" => {
            projection()?;
            match str_param(p, "path") {
                Some(path) => d.car_screenshot_to(std::path::Path::new(path)).map(|(w, h)| json!({"path": path, "width": w, "height": h})),
                None => d.car_screenshot().and_then(|(w, h, png)| png_result(w, h, png, p)),
            }
        }
        "tap" => null(d.car_command(&Command::Tap { x: pixel(p, "x")?, y: pixel(p, "y")? })),
        "swipe" => {
            null(d.car_command(&Command::Swipe { from: (pixel(p, "x1")?, pixel(p, "y1")?), to: (pixel(p, "x2")?, pixel(p, "y2")?) }))
        }
        other => Err(format!(
            "display=car is the Android Auto head unit's screen: it works with screen, screenshot, tap and swipe, not `{other}`"
        )),
    }
}

/// `vehicle` on a device whose head unit runs: `Some(result)`, else `None` (the vehicle HAL's).
pub(crate) fn vehicle(d: &Device, p: &Value) -> Option<Result<Value, String>> {
    d.head_unit().ok().flatten()?;
    Some(vehicle_on_head_unit(d, p))
}

fn vehicle_on_head_unit(d: &Device, p: &Value) -> Result<Value, String> {
    let property = str_param(p, "property").ok_or_else(|| {
        format!("this device has a head unit: give the `property` to set ({}) and its `value`", dhu::VEHICLE_PROPERTIES.join(", "))
    })?;
    let value = crate::params::vehicle_value(p)?.ok_or("the DHU's console sets a property but can't read one: give the `value` to set")?;
    d.car_command(&dhu::vehicle(property, &value)?).map(|()| Value::Null)
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn start_options_default_and_validate() {
        let o = start_options(&json!({"device": "phone"})).expect("defaults");
        assert_eq!((o.config.resolution, o.config.dpi, o.config.input, o.headless), (Resolution::R1280x720, 160, InputMode::Touch, false));
        let o = start_options(&json!({"input": "rotary", "resolution": "1920x1080", "dpi": 240, "headless": true})).expect("given");
        assert_eq!((o.config.resolution, o.config.dpi, o.config.input, o.headless), (Resolution::R1920x1080, 240, InputMode::Rotary, true));
        assert!(start_options(&json!({"input": "voice"})).expect_err("input").contains("touch, rotary or hybrid"));
        assert!(start_options(&json!({"input": 3})).expect_err("input").contains("`input` must be one of"));
        assert!(start_options(&json!({"resolution": "640x480"})).expect_err("res").contains("800x480"));
        assert!(start_options(&json!({"dpi": 9})).expect_err("dpi").contains("out of range"));
        assert!(start_options(&json!({"dpi": "big"})).is_err());
        assert!(start_options(&json!({"headless": "yes"})).is_err());
    }
}

/// The whole path with a fake adb and a fake DHU (a shell script): `car_start`, the car display
/// on `screen`, `screenshot`, `tap` and `swipe`, `vehicle`, `car_stop`.
#[cfg(all(test, unix))]
mod path {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use sim_android::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator, write_script};

    use crate::methods::{Session, call};

    const FAKE_DHU: &str = r#"d="$(dirname "$0")"
echo "$*" > "$d/dhu.argv"
while read -r line; do
  echo "$line" >> "$d/dhu.lines"
  case "$line" in
    "screenshot "*) cp "$d/shot.png" "${line#screenshot }";;
  esac
done
"#;

    struct Fixture {
        s: Session,
        root: PathBuf,
        adb: FakeAdb,
        _emulator: FakeEmulator,
    }

    /// A running AVD `phone` with the fake adb; the DHU is installed only with `dhu`.
    fn fixture(tag: &str, dhu: bool) -> Fixture {
        let root = PathBuf::from("/tmp").join(format!("sa-car-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/x86_64");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;x86_64", None).expect("create");
        let adb = fake_adb(&android);
        let emulator = fake_emulator(&android, "phone");
        if dhu {
            let auto = root.join("sdk/extras/google/auto");
            std::fs::create_dir_all(&auto).expect("auto dir");
            let png = sim_core::frame::encode_png(&sim_core::frame::Frame { width: 1280, height: 720, rgba: vec![5; 1280 * 720 * 4] })
                .expect("png");
            std::fs::write(auto.join("shot.png"), png).expect("png");
            write_script(&auto.join("desktop-head-unit"), FAKE_DHU);
        }
        Fixture { s: Session::with_android(android), root, adb, _emulator: emulator }
    }

    fn run(f: &mut Fixture, method: &str, mut p: Value) -> Result<Value, String> {
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(&mut f.s, method, &p)
    }

    fn lines(path: &Path, n: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let all: Vec<String> = text.lines().map(str::to_owned).collect();
            if all.len() >= n || Instant::now() > deadline {
                return all;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_methods_are_android_only_and_listed() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        for method in ["car_start", "car_stop"] {
            let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == method)).expect(method);
            assert_eq!(row["platforms"], json!(["android"]), "{method}");
        }
    }

    #[test]
    fn car_start_says_what_is_missing() {
        let mut f = fixture("missing", false);
        let err = run(&mut f, "car_start", json!({})).expect_err("no DHU");
        assert!(err.contains("Desktop Head Unit isn't installed") && err.contains("emu -provision-dhu"), "{err}");
        std::fs::remove_dir_all(&f.root).ok();

        let mut f = fixture("missing2", true);
        let err = run(&mut f, "car_start", json!({})).expect_err("no Android Auto");
        assert!(err.contains("Android Auto isn't installed on phone") && err.contains("com.google.android.projection.gearhead"), "{err}");
        f.adb.android_auto(Some("5.2.1"));
        let err = run(&mut f, "car_start", json!({})).expect_err("too old");
        assert!(err.contains("Android Auto 5.2.1 on phone is too old") && err.contains("6.0 or newer"), "{err}");
        f.adb.android_auto(Some("14.2.640004-rc"));
        let err = run(&mut f, "car_start", json!({})).expect_err("no server");
        assert!(
            err.contains("no head unit server is running on phone") && err.contains("5277") && err.contains("Start head unit server"),
            "{err}"
        );
        assert!(
            !f.adb.calls().iter().any(|c| c.starts_with("forward")),
            "nothing is forwarded before the checks pass: {:?}",
            f.adb.calls()
        );
        assert_eq!(run(&mut f, "car_stop", json!({})).expect_err("none"), "no head unit is running for this device (nothing to stop)");
        let err = run(&mut f, "screenshot", json!({"display": "car"})).expect_err("none");
        assert_eq!(err, "no head unit is running for this device: start one with `car_start`");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_whole_session_with_a_fake_dhu() {
        let mut f = fixture("whole", true);
        f.adb.android_auto(Some("14.2.640004-rc"));
        f.adb.head_unit_server(true);
        let started =
            run(&mut f, "car_start", json!({"input": "hybrid", "resolution": "800x480", "dpi": 200, "headless": true})).expect("start");
        let port = started["forward_port"].as_u64().expect("port");
        assert_eq!(
            (started["resolution"].as_str(), started["dpi"].as_u64(), started["input"].as_str(), started["headless"].as_bool()),
            (Some("800x480"), Some(200), Some("hybrid"), Some(true))
        );
        assert!(started["pid"].as_u64().is_some() && started["serial"].as_str().is_some_and(|s| s.starts_with("emulator-")), "{started}");
        assert!(f.adb.calls().iter().any(|c| *c == format!("forward --no-rebind tcp:{port} tcp:5277")), "{:?}", f.adb.calls());
        let auto = f.root.join("sdk/extras/google/auto");
        assert!(lines(&auto.join("dhu.argv"), 1)[0].starts_with(&format!("--adb={port} -i hybrid -c ")));
        assert!(run(&mut f, "car_start", json!({})).expect_err("twice").contains("already running"));

        // The car display is its own screen: its size, its PNG, its taps.
        assert_eq!(run(&mut f, "screen", json!({"display": "car"})), Ok(json!({"width": 800, "height": 480})));
        let shot = run(&mut f, "screenshot", json!({"display": "car"})).expect("screenshot");
        assert_eq!((shot["width"].as_u64(), shot["height"].as_u64()), (Some(1280), Some(720)));
        assert!(shot["png"].is_string(), "{shot}");
        let path = f.root.join("car.png");
        let shot = run(&mut f, "screenshot", json!({"display": "carplay", "path": path})).expect("to path");
        assert_eq!(shot["path"], json!(path));
        assert!(path.is_file());
        assert_eq!(run(&mut f, "tap", json!({"x": 400, "y": 240.4, "display": "car"})), Ok(Value::Null));
        assert_eq!(run(&mut f, "swipe", json!({"x1": 10, "y1": 20, "x2": 700, "y2": 20, "display": "car"})), Ok(Value::Null));
        let err = run(&mut f, "tap", json!({"x": 900, "y": 1, "display": "car"})).expect_err("off screen");
        assert!(err.contains("off the 800x480 car display"), "{err}");
        let err = run(&mut f, "tap", json!({"x": -1, "y": 1, "display": "car"})).expect_err("negative");
        assert!(err.contains("must be a pixel"), "{err}");
        let err = run(&mut f, "button", json!({"button": "home", "display": "car"})).expect_err("not a car method");
        assert!(err.contains("works with screen, screenshot, tap and swipe, not `button`"), "{err}");

        // `vehicle` goes to the DHU while it runs.
        for (property, value) in [("speed", json!(12.5)), ("gear", json!("drive")), ("night", json!(true)), ("parking-brake", json!(false))]
        {
            assert_eq!(run(&mut f, "vehicle", json!({"property": property, "value": value})), Ok(Value::Null), "{property}");
        }
        let err = run(&mut f, "vehicle", json!({"property": "fuel", "value": 1})).expect_err("unmapped");
        assert!(err.contains("speed, gear, night, parking-brake"), "{err}");
        assert!(run(&mut f, "vehicle", json!({})).expect_err("no property").contains("give the `property` to set"));
        assert!(run(&mut f, "vehicle", json!({"property": "speed"})).expect_err("no value").contains("can't read one"));

        let sent = lines(&auto.join("dhu.lines"), 8);
        assert!(sent[0].starts_with("screenshot ") && sent[1].starts_with("screenshot "), "{sent:?}");
        assert_eq!(sent[2..], ["tap 400 240", "swipe 10 20 700 20", "speed 12.5", "gear drive", "night", "parking_brake off"], "{sent:?}");

        let stopped = run(&mut f, "car_stop", json!({})).expect("stop");
        assert_eq!((stopped["was_running"].as_bool(), stopped["forward_removed"].as_bool()), (Some(true), Some(true)));
        assert!(f.adb.calls().iter().any(|c| *c == format!("forward --remove tcp:{port}")), "{:?}", f.adb.calls());
        assert!(run(&mut f, "tap", json!({"x": 1, "y": 1, "display": "car"})).expect_err("stopped").contains("no head unit is running"));
        // Without a head unit, `vehicle` is the HAL's again (and a phone has none).
        assert!(run(&mut f, "vehicle", json!({"property": "speed", "value": 1})).expect_err("HAL").contains("vehicle HAL"));
        std::fs::remove_dir_all(&f.root).ok();
    }
}
