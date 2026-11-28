//! End-to-end tests against a real booted device. Skipped unless `SIMEE_E2E_UDID` names one:
//!
//! ```text
//! SIMEE_E2E_UDID=$(simee create e2e "iPhone 17" "iOS 26.5") && simee boot $SIMEE_E2E_UDID
//! SIMEE_E2E_UDID=... cargo nextest run -p simee --test e2e
//! ```

use std::process::{Command, Output};

fn udid() -> Option<String> {
    std::env::var("SIMEE_E2E_UDID").ok().filter(|s| !s.is_empty())
}

fn sim(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_simee")).args(args).output().expect("simee runs")
}

fn ok(args: &[&str]) -> String {
    let out = sim(args);
    assert!(out.status.success(), "simee {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn device_is_listed_as_booted_with_the_same_identity_simctl_reports() {
    let Some(udid) = udid() else { return };
    let ours: serde_json::Value = serde_json::from_str(&ok(&["list", "--json", "--booted"])).expect("json");
    let ours = ours.as_array().expect("array").iter().find(|d| d["udid"] == udid.as_str()).expect("listed").clone();
    let simctl = Command::new("xcrun").args(["simctl", "list", "-j", "devices"]).output().expect("simctl");
    let theirs: serde_json::Value = serde_json::from_slice(&simctl.stdout).expect("simctl json");
    let (runtime, device) = theirs["devices"]
        .as_object()
        .expect("devices")
        .iter()
        .find_map(|(rt, list)| list.as_array()?.iter().find(|d| d["udid"] == udid.as_str()).map(|d| (rt.clone(), d.clone())))
        .expect("simctl lists it");
    assert_eq!(ours["name"], device["name"]);
    assert_eq!(device["state"], "Booted");
    assert_eq!(ours["runtime_id"], runtime.as_str());
    assert_eq!(ours["device_type_id"], device["deviceTypeIdentifier"]);
}

#[test]
fn screenshot_matches_the_device_type_resolution() {
    let Some(udid) = udid() else { return };
    let dir = std::env::temp_dir().join(format!("simee-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp");
    let path = dir.join("screen.png");
    ok(&["screenshot", &udid, path.to_str().expect("utf8")]);
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).expect("png")));
    let info = decoder.read_info().expect("header").info().clone();
    let types: serde_json::Value = serde_json::from_str(&ok(&["device-types", "--json"])).expect("json");
    let list: serde_json::Value = serde_json::from_str(&ok(&["list", "--json"])).expect("json");
    let type_id = list.as_array().expect("array").iter().find(|d| d["udid"] == udid.as_str()).expect("listed")["device_type_id"].clone();
    let ty = types.as_array().expect("array").iter().find(|t| t["identifier"] == type_id).expect("type");
    let (w, h) = (ty["screen_width"].as_f64().expect("w") as u32, ty["screen_height"].as_f64().expect("h") as u32);
    assert!((info.width, info.height) == (w, h) || (info.width, info.height) == (h, w), "{}x{} vs {w}x{h}", info.width, info.height);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn input_and_app_commands_succeed() {
    let Some(udid) = udid() else { return };
    ok(&["button", &udid, "home"]);
    ok(&["tap", &udid, "10", "10"]);
    ok(&["swipe", &udid, "600", "1800", "600", "1200", "--duration", "200"]);
    let pid: i32 = ok(&["launch", &udid, "com.apple.Preferences"]).trim().parse().expect("pid");
    assert!(pid > 0);
    ok(&["terminate", &udid, "com.apple.Preferences"]);
    assert!(ok(&["apps", &udid]).lines().any(|l| l == "com.apple.Preferences"));
    ok(&["location", &udid, "37.33", "-122.03"]);
    ok(&["button", &udid, "home"]);
}

#[test]
fn unknown_device_fails_cleanly() {
    let out = sim(&["tap", "no-such-device", "1", "1"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no device matches"), "stderr: {stderr}");
}
