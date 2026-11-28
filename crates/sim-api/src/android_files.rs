//! The app-file methods (`containers`, `ls`, `read`, `write`, `rm`, `du`, `clear_container`) on
//! Android devices, with iOS's parameters and results: `sim_android::files` does the work, over
//! adb (`run-as` for a debuggable app, root otherwise, `adb root` only with `root: true`; paths
//! confined to one app's container, relative to the device's `/` as iOS's are to its data dir).

use base64::Engine;
use serde_json::{Value, json};
use sim_android::files::Allow;
use sim_android::{Android, Device};

use crate::params::{need_str, opt_bool, str_param};

/// The methods this module answers.
pub(crate) const METHODS: &[&str] = &["containers", "ls", "read", "write", "rm", "du", "clear_container"];

/// Answers one of [`METHODS`] for the running device `d`. Parameters are read before adb runs.
pub(crate) fn call(android: &Android, d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    let b64 = &base64::engine::general_purpose::STANDARD;
    let null = |r: Result<(), String>| r.map(|()| Value::Null);
    // `ls` and `du` without a path mean the root, as iOS's do: here the device's `/`, which is in
    // no app's container, so it is refused as any path outside one is.
    let root_or_path = str_param(p, "path").unwrap_or("");
    // `root: true` lets a call run `adb root` (it restarts adbd, dropping the device's forwards);
    // `pm_clear: true` lets clear_container run `pm clear`. Neither is done unasked.
    let allow = Allow {
        adb_root: opt_bool(p, "root")?.unwrap_or(false),
        pm_clear: method == "clear_container" && opt_bool(p, "pm_clear")?.unwrap_or(false),
    };
    match method {
        "containers" => {
            let sizes = opt_bool(p, "sizes")?.unwrap_or(false);
            let apps = d.adb(android)?.containers(sizes, allow)?;
            Ok(Value::Array(
                apps.into_iter()
                    .map(|a| {
                        json!({"bundle_id": a.package, "data": a.data, "bundle": a.bundle, "data_bytes": a.data_bytes,
                            "bundle_bytes": a.bundle_bytes})
                    })
                    .collect(),
            ))
        }
        "ls" => {
            let entries = d.adb(android)?.ls(root_or_path, allow)?;
            Ok(Value::Array(entries.into_iter().map(|e| json!({"name": e.name, "dir": e.dir, "bytes": e.bytes})).collect()))
        }
        "read" => {
            let path = need_str(p, "path")?;
            let bytes = d.adb(android)?.read_file(path, allow)?;
            Ok(json!({"bytes": bytes.len(), "base64": b64.encode(&bytes), "text": String::from_utf8(bytes).ok()}))
        }
        "write" => {
            let path = need_str(p, "path")?;
            let bytes = match (str_param(p, "text"), str_param(p, "base64")) {
                (Some(t), _) => t.as_bytes().to_vec(),
                (None, Some(b)) => b64.decode(b).map_err(|e| e.to_string())?,
                (None, None) => return Err("give `text` or `base64`".into()),
            };
            null(d.adb(android)?.write_file(path, &bytes, allow))
        }
        "rm" => {
            let path = need_str(p, "path")?;
            null(d.adb(android)?.remove(path, allow))
        }
        "du" => Ok(json!({"bytes": d.adb(android)?.du(root_or_path, allow)?})),
        _ => {
            let path = need_str(p, "path")?;
            null(d.adb(android)?.clear_container(path, allow))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sim_android::files::fake::{FakeFiles, Root, fake_files_adb};
    use sim_android::testing::{FakeEmulator, fake_emulator};

    use crate::methods::{Session, call};

    struct Fixture {
        s: Session,
        root: std::path::PathBuf,
        fake: FakeFiles,
        _emulator: FakeEmulator,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A session over a running AVD `phone` whose adb is the files fake.
    fn fixture(tag: &str, root: Root) -> Fixture {
        let dir = std::path::PathBuf::from("/tmp").join(format!("sa-afiles-{tag}-{}", std::process::id()));
        let image = dir.join("sdk/system-images/android-34/google_apis/x86_64");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=34\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(dir.join("sdk")), avd_home: dir.join("avd") });
        android.running_dir = Some(dir.join("running"));
        android.create("phone", "system-images;android-34;google_apis;x86_64", None).expect("create");
        let fake = fake_files_adb(&android, root);
        let _emulator = fake_emulator(&android, "phone");
        Fixture { s: Session::with_android(android), root: dir, fake, _emulator }
    }

    fn run(f: &mut Fixture, method: &str, p: Value) -> Result<Value, String> {
        let mut p = p;
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(&mut f.s, method, &p)
    }

    #[test]
    fn help_lists_both_platforms_for_the_file_methods() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        for method in super::METHODS {
            let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == *method)).expect(method);
            assert_eq!(row["platforms"], json!(["android", "ios"]), "{method}");
        }
    }

    #[test]
    fn the_file_methods_answer_as_on_ios() {
        let mut f = fixture("shapes", Root::None);
        assert_eq!(
            run(&mut f, "containers", json!({})),
            Ok(json!([
                {"bundle_id": "com.example.app", "data": "data/user/0/com.example.app", "bundle": "data/app/~~Ab==/com.example.app-Cd==",
                    "data_bytes": 0, "bundle_bytes": 0},
                {"bundle_id": "com.example.release", "data": "data/user/0/com.example.release",
                    "bundle": "data/app/~~Ef==/com.example.release-Gh==", "data_bytes": 0, "bundle_bytes": 0},
            ]))
        );
        let files = "data/user/0/com.example.app/files";
        assert_eq!(
            run(&mut f, "ls", json!({"path": files})),
            Ok(
                json!([{"name": "notes.txt", "dir": false, "bytes": 5}, {"name": "out", "dir": false, "bytes": f.fake.outside.to_string_lossy().len()}])
            )
        );
        assert_eq!(
            run(&mut f, "read", json!({"path": format!("{files}/notes.txt")})),
            Ok(json!({"bytes": 5, "base64": "aGVsbG8=", "text": "hello"}))
        );
        // Binary through base64, both ways: not UTF-8, so no text.
        let blob = format!("{files}/blob.bin");
        assert_eq!(run(&mut f, "write", json!({"path": blob, "base64": "AP8NCgA="})), Ok(Value::Null));
        assert_eq!(run(&mut f, "read", json!({"path": blob})), Ok(json!({"bytes": 5, "base64": "AP8NCgA=", "text": null})));
        assert_eq!(run(&mut f, "write", json!({"path": blob, "text": "t", "base64": "AA=="})), Ok(Value::Null), "text wins, as on iOS");
        assert_eq!(run(&mut f, "du", json!({"path": files})), Ok(json!({"bytes": 6})));
        assert_eq!(run(&mut f, "rm", json!({"path": blob})), Ok(Value::Null));
        assert_eq!(run(&mut f, "clear_container", json!({"path": "data/user/0/com.example.app"})), Ok(Value::Null));
        assert_eq!(run(&mut f, "ls", json!({"path": files})), Err(format!("{files}: No such file or directory")));
    }

    #[test]
    fn parameters_are_checked_before_adb_runs() {
        let mut f = fixture("params", Root::None);
        for method in ["read", "write", "rm", "clear_container"] {
            assert_eq!(run(&mut f, method, json!({})), Err("missing string parameter `path`".into()), "{method}");
        }
        for method in ["ls", "du"] {
            let err = run(&mut f, method, json!({})).expect_err(method);
            assert!(err.starts_with("path is outside every app container: "), "{method}: {err}");
        }
        assert_eq!(run(&mut f, "write", json!({"path": "data/data/com.example.app/x"})), Err("give `text` or `base64`".into()));
        assert!(run(&mut f, "containers", json!({"sizes": "yes"})).is_err());
        assert!(f.fake.calls().is_empty(), "{:?}", f.fake.calls());
        // Escapes and the no-access error reach the caller as they are.
        assert_eq!(
            run(&mut f, "read", json!({"path": "data/data/com.example.app/../x"})),
            Err("path escapes the container: data/data/com.example.app/../x".into())
        );
        let err = run(&mut f, "read", json!({"path": "data/data/com.example.release/files/r.txt"})).expect_err("no access");
        assert!(err.starts_with("can't reach com.example.release's files"), "{err}");
    }

    #[test]
    fn the_api_md_rows_agree_with_the_method_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Ok(api) = std::fs::read_to_string(root.join("docs/api.md")) else {
            eprintln!("skipped: not in the simee workspace, so docs/api.md isn't here");
            return;
        };
        for (method, params, doc) in crate::METHODS.iter().filter(|(m, ..)| super::METHODS.contains(m)) {
            let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
            let row = format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.contains(&row), "docs/api.md's row for `{method}` differs from METHODS; it should be\n{row}");
        }
    }

    #[test]
    fn adb_root_and_pm_clear_run_only_when_asked_for() {
        let mut f = fixture("consent", Root::AdbRoot);
        let release = "data/data/com.example.release/files/r.txt";
        for p in [json!({"path": release, "root": "yes"}), json!({"path": "data/data/com.example.release", "pm_clear": 1})] {
            let method = if p.get("pm_clear").is_some() { "clear_container" } else { "read" };
            assert!(run(&mut f, method, p).is_err());
        }
        assert!(f.fake.calls().is_empty(), "checked before adb: {:?}", f.fake.calls());
        let err = run(&mut f, "read", json!({"path": release})).expect_err("no consent");
        assert!(err.contains("`root: true`"), "{err}");
        assert!(!f.fake.rooted());
        assert_eq!(
            run(&mut f, "read", json!({"path": release, "root": true})),
            Ok(json!({"bytes": 7, "base64": "cmVsZWFzZQ==", "text": "release"}))
        );
        assert!(f.fake.rooted());

        let mut f = fixture("consentpm", Root::None);
        let err = run(&mut f, "clear_container", json!({"path": "data/user/10/com.example.release"})).expect_err("no way in");
        assert!(err.contains("`pm_clear: true`") && err.contains("pm clear --user 10 com.example.release"), "{err}");
        assert_eq!(run(&mut f, "clear_container", json!({"path": "data/user/10/com.example.release", "pm_clear": true})), Ok(Value::Null));
        assert!(f.fake.calls().contains(&"shell pm clear --user 10 com.example.release".to_owned()), "{:?}", f.fake.calls());
    }
}
