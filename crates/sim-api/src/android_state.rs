//! The device-state methods on Android (#418), with iOS's parameters and results (each returns
//! nothing): `erase` (simee's user data, from the image's golden when it has one),
//! `status_bar` / `clear_status_bar` (SystemUI's demo mode), `increase_contrast`,
//! `hardware_keyboard` and `memory_warning`, through `sim_android::state`.
//!
//! `keyboard_language` stays iOS-only: an Android keyboard's language is its input method's
//! subtype, which only that input method's own settings choose; nothing the host can run
//! (`ime`, `settings`) sets it without a subtype id the IME computes, so the method says so in
//! its help rather than pretending.

use serde_json::Value;
use sim_android::state::{DemoStatusBar, Radio};
use sim_android::{Android, Device};
use sim_core::debug;

use crate::params::{need_bool, str_param};

/// The methods this module serves on Android devices.
const METHODS: [&str; 6] = ["erase", "status_bar", "clear_status_bar", "increase_contrast", "hardware_keyboard", "memory_warning"];

pub(crate) fn is_method(method: &str) -> bool {
    METHODS.contains(&method)
}

/// One of [`METHODS`] on `d`. Parameters are read before anything runs, so a missing or bad one
/// is reported whether or not the device is up.
pub(crate) fn call(android: &Android, d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    let done = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "erase" => android.erase(d).map(|_| Value::Null),
        "status_bar" => {
            let bar = demo_status_bar(p)?;
            done(d.adb(android)?.demo_status_bar(&bar))
        }
        "clear_status_bar" => done(d.adb(android)?.clear_demo_status_bar()),
        "increase_contrast" => {
            let on = need_bool(p, "on")?;
            let api = d.avd.as_ref().and_then(|a| a.api_level());
            done(d.adb(android)?.set_increase_contrast(on, api))
        }
        "hardware_keyboard" => {
            let on = need_bool(p, "on")?;
            done(d.adb(android)?.set_hardware_keyboard(on))
        }
        _ => {
            let package = str_param(p, "bundle_id").map(sim_android::apps::package_name).transpose()?;
            d.adb(android)?.memory_warning(package).map(|_| Value::Null)
        }
    }
}

/// iOS's Wi-Fi bars (0-3, the most it shows) as Android's (0-4).
fn wifi_level(bars: u64) -> u8 {
    [0, 1, 3, 4][bars.min(3) as usize]
}

/// `status_bar`'s iOS parameters as demo mode shows them. A name iOS doesn't know is refused as
/// iOS refuses it; `operator` (demo mode shows no carrier name) and a `time` with no time of day
/// are refused too. Bars without their mode are ignored, as on iOS.
fn demo_status_bar(p: &Value) -> Result<DemoStatusBar, String> {
    let pick = |key: &str, table: &[&str]| -> Result<Option<u64>, String> {
        match str_param(p, key) {
            Some(name) => debug::index_of(table, name).map(Some).ok_or_else(|| format!("unknown {key} `{name}`")),
            None => Ok(None),
        }
    };
    let bars = |key: &str| p.get(key).and_then(Value::as_u64).unwrap_or(3);
    if str_param(p, "operator").is_some_and(|o| !o.is_empty()) {
        return Err("`status_bar` takes `operator` for iOS devices only (Android's demo-mode status bar shows no carrier name)".into());
    }
    let clock = match str_param(p, "time") {
        Some(t) => Some(
            sim_android::state::demo_clock(t)
                .ok_or_else(|| format!("Android's demo-mode clock takes a time of day (HH:MM, or an ISO 8601 date and time), not `{t}`"))?,
        ),
        None => None,
    };
    // debug::RADIO_MODES: not-supported, searching, failed, active.
    let radio = |mode: u64, level: u8| match mode {
        0 => Radio::Hidden,
        3 => Radio::Up(level),
        _ => Radio::Down,
    };
    let wifi_mode = pick("wifi_mode", debug::RADIO_MODES)?;
    let mut wifi = wifi_mode.map(|m| radio(m, wifi_level(bars("wifi_bars"))));
    let mobile = pick("cellular_mode", debug::RADIO_MODES)?.map(|m| radio(m, bars("cellular_bars").min(4) as u8));
    // debug::DATA_NETWORKS, in order: what demo mode's `datatype` shows for each.
    let datatype = match pick("data_network", debug::DATA_NETWORKS)? {
        None => None,
        Some(1) => {
            // Wi-Fi as the data network: the Wi-Fi icon, at full bars unless wifi_mode says.
            wifi = wifi.or(Some(Radio::Up(4)));
            None
        }
        Some(n) => Some(["none", "", "3g", "4g", "lte", "lte+", "lte+", "5g", "5g+", "5g+", "5g+"][n as usize]),
    };
    // debug::BATTERY_STATES: charging, charged, discharging.
    let battery = pick("battery_state", debug::BATTERY_STATES)?
        .map(|state| (p.get("battery_level").and_then(Value::as_u64).unwrap_or(100).min(100) as u8, state != 2));
    Ok(DemoStatusBar { clock, wifi, mobile, datatype, battery })
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::{Value, json};
    use sim_android::state::{DemoStatusBar, Radio};
    use sim_android::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator};

    use super::demo_status_bar;
    use crate::methods::{Session, call};

    const NAMES: [&str; 6] = super::METHODS;

    /// A session over AVD `phone` on an API 36 image, running (with the fake adb) unless
    /// `running` is false. Keep the emulator: it answers the console for as long as it lives.
    fn session(tag: &str, running: bool) -> (Session, std::path::PathBuf, FakeAdb, Option<FakeEmulator>) {
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let adb = fake_adb(&android);
        let emulator = running.then(|| fake_emulator(&android, "phone"));
        (Session::with_android(android), root, adb, emulator)
    }

    fn run(s: &mut Session, method: &str, p: Value) -> Result<Value, String> {
        let mut p = p;
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(s, method, &p)
    }

    fn shells(adb: &FakeAdb) -> Vec<String> {
        adb.calls().into_iter().map(|c| c.strip_prefix("shell ").map_or(c.clone(), str::to_owned)).collect()
    }

    #[test]
    fn help_lists_both_platforms_for_the_state_methods() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        let platforms =
            |method: &str| help.as_array().and_then(|a| a.iter().find(|x| x["method"] == method)).expect(method)["platforms"].clone();
        for method in NAMES {
            assert_eq!(platforms(method), json!(["android", "ios"]), "{method}");
        }
        // Not settable from the host on Android: it stays iOS's, and its help says why.
        assert_eq!(platforms("keyboard_language"), json!(["ios"]));
        let (mut s, root, _adb, _emulator) = session("kbdlang", true);
        assert_eq!(
            run(&mut s, "keyboard_language", json!({"language": "fr_FR"})),
            Err("`keyboard_language` isn't available on Android devices (it works on iOS)".into())
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_ios_status_bar_maps_onto_demo_mode() {
        let bar = demo_status_bar(&json!({
            "time": "9:41", "data_network": "lte-a", "wifi_mode": "active", "wifi_bars": 2,
            "cellular_mode": "active", "cellular_bars": 4, "battery_state": "discharging", "battery_level": 42,
        }));
        assert_eq!(
            bar,
            Ok(DemoStatusBar {
                clock: Some("0941".into()),
                wifi: Some(Radio::Up(3)),
                mobile: Some(Radio::Up(4)),
                datatype: Some("lte+"),
                battery: Some((42, false)),
            })
        );
        // iOS's defaults: 3 bars (Wi-Fi's full 3 is Android's 4), battery 100.
        let defaults = demo_status_bar(&json!({"wifi_mode": "active", "cellular_mode": "searching", "battery_state": "charged"}));
        assert_eq!(
            defaults,
            Ok(DemoStatusBar { wifi: Some(Radio::Up(4)), mobile: Some(Radio::Down), battery: Some((100, true)), ..Default::default() })
        );
        let off = demo_status_bar(&json!({"wifi_mode": "not-supported", "cellular_mode": "failed", "battery_state": "charging"}));
        assert_eq!(
            off,
            Ok(DemoStatusBar { wifi: Some(Radio::Hidden), mobile: Some(Radio::Down), battery: Some((100, true)), ..Default::default() })
        );
        for (network, datatype) in [
            ("hide", "none"),
            ("3g", "3g"),
            ("4g", "4g"),
            ("lte", "lte"),
            ("lte+", "lte+"),
            ("5g", "5g"),
            ("5g+", "5g+"),
            ("5g-uwb", "5g+"),
            ("5g-uc", "5g+"),
        ] {
            assert_eq!(demo_status_bar(&json!({"data_network": network})).map(|b| b.datatype), Ok(Some(datatype)), "{network}");
        }
        // Wi-Fi as the data network shows Wi-Fi, unless wifi_mode says otherwise.
        let wifi = demo_status_bar(&json!({"data_network": "wifi"})).expect("wifi");
        assert_eq!((wifi.wifi, wifi.datatype), (Some(Radio::Up(4)), None));
        let hidden = demo_status_bar(&json!({"data_network": "wifi", "wifi_mode": "not-supported"})).expect("hidden");
        assert_eq!(hidden.wifi, Some(Radio::Hidden));
        // Bars past iOS's range are its maximum.
        assert_eq!(demo_status_bar(&json!({"wifi_mode": "active", "wifi_bars": 9})).map(|b| b.wifi), Ok(Some(Radio::Up(4))));
        // Bars without their mode are ignored, as on iOS.
        assert_eq!(demo_status_bar(&json!({"wifi_bars": 1, "cellular_bars": 1})), Ok(DemoStatusBar::default()));
    }

    #[test]
    fn what_demo_mode_cant_show_is_refused() {
        assert_eq!(demo_status_bar(&json!({"wifi_mode": "on"})), Err("unknown wifi_mode `on`".into()));
        assert_eq!(demo_status_bar(&json!({"data_network": "6g"})), Err("unknown data_network `6g`".into()));
        assert_eq!(demo_status_bar(&json!({"battery_state": "full"})), Err("unknown battery_state `full`".into()));
        let op = demo_status_bar(&json!({"cellular_mode": "active", "operator": "Carrier"})).expect_err("operator");
        assert_eq!(op, "`status_bar` takes `operator` for iOS devices only (Android's demo-mode status bar shows no carrier name)");
        assert_eq!(demo_status_bar(&json!({"operator": ""})), Ok(DemoStatusBar::default()), "an empty one asks for nothing");
        let time = demo_status_bar(&json!({"time": "Tuesday"})).expect_err("no time of day");
        assert_eq!(time, "Android's demo-mode clock takes a time of day (HH:MM, or an ISO 8601 date and time), not `Tuesday`");
    }

    #[test]
    fn the_state_methods_answer_as_on_ios() {
        let (mut s, root, adb, _emulator) = session("shapes", true);
        let bar = json!({"time": "09:41", "battery_state": "charged", "battery_level": 100});
        assert_eq!(run(&mut s, "status_bar", bar), Ok(Value::Null));
        assert_eq!(run(&mut s, "clear_status_bar", json!({})), Ok(Value::Null));
        assert_eq!(run(&mut s, "increase_contrast", json!({"on": true})), Ok(Value::Null));
        assert_eq!(run(&mut s, "hardware_keyboard", json!({"on": false})), Ok(Value::Null));
        assert_eq!(run(&mut s, "memory_warning", json!({})), Ok(Value::Null));
        assert_eq!(run(&mut s, "memory_warning", json!({"bundle_id": "com.example.app"})), Ok(Value::Null));
        let demo = "am broadcast -a com.android.systemui.demo -e command";
        assert_eq!(
            shells(&adb),
            [
                format!(
                    "settings put global sysui_demo_allowed 1 && {demo} enter && {demo} clock -e hhmm 0941 && {demo} battery -e level 100 -e plugged true"
                ),
                format!("{demo} exit"),
                // An API 36 guest has the color contrast too.
                "settings put secure high_text_contrast_enabled 1 && settings put secure contrast_level 1.0".into(),
                "settings put secure show_ime_with_hard_keyboard 1".into(),
                "dumpsys activity activities | grep -E 'topResumedActivity=|mResumedActivity:' || true".into(),
                "am send-trim-memory com.example.app RUNNING_CRITICAL".into(),
                "am send-trim-memory com.example.app RUNNING_CRITICAL".into(),
            ]
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn parameters_are_read_before_adb_runs() {
        let (mut s, root, adb, _emulator) = session("params", true);
        for method in ["increase_contrast", "hardware_keyboard"] {
            assert_eq!(run(&mut s, method, json!({})), Err("missing boolean parameter `on`".into()), "{method}");
        }
        assert!(run(&mut s, "status_bar", json!({"time": "late"})).is_err());
        let bad = run(&mut s, "memory_warning", json!({"bundle_id": "com.example;reboot"})).expect_err("not a package");
        assert!(bad.contains("isn't a package name"), "{bad}");
        assert_eq!(adb.calls(), Vec::<String>::new(), "none reached adb");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn erase_refuses_a_running_device_and_erases_a_shut_down_one() {
        let (mut s, root, _adb, emulator) = session("erase", true);
        let disks = emu_avd::state_dir(&root.join("avd/phone.avd"));
        std::fs::create_dir_all(&disks).expect("simee");
        std::fs::write(disks.join("userdata.img"), b"used").expect("userdata");
        let err = run(&mut s, "erase", json!({})).expect_err("running");
        assert!(err.ends_with("shut it down before erasing it"), "{err}");
        assert!(disks.join("userdata.img").exists(), "kept while it runs");
        drop(emulator);
        std::fs::remove_dir_all(root.join("running")).expect("no longer advertised");
        assert_eq!(run(&mut s, "erase", json!({})), Ok(Value::Null));
        assert!(!disks.join("userdata.img").exists(), "its user data is gone");
        assert!(root.join("avd/phone.avd/config.ini").is_file(), "the AVD stays");
        std::fs::remove_dir_all(root).ok();
    }

    /// docs/api.md has each of these methods' rows as `help` writes them.
    #[test]
    fn the_docs_agree_with_the_method_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(api) = std::fs::read_to_string(root.join("docs/api.md")) else {
            eprintln!("skipped: not in the simee workspace, so docs/api.md isn't here");
            return;
        };
        let rows = NAMES.iter().chain(&["keyboard_language"]);
        for (method, params, doc) in crate::METHODS.iter().filter(|(m, ..)| rows.clone().any(|r| r == m)) {
            let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
            let row = format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.lines().any(|l| l == row), "docs/api.md's row for `{method}` differs from METHODS; it should be\n{row}");
        }
    }
}
