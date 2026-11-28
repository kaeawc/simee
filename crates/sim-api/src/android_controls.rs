//! The emulator console's hardware controls as typed methods (#160), through
//! `sim_android::controls`: `battery` (`power`), `cellular` (`gsm signal|voice|data|status`),
//! `phone_call` (`gsm call|accept|busy|hold|cancel|list`), `sms` (`sms send`), `sensor`
//! (`sensor set|get|status`) and `shake` (the accelerometer swung and put back). Each reads its
//! parameters before the console is used and answers with what the console reads back, so a
//! caller sees the state it left, not the one it asked for.
//!
//! `biometric` (the fingerprint sensor, `android_notify.rs`) and `network_shape` (the console's
//! `network speed|delay|loss`, airplane mode, `android_net.rs`) are the issue's other two.

use serde_json::{Value, json};
use sim_android::Device;
use sim_android::controls::{
    Battery, BatteryChange, BatteryHealth, BatteryStatus, Call, CallAction, Cellular, CellularChange, Registration, Shake,
};

use crate::params::{need_str, opt_bool, str_param};

/// The methods this module answers.
pub(crate) const METHODS: &[&str] = &["battery", "cellular", "phone_call", "sms", "sensor", "shake"];

/// Answers one of [`METHODS`] for the device `d`.
pub(crate) fn call(d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "battery" => d.battery(&battery_change(p)?).map(|b| battery_json(&b)),
        "cellular" => d.cellular(&cellular_change(p)?).map(|c| cellular_json(&c)),
        "phone_call" => {
            let action = match str_param(p, "action").unwrap_or("list") {
                "list" if p.get("number").is_some_and(|n| !n.is_null()) => {
                    return Err("`phone_call` `list` takes no `number`".into());
                }
                "list" => None,
                a => Some((CallAction::parse(a)?, need_str(p, "number")?)),
            };
            d.phone_call(action).map(|calls| Value::Array(calls.iter().map(call_json).collect()))
        }
        "sms" => d.sms(need_str(p, "from")?, need_str(p, "text")?).map(|()| Value::Null),
        "sensor" => match (str_param(p, "name"), values(p)?) {
            (None, None) => d.sensors().map(|names| json!({"sensors": names})),
            (None, Some(_)) => Err("give the sensor's `name` to set its `values`".into()),
            (Some(name), v) => d.sensor(name, v.as_deref()).map(|values| json!({"name": name, "values": values})),
        },
        _ => d.shake(&shake_options(p)?, &crate::call_cancelled).map(|()| Value::Null),
    }
}

/// `shake`'s duration and intensity (by default [`Shake::DEFAULT`]'s), checked before the console
/// is reached.
pub(crate) fn shake_options(p: &Value) -> Result<Shake, String> {
    let mut shake = Shake::DEFAULT;
    if let Some(ms) = p.get("duration_ms").filter(|v| !v.is_null()) {
        shake.duration = std::time::Duration::from_millis(ms.as_u64().ok_or("duration_ms must be a whole number")?);
    }
    if let Some(i) = p.get("intensity").filter(|v| !v.is_null()) {
        shake.intensity = i.as_f64().ok_or("intensity must be a number (m/s²)")?;
    }
    shake.plan([0.0; 3])?;
    Ok(shake)
}

/// `battery`'s parameters: `charging` is the cable (AC and status together); `ac` and `status`
/// set one each, so they don't go with it.
fn battery_change(p: &Value) -> Result<BatteryChange, String> {
    let mut change = match opt_bool(p, "charging")? {
        Some(on) => {
            if let Some(k) = ["ac", "status"].into_iter().find(|k| p.get(*k).is_some_and(|v| !v.is_null())) {
                return Err(format!("`charging` sets both AC and status; give `charging` or `{k}`, not both"));
            }
            BatteryChange::charging(on)
        }
        None => BatteryChange {
            ac: opt_bool(p, "ac")?,
            status: str_param(p, "status").map(BatteryStatus::parse).transpose()?,
            ..Default::default()
        },
    };
    change.health = str_param(p, "health").map(BatteryHealth::parse).transpose()?;
    change.present = opt_bool(p, "present")?;
    change.level = match p.get("level") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().filter(|n| *n <= 100).ok_or("level must be a whole percent, 0 to 100")? as u32),
    };
    for k in ["status", "health"] {
        if let Some(v) = p.get(k).filter(|v| !v.is_null() && !v.is_string()) {
            return Err(format!("`{k}` must be a string, not {v}"));
        }
    }
    Ok(change)
}

fn battery_json(b: &Battery) -> Value {
    json!({"level": b.level, "ac": b.ac, "status": b.status.name(), "health": b.health.name(), "present": b.present})
}

fn cellular_change(p: &Value) -> Result<CellularChange, String> {
    let state = |k: &str| match p.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Registration::parse(s).map(Some),
        Some(other) => Err(format!("`{k}` must be a registration state (a string), not {other}")),
    };
    let rssi = match p.get("rssi") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().filter(|n| *n <= 31).ok_or("rssi must be a whole number, 0 (no signal) to 31 (full)")? as u8),
    };
    Ok(CellularChange { rssi, voice: state("voice")?, data: state("data")? })
}

fn cellular_json(c: &Cellular) -> Value {
    json!({"voice": c.voice.map(Registration::name), "data": c.data.map(Registration::name)})
}

fn call_json(c: &Call) -> Value {
    json!({"direction": c.direction, "number": c.number, "state": c.state})
}

/// `sensor`'s `values`: an array of numbers, or `None` to only read.
fn values(p: &Value) -> Result<Option<Vec<f64>>, String> {
    match p.get("values") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_f64().ok_or_else(|| format!("sensor value {v} isn't a number"))).collect::<Result<_, _>>().map(Some)
        }
        Some(Value::Number(n)) => Ok(n.as_f64().map(|v| vec![v])),
        Some(other) => Err(format!("`values` must be an array of numbers, not {other}")),
    }
}

/// Run against `sim_android::testing`'s fake emulator, which is unix only.
#[cfg(all(test, unix))]
mod tests {
    use serde_json::json;
    use sim_android::Android;
    use sim_android::testing::{FakeEmulator, fake_emulator};

    use super::*;
    use crate::{Session, call_method as call};

    struct Fixture {
        s: Session,
        root: std::path::PathBuf,
        emulator: FakeEmulator,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A session over a running AVD `phone` whose emulator is the fake.
    fn fixture(tag: &str) -> Fixture {
        let dir = std::path::PathBuf::from("/tmp").join(format!("sa-acontrols-{tag}-{}", std::process::id()));
        let image = dir.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(dir.join("sdk")), avd_home: dir.join("avd") });
        android.running_dir = Some(dir.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let emulator = fake_emulator(&android, "phone");
        Fixture { s: Session::with_android(android), root: dir, emulator }
    }

    fn run(f: &mut Fixture, method: &str, p: Value) -> Result<Value, String> {
        let mut p = p;
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(&mut f.s, method, &p)
    }

    fn console(f: &Fixture) -> Vec<String> {
        std::mem::take(&mut *f.emulator.console.lock().expect("console"))
    }

    #[test]
    fn help_lists_the_controls_for_android_only() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        for method in METHODS {
            let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == *method)).expect(method);
            assert_eq!(row["platforms"], json!(["android"]), "{method}");
        }
    }

    #[test]
    fn battery_sets_level_and_charging_and_reads_it_back() {
        let mut f = fixture("battery");
        let read = run(&mut f, "battery", json!({})).expect("read");
        assert_eq!(read, json!({"level": 50, "ac": true, "status": "charging", "health": "good", "present": true}));
        assert_eq!(console(&f), ["power display"]);
        let out = run(&mut f, "battery", json!({"level": 15, "charging": false})).expect("set");
        assert_eq!(out, json!({"level": 15, "ac": false, "status": "discharging", "health": "good", "present": true}));
        assert_eq!(console(&f), ["power ac off", "power status discharging", "power capacity 15", "power display"]);
        let out = run(&mut f, "battery", json!({"status": "full", "health": "cold", "present": false})).expect("each");
        assert_eq!(out["status"], "full");
        assert_eq!(console(&f), ["power status full", "power health cold", "power present false", "power display"]);
    }

    #[test]
    fn battery_refuses_bad_parameters_before_the_console() {
        let mut f = fixture("batteryparams");
        for (p, says) in [
            (json!({"level": 101}), "level must be a whole percent, 0 to 100"),
            (json!({"level": -1}), "level must be a whole percent"),
            (json!({"level": "50"}), "level must be a whole percent"),
            (json!({"charging": true, "ac": false}), "give `charging` or `ac`, not both"),
            (json!({"charging": true, "status": "full"}), "give `charging` or `status`, not both"),
            (json!({"charging": "yes"}), "`charging` must be true or false"),
            (json!({"status": "empty"}), "unknown battery status `empty`"),
            (json!({"status": 3}), "`status` must be a string"),
            (json!({"health": "fine"}), "unknown battery health `fine`"),
        ] {
            let err = run(&mut f, "battery", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(console(&f).is_empty());
    }

    #[test]
    fn cellular_sets_signal_and_registration() {
        let mut f = fixture("cellular");
        assert_eq!(run(&mut f, "cellular", json!({})).expect("read"), json!({"voice": "home", "data": "home"}));
        let out = run(&mut f, "cellular", json!({"rssi": 4, "voice": "roaming", "data": "unregistered"})).expect("set");
        assert_eq!(out, json!({"voice": "roaming", "data": "unregistered"}));
        assert_eq!(console(&f), ["gsm status", "gsm voice roaming", "gsm data unregistered", "gsm signal 4", "gsm status"]);
        for (p, says) in [
            (json!({"rssi": 32}), "rssi must be a whole number, 0 (no signal) to 31"),
            (json!({"rssi": 2.5}), "rssi must be a whole number"),
            (json!({"data": "gone"}), "unknown registration `gone`"),
            (json!({"voice": true}), "`voice` must be a registration state"),
        ] {
            let err = run(&mut f, "cellular", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(console(&f).is_empty());
    }

    #[test]
    fn phone_call_rings_answers_and_hangs_up() {
        let mut f = fixture("call");
        assert_eq!(run(&mut f, "phone_call", json!({})).expect("list"), json!([]));
        let ringing = run(&mut f, "phone_call", json!({"action": "incoming", "number": "+15551234567"})).expect("ring");
        assert_eq!(ringing, json!([{"direction": "inbound", "number": "+15551234567", "state": "incoming"}]));
        let held = run(&mut f, "phone_call", json!({"action": "hold", "number": "+15551234567"})).expect("hold");
        assert_eq!(held[0]["state"], "held");
        assert_eq!(run(&mut f, "phone_call", json!({"action": "cancel", "number": "+15551234567"})).expect("cancel"), json!([]));
        assert_eq!(
            console(&f),
            ["gsm list", "gsm call +15551234567", "gsm list", "gsm hold +15551234567", "gsm list", "gsm cancel +15551234567", "gsm list"]
        );
        let err = run(&mut f, "phone_call", json!({"action": "accept", "number": "123"})).expect_err("no call");
        assert!(err.contains("no current call to/from number '123'"), "{err}");
        console(&f);
        for (p, says) in [
            (json!({"action": "incoming"}), "missing string parameter `number`"),
            (json!({"action": "incoming", "number": "555-1234"}), "bad phone number `555-1234`"),
            (json!({"action": "dial", "number": "1"}), "unknown call action `dial`"),
            (json!({"action": "list", "number": "1"}), "`list` takes no `number`"),
        ] {
            let err = run(&mut f, "phone_call", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(console(&f).is_empty());
    }

    #[test]
    fn sms_delivers_one_line() {
        let mut f = fixture("sms");
        assert_eq!(run(&mut f, "sms", json!({"from": "+15551234567", "text": "Your code is 1234"})), Ok(Value::Null));
        assert_eq!(console(&f), ["sms send +15551234567 Your code is 1234"]);
        for (p, says) in [
            (json!({"text": "hi"}), "missing string parameter `from`"),
            (json!({"from": "1"}), "missing string parameter `text`"),
            (json!({"from": "a b", "text": "hi"}), "bad SMS sender `a b`"),
            (json!({"from": "1", "text": "a\nb"}), "line break"),
        ] {
            let err = run(&mut f, "sms", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(console(&f).is_empty());
    }

    #[test]
    fn sensor_sets_reads_and_lists() {
        let mut f = fixture("sensor");
        assert_eq!(run(&mut f, "sensor", json!({})).expect("list"), json!({"sensors": ["acceleration", "light"]}));
        assert_eq!(run(&mut f, "sensor", json!({"name": "light"})).expect("get"), json!({"name": "light", "values": [400.0]}));
        let set = run(&mut f, "sensor", json!({"name": "acceleration", "values": [1, 2.5, 3]})).expect("set");
        assert_eq!(set, json!({"name": "acceleration", "values": [1.0, 2.5, 3.0]}));
        assert_eq!(console(&f), ["sensor status", "sensor get light", "sensor set acceleration 1:2.5:3", "sensor get acceleration"]);
        for (p, says) in [
            (json!({"values": [1]}), "give the sensor's `name`"),
            (json!({"name": "smell"}), "unknown sensor `smell`"),
            (json!({"name": "light", "values": [1, 2]}), "takes 1 value, not 2"),
            (json!({"name": "light", "values": ["a"]}), "isn't a number"),
            (json!({"name": "light", "values": "1"}), "`values` must be an array of numbers"),
        ] {
            let err = run(&mut f, "sensor", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(console(&f).is_empty());
    }

    #[test]
    fn shake_swings_the_accelerometer_and_puts_it_back() {
        let mut f = fixture("shake");
        run(&mut f, "sensor", json!({"name": "acceleration", "values": [0, 9.5, 1]})).expect("rest");
        console(&f);
        assert_eq!(run(&mut f, "shake", json!({"duration_ms": 200, "intensity": 20})), Ok(Value::Null));
        assert_eq!(
            console(&f),
            [
                "sensor get acceleration",
                "sensor set acceleration 20:9.5:1",
                "sensor set acceleration -20:9.5:1",
                "sensor set acceleration 20:9.5:1",
                "sensor set acceleration -20:9.5:1",
                "sensor set acceleration 0:9.5:1",
            ]
        );
        for (p, says) in [
            (json!({"intensity": 0}), "shake intensity 0 must be above 0"),
            (json!({"intensity": "hard"}), "intensity must be a number"),
            (json!({"duration_ms": 60001}), "at most 60 s"),
            (json!({"duration_ms": -1}), "duration_ms must be a whole number"),
        ] {
            let err = run(&mut f, "shake", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(console(&f).is_empty());
    }

    #[test]
    fn a_cancelled_shake_puts_the_accelerometer_back_and_returns_promptly() {
        let mut f = fixture("shakecancel");
        run(&mut f, "sensor", json!({"name": "acceleration", "values": [0, 9.5, 1]})).expect("rest");
        console(&f);
        let asked = std::cell::Cell::new(0);
        let hooks = crate::CallHooks {
            cancelled: Box::new(move || {
                asked.set(asked.get() + 1);
                asked.get() > 3
            }),
            progress: Box::new(|_| {}),
        };
        let started = std::time::Instant::now();
        let err =
            crate::with_call_hooks(hooks, || run(&mut f, "shake", json!({"duration_ms": 60000, "intensity": 20}))).expect_err("cancelled");
        assert!(err.starts_with("cancelled"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(
            console(&f),
            [
                "sensor get acceleration",
                "sensor set acceleration 20:9.5:1",
                "sensor set acceleration -20:9.5:1",
                "sensor set acceleration 20:9.5:1",
                "sensor set acceleration 0:9.5:1",
            ]
        );
    }

    #[test]
    fn sms_refuses_a_text_that_would_cross_the_consoles_line() {
        let mut f = fixture("smscap");
        let text = format!("{}kill", "a".repeat(4084));
        let err = run(&mut f, "sms", json!({"from": "1", "text": text})).expect_err("long");
        assert!(err.contains("over the 1024"), "{err}");
        assert!(run(&mut f, "sms", json!({"from": "1", "text": "a\u{0}kill"})).is_err());
        assert!(console(&f).is_empty());
    }

    #[test]
    fn a_device_that_isnt_running_says_so() {
        let mut f = fixture("stopped");
        drop(std::fs::remove_dir_all(f.root.join("running")));
        for method in METHODS {
            let p = match *method {
                "sms" => json!({"from": "1", "text": "hi"}),
                _ => json!({}),
            };
            let err = run(&mut f, method, p).expect_err(method);
            assert!(err.contains("isn't running") || err.contains("boot it first"), "{method}: {err}");
        }
    }

    #[test]
    fn the_api_md_rows_agree_with_the_method_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(api) = std::fs::read_to_string(root.join("docs/api.md")) else {
            eprintln!("skipped: not in the simee workspace, so docs/api.md isn't here");
            return;
        };
        let differ: Vec<String> = crate::METHODS
            .iter()
            .filter(|(m, ..)| METHODS.contains(m))
            .map(|(method, params, doc)| {
                let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
                format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"))
            })
            .filter(|row| !api.lines().any(|l| l == row))
            .collect();
        assert!(differ.is_empty(), "docs/api.md's rows differ from METHODS; they should be\n{}", differ.join("\n"));
    }
}
