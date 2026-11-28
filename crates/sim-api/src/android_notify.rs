//! iOS's `push`, `notify` and `biometric` on Android devices (#419,
//! docs/research/android-notify-biometric.md), through `sim_android::notify`:
//!
//! - `push`: an FCM message (`{data, notification}`, FCM's own shape) delivered to the app's
//!   Firebase receiver as Play services delivers one, as root. An APNs payload is refused.
//! - `notify`: an `am broadcast` of `name` as the action, with typed `extras`, to every receiver
//!   or `bundle_id`'s. A broadcast has no state, so iOS's `state` is refused.
//! - `biometric`: the fingerprint sensor through the console's `finger touch`: `match` touches an
//!   enrolled finger (1, or `finger_id`), `nomatch` one that isn't. Enrollment is Settings' flow
//!   and there is no face sensor, so `enroll`, `unenroll` and `face` are refused with why.
//!
//! `reset_keychain` has no Android counterpart; the dispatcher refuses it with
//! [`UNSUPPORTED_WHY`].

use serde_json::{Map, Value};
use sim_android::notify::{Extra, FcmMessage, NOMATCH_FINGER};
use sim_android::{Android, Device};

use crate::params::{need_str, opt_bool, str_param};

/// The methods this module answers.
pub(crate) const METHODS: &[&str] = &["push", "notify", "biometric"];

/// iOS methods Android has no faithful counterpart for, and why: the dispatcher adds the reason to
/// its "isn't available on Android devices" error.
pub(crate) const UNSUPPORTED_WHY: &[(&str, &str)] = &[(
    "reset_keychain",
    "Android has no device-wide keychain: each app's KeyStore keys belong to its uid and go when the app is uninstalled, and the \
     credentials `locksettings clear` resets are the screen lock's",
)];

/// Answers one of [`METHODS`] for the running device `d`. Parameters are read before adb or the
/// console is used.
pub(crate) fn call(android: &Android, d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    match method {
        "push" => {
            let package = need_str(p, "bundle_id")?;
            let message = fcm_message(p.get("payload").ok_or("missing parameter `payload`")?)?;
            null(d.adb(android)?.push(package, &message))
        }
        "notify" => {
            let action = need_str(p, "name")?;
            if !matches!(p.get("state"), None | Some(Value::Null)) {
                return Err("`notify` takes `state` for iOS devices only (a Darwin notification's 64-bit state; an Android \
                            broadcast carries `extras` instead)"
                    .into());
            }
            let package = str_param(p, "bundle_id");
            let extras = match p.get("extras") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Object(m)) => extras(m)?,
                Some(other) => return Err(format!("`extras` must be an object, not {other}")),
            };
            null(d.adb(android)?.broadcast(action, package, &extras))
        }
        _ => {
            let action = need_str(p, "action")?;
            if opt_bool(p, "face")?.unwrap_or(false) {
                return Err("an Android emulator has no face sensor: `biometric` drives its fingerprint sensor (leave out `face`)".into());
            }
            let finger = match p.get("finger_id") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0).ok_or("finger_id must be an integer, at least 1")?,
                ),
            };
            match action {
                "match" => null(d.finger_touch(finger.unwrap_or(1))),
                "nomatch" => null(d.finger_touch(finger.unwrap_or(NOMATCH_FINGER))),
                "enroll" | "unenroll" => Err(format!(
                    "`biometric` `{action}` is iOS's: Android enrolls a fingerprint in Settings (Security, after a screen lock is set), \
                     where each touch it asks for is a `biometric` `match` with the finger's id, and removes one there too"
                )),
                other => Err(format!("unknown action `{other}` (match, nomatch, enroll or unenroll)")),
            }
        }
    }
}

/// `push`'s payload as an FCM message: `data` (flat; numbers and booleans become strings, as FCM
/// sends every value) and `notification` (string fields).
fn fcm_message(payload: &Value) -> Result<FcmMessage, String> {
    let Value::Object(top) = payload else {
        return Err(format!("`payload` must be an object, not {payload}"));
    };
    if top.contains_key("aps") {
        return Err("`payload` is an APNs payload (`aps`), which is iOS's: on Android give FCM's message, \
                    {\"data\": {...}, \"notification\": {\"title\": ..., \"body\": ...}}"
            .into());
    }
    if let Some(key) = top.keys().find(|k| !matches!(k.as_str(), "data" | "notification")) {
        return Err(format!("`payload` has `{key}`; an FCM message on Android takes `data` and `notification`"));
    }
    let section = |key: &str, numbers: bool| -> Result<Vec<(String, String)>, String> {
        match top.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => Ok((k.clone(), s.clone())),
                    Value::Number(_) | Value::Bool(_) if numbers => Ok((k.clone(), v.to_string())),
                    other => Err(format!(
                        "`payload.{key}.{k}` must be a {}, not {other}",
                        if numbers { "string, number or boolean" } else { "string" }
                    )),
                })
                .collect(),
            Some(other) => Err(format!("`payload.{key}` must be an object, not {other}")),
        }
    };
    Ok(FcmMessage { data: section("data", true)?, notification: section("notification", false)? })
}

/// `notify`'s extras, each by its JSON type: a string `--es`, null `--esn`, a boolean `--ez`, an
/// integer `--ei` (or `--el` past 32 bits), another number `--ef`.
fn extras(m: &Map<String, Value>) -> Result<Vec<(String, Extra)>, String> {
    m.iter()
        .map(|(k, v)| {
            let extra = match v {
                Value::String(s) => Extra::Str(s.clone()),
                Value::Null => Extra::Null,
                Value::Bool(b) => Extra::Bool(*b),
                Value::Number(n) => match (n.as_i64(), n.as_f64()) {
                    (Some(i), _) => i32::try_from(i).map_or(Extra::Long(i), Extra::Int),
                    (None, Some(_)) if n.is_u64() => return Err(format!("extra `{k}` is past a long's range")),
                    (None, Some(f)) => Extra::Float(f),
                    (None, None) => return Err(format!("extra `{k}` isn't a number a broadcast can carry")),
                },
                other => Err(format!("extra `{k}` must be a string, number, boolean or null, not {other}"))?,
            };
            Ok((k.clone(), extra))
        })
        .collect()
}

/// Run against `sim_android::testing`'s fake adb and emulator, which are unix only.
#[cfg(all(test, unix))]
mod tests {
    use serde_json::json;
    use sim_android::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator};

    use super::*;
    use crate::{Session, call_method as call};

    struct Fixture {
        s: Session,
        root: std::path::PathBuf,
        adb: FakeAdb,
        emulator: FakeEmulator,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A session over a running AVD `phone` whose adb is the fake.
    fn fixture(tag: &str) -> Fixture {
        let dir = std::path::PathBuf::from("/tmp").join(format!("sa-anotify-{tag}-{}", std::process::id()));
        let image = dir.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(dir.join("sdk")), avd_home: dir.join("avd") });
        android.running_dir = Some(dir.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let adb = fake_adb(&android);
        let emulator = fake_emulator(&android, "phone");
        Fixture { s: Session::with_android(android), root: dir, adb, emulator }
    }

    fn run(f: &mut Fixture, method: &str, p: Value) -> Result<Value, String> {
        let mut p = p;
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(&mut f.s, method, &p)
    }

    #[test]
    fn help_lists_both_platforms_for_push_notify_and_biometric() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        for method in METHODS {
            let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == *method)).expect(method);
            assert_eq!(row["platforms"], json!(["android", "ios"]), "{method}");
        }
        let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == "reset_keychain")).expect("reset_keychain");
        assert_eq!(row["platforms"], json!(["ios"]));
    }

    #[test]
    fn push_delivers_an_fcm_message_to_the_app() {
        let mut f = fixture("push");
        let payload = json!({"data": {"order": 42, "paid": true, "note": "x"}, "notification": {"title": "Hi", "body": "Sent"}});
        assert_eq!(run(&mut f, "push", json!({"bundle_id": "com.example.app", "payload": payload})), Ok(Value::Null));
        let calls = f.adb.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        for extra in [
            "--es 'note' 'x'",
            "--es 'order' '42'",
            "--es 'paid' 'true'",
            "--es 'gcm.n.title' 'Hi'",
            "--es 'gcm.n.body' 'Sent'",
            "--es 'gcm.n.e' '1'",
        ] {
            assert!(calls[1].contains(extra), "{extra}: {}", calls[1]);
        }
    }

    #[test]
    fn push_refuses_what_isnt_an_fcm_message_before_adb_runs() {
        let mut f = fixture("pushparams");
        let app = "com.example.app";
        for (p, says) in [
            (json!({"payload": {"data": {"a": "b"}}}), "missing string parameter `bundle_id`"),
            (json!({"bundle_id": app}), "missing parameter `payload`"),
            (json!({"bundle_id": app, "payload": "hi"}), "`payload` must be an object"),
            (json!({"bundle_id": app, "payload": {"aps": {"alert": "hi"}}}), "`payload` is an APNs payload (`aps`), which is iOS's"),
            (json!({"bundle_id": app, "payload": {"priority": "high"}}), "`payload` has `priority`"),
            (json!({"bundle_id": app, "payload": {"data": []}}), "`payload.data` must be an object"),
            (json!({"bundle_id": app, "payload": {"data": {"a": {"b": 1}}}}), "`payload.data.a` must be a string, number or boolean"),
            (json!({"bundle_id": app, "payload": {"notification": {"title": 1}}}), "`payload.notification.title` must be a string"),
            (json!({"bundle_id": app, "payload": {"data": {"from": "x"}}}), "`from` is reserved by FCM"),
            (json!({"bundle_id": app, "payload": {}}), "needs `data` or `notification`"),
        ] {
            let err = run(&mut f, "push", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(f.adb.calls().is_empty(), "{:?}", f.adb.calls());
    }

    #[test]
    fn notify_broadcasts_the_name_with_typed_extras() {
        let mut f = fixture("notify");
        let extras = json!({"s": "v", "n": null, "b": false, "i": 7, "l": 5_000_000_000_i64, "f": 1.5});
        assert_eq!(
            run(&mut f, "notify", json!({"name": "com.example.PING", "bundle_id": "com.example.app", "extras": extras})),
            Ok(Value::Null)
        );
        assert_eq!(run(&mut f, "notify", json!({"name": "com.example.PING"})), Ok(Value::Null));
        let err = run(&mut f, "notify", json!({"name": "android.intent.action.BOOT_COMPLETED"})).expect_err("protected");
        assert!(err.contains("Permission Denial"), "{err}");
        assert_eq!(
            f.adb.calls(),
            [
                "shell am broadcast -a 'com.example.PING' -p com.example.app --ez 'b' false --ef 'f' 1.5 --ei 'i' 7 --el 'l' 5000000000 --esn 'n' --es 's' 'v'",
                "shell am broadcast -a 'com.example.PING'",
                "shell am broadcast -a 'android.intent.action.BOOT_COMPLETED'",
            ]
        );
    }

    #[test]
    fn notify_refuses_ios_state_and_bad_extras_before_adb_runs() {
        let mut f = fixture("notifyparams");
        for (p, says) in [
            (json!({}), "missing string parameter `name`"),
            (json!({"name": "a.b", "state": 1}), "`notify` takes `state` for iOS devices only"),
            (json!({"name": "a.b", "extras": [1]}), "`extras` must be an object"),
            (json!({"name": "a.b", "extras": {"x": [1]}}), "extra `x` must be a string, number, boolean or null"),
            (json!({"name": "a.b", "extras": {"x": u64::MAX}}), "extra `x` is past a long's range"),
        ] {
            let err = run(&mut f, "notify", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert!(f.adb.calls().is_empty(), "{:?}", f.adb.calls());
    }

    #[test]
    fn biometric_touches_the_fingerprint_sensor() {
        let mut f = fixture("finger");
        assert_eq!(run(&mut f, "biometric", json!({"action": "match"})), Ok(Value::Null));
        assert_eq!(run(&mut f, "biometric", json!({"action": "match", "finger_id": 3, "face": false})), Ok(Value::Null));
        assert_eq!(run(&mut f, "biometric", json!({"action": "nomatch"})), Ok(Value::Null));
        for (p, says) in [
            (json!({}), "missing string parameter `action`"),
            (json!({"action": "match", "face": true}), "no face sensor"),
            (json!({"action": "match", "finger_id": 0}), "finger_id must be an integer, at least 1"),
            (json!({"action": "match", "finger_id": "1"}), "finger_id must be an integer"),
            (json!({"action": "enroll"}), "`biometric` `enroll` is iOS's: Android enrolls a fingerprint in Settings"),
            (json!({"action": "unenroll"}), "`biometric` `unenroll` is iOS's"),
            (json!({"action": "wave"}), "unknown action `wave`"),
        ] {
            let err = run(&mut f, "biometric", p.clone()).expect_err(&p.to_string());
            assert!(err.contains(says), "{p}: {err}");
        }
        assert_eq!(
            *f.emulator.console.lock().expect("console"),
            ["finger touch 1", "finger remove", "finger touch 3", "finger remove", "finger touch 99", "finger remove"]
        );
    }

    #[test]
    fn reset_keychain_is_refused_on_android_with_why() {
        let mut f = fixture("keychain");
        let err = run(&mut f, "reset_keychain", json!({})).expect_err("unsupported");
        assert!(
            err.starts_with("`reset_keychain` isn't available on Android devices (it works on iOS): Android has no device-wide keychain"),
            "{err}"
        );
        assert!(f.adb.calls().is_empty());
    }

    #[test]
    fn the_api_md_rows_agree_with_the_method_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(api) = std::fs::read_to_string(root.join("docs/api.md")) else {
            eprintln!("skipped: not in the simee workspace, so docs/api.md isn't here");
            return;
        };
        for (method, params, doc) in crate::METHODS.iter().filter(|(m, ..)| METHODS.contains(m) || *m == "reset_keychain") {
            let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
            let row = format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.lines().any(|l| l == row), "docs/api.md's row for `{method}` differs from METHODS; it should be\n{row}");
        }
    }
}
