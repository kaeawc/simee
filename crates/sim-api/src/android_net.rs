//! Android's network methods (#416), in iOS's shapes.
//!
//! - `network_shape`, `network_state`, `network_clear`: the emulator's NAT through its console
//!   (`network speed <up>:<down>`, `network delay <ms>`, `network status`: simee's emulator and
//!   the stock one answer alike), loss as simee's `network loss <pct>` (that percent of packets
//!   dropped each way; the stock console has none, so there it is refused with the reason), and
//!   offline as airplane mode over adb. Per-host rules, source-port ranges, the proxy and
//!   injection are iOS's shaper's: refused, before anything runs, with the reason.
//! - `connections`: the guest kernel's socket tables matched to processes by socket inode
//!   (`sim_android::net`), as iOS's rows plus each socket's `uid`; `pid` is null for a socket no
//!   process could be matched to.
//! - `traffic`: bytes in and out per app uid since boot (the kernel's per-uid counters), as
//!   iOS's rows plus `uid`; `pid` is the app's own process, null when it isn't running or the uid
//!   is shared.
//!
//! `network_flows` and `network_env` stay iOS's, and say why in their help: emu's NAT keeps
//! only its live flows, with no history or byte counts per flow, and an Android app starts from an
//! intent with zygote's environment while the NAT shapes the whole device, so no environment
//! shapes one app's launch.

use serde_json::{Value, json};
use sim_android::{Android, Device};
use sim_core::network::Conditions;
use sim_netshape::Profile;

use crate::params::shape_profile;

/// `network_shape` parameters only iOS shapes, and why.
const IOS_SHAPE_PARAMS: &[(&str, &str)] = &[
    ("rules", "the emulator's NAT shapes the whole device, not one host"),
    ("port_base", "a source-port range is for the iOS in-process shaper"),
    ("port_count", "a source-port range is for the iOS in-process shaper"),
    ("proxy", "the emulator's NAT shapes every flow, with no proxy"),
    ("inject", "the shaper is injected into a simulator's processes"),
];

/// Dispatches the Android network methods.
pub(crate) fn call(android: &Android, d: &Device, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        "network_shape" => shape(android, d, p),
        "network_state" => {
            let (profile, lines) = current(android, d)?;
            Ok(json!({"profile": profile, "proxy_port": null, "injected": false, "boot_injected": false, "network_status": lines}))
        }
        "network_clear" => {
            // Loss only where there is any: the stock console has no `network loss`.
            let loss = d.check_network_loss().is_ok().then_some("none");
            d.set_network(Some("full"), Some("none"), loss)?;
            // Only when it is on: API 29 has no `cmd connectivity airplane-mode`, and an online
            // device has nothing to undo.
            let adb = d.adb(android)?;
            if adb.airplane_mode()? {
                adb.set_airplane_mode(false)?;
            }
            Ok(Value::Null)
        }
        "connections" => Ok(d.adb(android)?.sockets()?.sockets.iter().map(connection).collect()),
        "traffic" => Ok(d
            .adb(android)?
            .traffic()?
            .into_iter()
            .map(|t| json!({"pid": t.pid, "process": t.process, "bytes_in": t.bytes_in, "bytes_out": t.bytes_out, "uid": t.uid}))
            .collect()),
        other => Err(format!("`{other}` isn't implemented for Android devices")),
    }
}

/// `network_shape`: the profile asked for over what the device has now; only what changed is
/// sent (so a latency range set from the console stays when only the speed changes).
fn shape(android: &Android, d: &Device, p: &Value) -> Result<Value, String> {
    for (key, why) in IOS_SHAPE_PARAMS {
        if asks(p.get(*key)) {
            return Err(format!("`network_shape` takes `{key}` for iOS devices only ({why})"));
        }
    }
    // Checked before the device is read: what a preset or loss_pct asks for doesn't depend on it,
    // and whether the device can lose packets is in its advertisement.
    if shape_profile(p, Profile::default())?.loss_pct > 0 {
        d.check_network_loss().map_err(|why| {
            format!("`network_shape` takes `loss_pct` (and so the lossy and very-bad presets) on simee's emu only: {why}")
        })?;
    }
    let (base, _) = current(android, d)?;
    let want = shape_profile(p, base.clone())?;
    let speed = ((want.up_kbps, want.down_kbps) != (base.up_kbps, base.down_kbps)).then(|| match (want.up_kbps, want.down_kbps) {
        (0, 0) => "full".to_owned(),
        (up, down) => format!("{up}:{down}"),
    });
    let delay = (want.latency_ms != base.latency_ms).then(|| match want.latency_ms {
        0 => "none".to_owned(),
        ms => ms.to_string(),
    });
    let loss = (want.loss_pct != base.loss_pct).then(|| match want.loss_pct {
        0 => "none".to_owned(),
        pct => pct.to_string(),
    });
    let mut commands = d.set_network(speed.as_deref(), delay.as_deref(), loss.as_deref())?;
    if want.offline != base.offline {
        commands.extend(d.adb(android)?.set_airplane_mode(want.offline)?);
    }
    Ok(json!({"profile": want, "proxy_port": null, "injected": false, "inject_mode": "none", "commands": commands}))
}

/// Whether a parameter asks for something: not absent, `null`, `false`, `0` or empty.
fn asks(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// The device's shaping as a profile: the console's conditions (a latency range, and a loss
/// that differs each way, read as their mean) and airplane mode as offline; with the console's
/// report.
fn current(android: &Android, d: &Device) -> Result<(Profile, Vec<String>), String> {
    let (c, lines) = d.network_conditions()?;
    let offline = d.adb(android)?.airplane_mode()?;
    Ok((profile_of(&c, offline), lines))
}

fn profile_of(c: &Conditions, offline: bool) -> Profile {
    let kbps = |bps: f64| (bps / 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32;
    let latency = (i64::from(c.min_latency_ms.max(0)) + i64::from(c.max_latency_ms.max(0))) / 2;
    Profile {
        offline,
        latency_ms: u32::try_from(latency).unwrap_or(u32::MAX),
        down_kbps: kbps(c.download_bps),
        up_kbps: kbps(c.upload_bps),
        loss_pct: ((c.upload_loss_pct + c.download_loss_pct) / 2.0).round().clamp(0.0, 100.0) as u32,
        ..Profile::default()
    }
}

/// A socket as iOS's `connections` row (`lsof`'s names and endpoints), with its `uid`.
fn connection(s: &sim_android::net::Socket) -> Value {
    let row = sim_debug::net::Connection::from_socket(&sim_debug::net::Socket {
        pid: s.pid.unwrap_or_default(),
        process: s.process.clone(),
        fd: s.fd.unwrap_or_default(),
        protocol: s.protocol,
        local: s.local,
        remote: s.remote,
        tcp_state: s.tcp_state,
    });
    json!({"pid": s.pid, "process": row.process, "protocol": row.protocol, "local": row.local, "remote": row.remote, "state": row.state,
        "uid": s.uid})
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::{Value, json};
    use sim_android::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator, fake_stock_emulator};

    use crate::methods::{Session, call};

    /// The methods Android has, and the ones that stay iOS's (each says why in its help).
    const SHARED: &[&str] = &["network_shape", "network_state", "network_clear", "connections", "traffic"];
    const IOS_ONLY: &[&str] = &["network_flows", "network_env"];

    fn session(tag: &str) -> (Session, std::path::PathBuf, FakeAdb, FakeEmulator) {
        session_as(tag, false)
    }

    /// A session whose device runs under simee's emu, or under the stock emulator (`stock`).
    fn session_as(tag: &str, stock: bool) -> (Session, std::path::PathBuf, FakeAdb, FakeEmulator) {
        let root = std::path::PathBuf::from("/tmp").join(format!("sa-net-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/x86_64");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;x86_64", None).expect("create");
        let adb = fake_adb(&android);
        let emulator = if stock { fake_stock_emulator(&android, "phone") } else { fake_emulator(&android, "phone") };
        (Session::with_android(android), root, adb, emulator)
    }

    fn run(s: &mut Session, method: &str, p: Value) -> Result<Value, String> {
        let mut p = p;
        p["device"] = json!("phone");
        p["platform"] = json!("android");
        call(s, method, &p)
    }

    fn shaping(emulator: &FakeEmulator) -> Vec<String> {
        emulator.console.lock().expect("console").iter().filter(|c| c.starts_with("network ") && *c != "network status").cloned().collect()
    }

    fn profile(name: &str) -> Value {
        serde_json::to_value(sim_netshape::Profile::preset(name).expect("preset")).expect("json")
    }

    #[test]
    fn help_lists_the_platforms_and_the_ios_only_ones_say_why() {
        let help = call(&mut Session::default(), "help", &Value::Null).expect("help");
        let row = |m: &str| help.as_array().and_then(|a| a.iter().find(|x| x["method"] == m)).cloned().expect(m);
        for method in SHARED {
            assert_eq!(row(method)["platforms"], json!(["android", "ios"]), "{method}");
        }
        for method in IOS_ONLY {
            let r = row(method);
            assert_eq!(r["platforms"], json!(["ios"]), "{method}");
            assert!(r["doc"].as_str().is_some_and(|d| d.contains("not on Android: ")), "{method} says why: {r}");
        }
    }

    #[test]
    fn the_ios_only_ones_fail_on_android_naming_ios() {
        let (mut s, root, adb, emulator) = session("iosonly");
        for method in IOS_ONLY {
            assert_eq!(run(&mut s, method, json!({})), Err(format!("`{method}` isn't available on Android devices (it works on iOS)")));
        }
        assert!(adb.calls().is_empty() && shaping(&emulator).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn network_shape_sets_the_consoles_speed_and_delay_from_a_preset() {
        let (mut s, root, adb, emulator) = session("preset");
        let out = run(&mut s, "network_shape", json!({"preset": "3g"})).expect("shape");
        assert_eq!(
            out,
            json!({"profile": profile("3g"), "proxy_port": null, "injected": false, "inject_mode": "none",
                "commands": ["network speed 330:780", "network delay 200"]})
        );
        // A field alone changes only what it names: the rest is read back from the device.
        let out = run(&mut s, "network_shape", json!({"latency_ms": 50, "proxy": false, "inject": false, "rules": []})).expect("latency");
        assert_eq!(out["commands"], json!(["network delay 50"]));
        assert_eq!(
            (&out["profile"]["down_kbps"], &out["profile"]["up_kbps"], &out["profile"]["latency_ms"]),
            (&json!(780), &json!(330), &json!(50))
        );
        assert_eq!(shaping(&emulator), ["network speed 330:780", "network delay 200", "network delay 50"]);
        assert!(!adb.calls().iter().any(|c| c.contains("airplane-mode")), "online throughout: {:?}", adb.calls());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn offline_is_airplane_mode_and_state_reads_it_back() {
        let (mut s, root, adb, emulator) = session("offline");
        let out = run(&mut s, "network_shape", json!({"preset": "offline"})).expect("offline");
        assert_eq!(out["profile"], profile("offline"));
        assert_eq!(out["commands"], json!(["cmd connectivity airplane-mode enable"]), "unshaped speed and delay stay as they are");
        let state = run(&mut s, "network_state", json!({})).expect("state");
        assert_eq!(state["profile"], profile("offline"));
        assert_eq!((&state["proxy_port"], &state["injected"], &state["boot_injected"]), (&Value::Null, &json!(false), &json!(false)));
        assert_eq!(state["network_status"][0], "Current network status:");
        let out = run(&mut s, "network_shape", json!({"preset": "edge"})).expect("edge");
        assert_eq!(out["commands"], json!(["network speed 200:240", "network delay 400", "cmd connectivity airplane-mode disable"]));
        assert_eq!(run(&mut s, "network_state", json!({})).expect("state")["profile"], profile("edge"));
        assert!(adb.calls().contains(&"shell cmd connectivity airplane-mode enable".to_owned()));
        assert_eq!(shaping(&emulator), ["network speed 200:240", "network delay 400"]);
        std::fs::remove_dir_all(root).ok();
    }

    /// Makes the fake guest an older Android.
    fn api(root: &std::path::Path, level: u32) {
        std::fs::write(root.join("sdk/platform-tools/sdk"), format!("{level}\n")).expect("sdk");
    }

    #[test]
    fn api_29_has_no_airplane_mode_command_so_offline_and_clear_use_the_setting() {
        let (mut s, root, adb, _emulator) = session("api29");
        api(&root, 29);
        let out = run(&mut s, "network_shape", json!({"preset": "offline"})).expect("offline");
        assert_eq!(
            out["commands"],
            json!(["settings put global airplane_mode_on 1", "am broadcast -a android.intent.action.AIRPLANE_MODE --ez state true"])
        );
        assert_eq!(run(&mut s, "network_state", json!({})).expect("state")["profile"], profile("offline"));
        assert_eq!(run(&mut s, "network_clear", json!({})), Ok(Value::Null));
        assert_eq!(run(&mut s, "network_state", json!({})).expect("state")["profile"], profile("none"));
        // Online already: clear touches nothing.
        let before = adb.calls().len();
        assert_eq!(run(&mut s, "network_clear", json!({})), Ok(Value::Null));
        assert!(!adb.calls()[before..].iter().any(|c| c.contains("airplane_mode_on 0") || c.contains("am broadcast")), "{:?}", adb.calls());
        assert!(!adb.calls().iter().any(|c| c.contains("cmd connectivity")), "{:?}", adb.calls());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn api_30_and_later_use_the_connectivity_command() {
        let (mut s, root, adb, _emulator) = session("api30");
        api(&root, 30);
        let out = run(&mut s, "network_shape", json!({"preset": "offline"})).expect("offline");
        assert_eq!(out["commands"], json!(["cmd connectivity airplane-mode enable"]));
        assert_eq!(run(&mut s, "network_clear", json!({})), Ok(Value::Null));
        assert!(adb.calls().contains(&"shell cmd connectivity airplane-mode disable".to_owned()), "{:?}", adb.calls());
        assert!(!adb.calls().iter().any(|c| c.contains("am broadcast")), "{:?}", adb.calls());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn network_clear_removes_every_condition() {
        let (mut s, root, adb, emulator) = session("clear");
        run(&mut s, "network_shape", json!({"preset": "lte", "offline": true})).expect("shape");
        assert_eq!(run(&mut s, "network_clear", json!({})), Ok(Value::Null));
        assert_eq!(&shaping(&emulator)[2..], ["network speed full", "network delay none", "network loss none"]);
        assert_eq!(adb.calls().last().map(String::as_str), Some("shell cmd connectivity airplane-mode disable"));
        let state = run(&mut s, "network_state", json!({})).expect("state");
        assert_eq!(state["profile"], profile("none"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn network_shape_refuses_what_only_ios_shapes_before_anything_runs() {
        let (mut s, root, adb, emulator) = session("refuse");
        for (p, says) in [
            (json!({"rules": [{"pattern": "*", "offline": true}]}), "`rules`"),
            (json!({"port_base": 40000}), "`port_base`"),
            (json!({"port_count": 10}), "`port_count`"),
            (json!({"proxy": true}), "`proxy`"),
            (json!({"inject": "boot"}), "`inject`"),
        ] {
            let err = run(&mut s, "network_shape", p.clone()).expect_err("refused");
            assert!(err.contains(says) && err.contains("iOS"), "{p}: {err}");
        }
        assert_eq!(run(&mut s, "network_shape", json!({"preset": "warp"})), Err("unknown preset `warp`".into()));
        assert!(adb.calls().is_empty(), "{:?}", adb.calls());
        assert!(emulator.console.lock().expect("console").is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    /// iOS's loss presets (lossy: 100 ms, 2000/1000 kbit/s, 10%; very-bad: 500 ms, 1000/1000
    /// kbit/s, 10%) become simee's `network loss`, read back by `network_state`.
    #[test]
    fn loss_presets_and_loss_pct_map_to_simees_network_loss() {
        let (mut s, root, _adb, emulator) = session("loss");
        let out = run(&mut s, "network_shape", json!({"preset": "lossy"})).expect("lossy");
        assert_eq!(out["profile"], profile("lossy"));
        assert_eq!(out["commands"], json!(["network speed 1000:2000", "network delay 100", "network loss 10"]));
        assert_eq!(run(&mut s, "network_state", json!({})).expect("state")["profile"], profile("lossy"));
        let out = run(&mut s, "network_shape", json!({"preset": "very-bad"})).expect("very-bad");
        assert_eq!(out["profile"], profile("very-bad"));
        assert_eq!(out["commands"], json!(["network speed 1000:1000", "network delay 500"]), "the loss is already 10%");
        // loss_pct alone changes only the loss; over 100 is capped as on iOS.
        let out = run(&mut s, "network_shape", json!({"loss_pct": 30})).expect("loss_pct");
        assert_eq!(out["commands"], json!(["network loss 30"]));
        assert_eq!(run(&mut s, "network_state", json!({})).expect("state")["profile"]["loss_pct"], 30);
        assert_eq!(run(&mut s, "network_shape", json!({"loss_pct": 400})).expect("capped")["commands"], json!(["network loss 100"]));
        // A preset without loss clears it; so does network_clear.
        assert_eq!(run(&mut s, "network_shape", json!({"preset": "3g"})).expect("3g")["commands"][2], "network loss none");
        run(&mut s, "network_shape", json!({"loss_pct": 5})).expect("loss again");
        assert_eq!(run(&mut s, "network_clear", json!({})), Ok(Value::Null));
        assert_eq!(shaping(&emulator).last().map(String::as_str), Some("network loss none"));
        assert_eq!(run(&mut s, "network_state", json!({})).expect("state")["profile"], profile("none"));
        std::fs::remove_dir_all(root).ok();
    }

    /// The stock emulator's console has no packet loss: loss is refused there before anything
    /// runs, saying why, while the rest still shapes and clear sends no loss command.
    #[test]
    fn the_stock_emulator_refuses_loss_and_says_why() {
        let (mut s, root, adb, emulator) = session_as("stockloss", true);
        for p in [json!({"loss_pct": 10}), json!({"preset": "lossy"}), json!({"preset": "very-bad"})] {
            let err = run(&mut s, "network_shape", p.clone()).expect_err("refused");
            assert!(err.contains("`loss_pct`") && err.contains("stock emulator") && err.contains("no `network loss`"), "{p}: {err}");
        }
        assert!(adb.calls().is_empty(), "{:?}", adb.calls());
        assert!(emulator.console.lock().expect("console").is_empty());
        // No loss asked for: shaped as before, and loss 0 is no request.
        let out = run(&mut s, "network_shape", json!({"preset": "3g", "loss_pct": 0})).expect("3g");
        assert_eq!(out["commands"], json!(["network speed 330:780", "network delay 200"]));
        assert_eq!(run(&mut s, "network_clear", json!({})), Ok(Value::Null));
        assert!(!shaping(&emulator).iter().any(|c| c.starts_with("network loss")), "{:?}", shaping(&emulator));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn connections_are_ios_rows_with_each_sockets_uid() {
        let (mut s, root, _adb, _emulator) = session("conns");
        let rows = run(&mut s, "connections", json!({})).expect("connections");
        let rows = rows.as_array().expect("rows");
        assert_eq!(
            rows[0],
            json!({"pid": 612, "process": "system_server", "protocol": "TCP", "local": "*:8080", "remote": "", "state": "LISTEN", "uid": 1000})
        );
        assert_eq!(
            rows[1],
            json!({"pid": 4321, "process": "com.example.app", "protocol": "TCP", "local": "10.0.2.15:41394", "remote": "216.58.46.46:443",
                "state": "ESTABLISHED", "uid": 10086})
        );
        assert_eq!(rows[2]["local"], "[::ffff:10.0.2.15]:45056");
        let udp = rows.iter().find(|r| r["protocol"] == "UDP").expect("udp");
        assert_eq!(
            udp,
            &json!({"pid": null, "process": "uid 1021", "protocol": "UDP", "local": "*:5353", "remote": "", "state": "", "uid": 1021})
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn traffic_is_bytes_per_app_uid_busiest_first() {
        let (mut s, root, _adb, _emulator) = session("traffic");
        assert_eq!(
            run(&mut s, "traffic", json!({})),
            Ok(json!([
                {"pid": 4321, "process": "com.example.app", "bytes_in": 123_456, "bytes_out": 23_456, "uid": 10086},
                {"pid": null, "process": "system", "bytes_in": 4096, "bytes_out": 2048, "uid": 1000},
            ]))
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn docs_api_rows_agree_with_the_method_table() {
        let api = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api.md")).expect("docs/api.md");
        for (method, params, doc) in crate::METHODS.iter().filter(|(m, ..)| SHARED.contains(m) || IOS_ONLY.contains(m)) {
            let platforms: Vec<&str> = crate::platforms(method).iter().map(|p| p.label()).collect();
            let row = format!("| `{method}` | {} | {} | {} |", platforms.join(", "), params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.lines().any(|l| l == row), "docs/api.md's row for `{method}` differs from METHODS; it should be\n{row}");
        }
    }
}
