//! The CLI end to end against Android devices, on any host: `simee` runs as a subprocess with
//! its own SDK, AVD home and `$HOME`, and a fake simee (`sim_android::testing`) stands in for
//! the running emulator: its console records commands, its display socket records input. Unix only
//! until the API has a Windows transport: the fake's display is a Unix socket.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use sim_android::testing::{fake_emulator, wait_for};

struct Env {
    root: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Env {
    fn new(tag: &str) -> Env {
        // Under /tmp: the fake display socket's path must fit a unix socket address.
        let root = PathBuf::from("/tmp").join(format!("cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (dir, tag, build_prop) in [
            ("android-36/google_apis/arm64-v8a", "google_apis", ""),
            ("android-36/android-wear-signed/arm64-v8a", "android-wear-signed", ""),
            ("android-34-ext9/android-automotive/arm64-v8a", "android-automotive", "ro.product.name=sdk_car_arm64\n"),
        ] {
            let d = root.join("sdk/system-images").join(dir);
            std::fs::create_dir_all(&d).expect("image dir");
            std::fs::write(d.join("system.img"), b"s").expect("system.img");
            std::fs::write(d.join("source.properties"), format!("AndroidVersion.ApiLevel=36\nSystemImage.TagId={tag}\n")).expect("props");
            if !build_prop.is_empty() {
                std::fs::write(d.join("build.prop"), build_prop).expect("build.prop");
            }
        }
        std::fs::create_dir_all(root.join("home")).expect("home");
        Env { root }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_simee"));
        c.env("HOME", self.root.join("home"))
            .env("ANDROID_HOME", self.root.join("sdk"))
            .env("ANDROID_AVD_HOME", self.root.join("avd"))
            .env("SIMEE_SOCKET", self.root.join("api.sock"))
            .env_remove("ANDROID_SDK_ROOT");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("simee runs")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "simee {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "simee {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// The same backend the CLI builds from this environment, to start a fake emulator in.
    fn android(&self) -> sim_android::Android {
        let mut a = sim_android::Android::new(emu_avd::Env { sdk_root: Some(self.root.join("sdk")), avd_home: self.root.join("avd") });
        a.running_dir = Some(self.root.join("home/Library/Caches/TemporaryItems/avd/running"));
        a
    }
}

#[test]
fn devices_are_created_listed_driven_and_deleted() {
    let e = Env::new("drive");
    let image = "system-images;android-36;google_apis;arm64-v8a";
    assert_eq!(e.ok(&["create", "fold", "--platform", "android", "--system-image", image, "--form-factor", "foldable"]).trim(), "fold");
    e.ok(&["create", "watch", "--platform", "android", "--system-image", "system-images;android-36;android-wear-signed;arm64-v8a"]);
    let err = e.err(&[
        "create",
        "t",
        "--platform",
        "android",
        "--system-image",
        "system-images;android-36;android-wear-signed;arm64-v8a",
        "--form-factor",
        "tablet",
    ]);
    assert!(err.contains("boots Wear OS · Watch devices, not tablet"), "{err}");

    let list: serde_json::Value = serde_json::from_str(&e.ok(&["list", "--json", "--platform", "android"])).expect("json");
    let kinds: Vec<(String, String)> = list
        .as_array()
        .expect("array")
        .iter()
        .map(|d| (d["udid"].as_str().unwrap_or_default().to_owned(), d["form_factor"].as_str().unwrap_or_default().to_owned()))
        .collect();
    assert_eq!(kinds, [("fold".to_owned(), "foldable".to_owned()), ("watch".to_owned(), "watch".to_owned())]);
    let text = e.ok(&["list", "--platform", "android", "--form-factor", "watch"]);
    assert!(text.contains("watch") && !text.contains("fold"), "{text}");
    assert!(e.err(&["tap", "fold", "1", "1"]).contains("isn't running"), "stopped devices refuse input");

    let fake = fake_emulator(&e.android(), "fold");
    let list: serde_json::Value = serde_json::from_str(&e.ok(&["list", "--json", "--platform", "android"])).expect("json");
    assert_eq!(list[0]["state"], "Booted", "the advertisement makes it booted");

    e.ok(&["tap", "fold", "10", "20"]);
    e.ok(&["swipe", "fold", "300", "900", "300", "300", "--duration", "50"]);
    e.ok(&["button", "fold", "home"]);
    e.ok(&["type", "fold", "Hi"]);
    e.ok(&["location", "fold", "37.5", "-122.25"]);
    e.ok(&["rotate", "fold"]);
    assert!(e.ok(&["fold", "fold", "half-opened"]).starts_with("half-opened"));
    assert!(e.ok(&["console", "fold", "ping"]).contains("I am alive!"));
    let shot = e.root.join("shot.png");
    e.ok(&["screenshot", "fold", shot.to_str().expect("utf8")]);
    assert!(std::fs::read(&shot).expect("png").starts_with(b"\x89PNG"));

    let console = wait_for(&fake.console, 9);
    for want in ["event send EV_KEY:KEY_HOME:1", "geo fix -122.25 37.5", "rotate", "ping"] {
        assert!(console.iter().any(|l| l == want), "{want} not in {console:?}");
    }
    let display = wait_for(&fake.display, 4);
    assert_eq!(display[..2], ["touch 10 20 down", "touch 10 20 up"]);
    assert!(display.iter().any(|l| l.starts_with("touch 300 300")), "the swipe ends where asked: {display:?}");

    assert!(e.err(&["rotary", "fold", "2"]).contains("needs a device with Rotary"));
    assert!(e.err(&["delete", "fold"]).contains("is running"));
    e.ok(&["delete", "watch"]);
    assert!(!e.root.join("avd/watch.avd").exists());
}

#[test]
fn the_vehicle_hal_reads_through_the_console() {
    let e = Env::new("car");
    e.ok(&["create", "car", "--platform", "android", "--system-image", "system-images;android-34-ext9;android-automotive;arm64-v8a"]);
    let _fake = fake_emulator(&e.android(), "car");
    let gear = e.ok(&["vehicle", "car", "gear"]);
    assert!(gear.contains("GEAR_SELECTION") && gear.contains('8'), "{gear}");
    let all: serde_json::Value = serde_json::from_str(&e.ok(&["vehicle", "car", "--json"])).expect("json");
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    e.ok(&["vehicle", "car", "gear", "drive"]);
}

#[test]
fn the_api_serves_the_same_methods_over_its_socket() {
    let e = Env::new("serve");
    e.ok(&["create", "phone", "--platform", "android", "--system-image", "system-images;android-36;google_apis;arm64-v8a"]);
    let mut server = e.cmd().arg("serve").spawn().expect("serve");
    let socket = e.root.join("api.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !Path::new(&socket).exists() {
        assert!(Instant::now() < deadline, "the server never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let devices = e.ok(&["call", "devices", r#"{"platform":"android"}"#]);
    let _ = server.kill();
    let _ = server.wait();
    let devices: serde_json::Value = serde_json::from_str(&devices).expect("json");
    assert_eq!(devices[0]["udid"], "phone");
    assert_eq!(devices[0]["platform"], "android");
}

#[test]
fn form_factors_print_the_support_matrix() {
    let e = Env::new("ff");
    let text = e.ok(&["form-factors"]);
    for want in ["Wear OS", "Android TV", "Android Automotive", "Android XR", "CarPlay"] {
        assert!(text.contains(want), "{want} missing from\n{text}");
    }
}
