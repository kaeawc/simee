use super::*;

// Captured verbatim on real emu guests (the #442 device check, posted on #443): a phone with no
// companion installed, and an unpaired Wear OS watch (API 37). The texts are exactly what adb
// returned.
const STATUS_PHONE_NO_COMPANION: &str = "Broadcasting: Intent { act=com.google.android.gms.wearable.EMULATOR flg=0x400000 (has extras) }\n\
Broadcast completed: result=0\n";
const STATUS_WATCH_UNPAIRED: &str = "Broadcasting: Intent { act=com.google.android.gms.wearable.EMULATOR flg=0x400000 (has extras) }\n\
Broadcast completed: result=1, data=\"Local:[6973c660]\nPeer:[null,false,true]\n\"\n";
const REFRESH_OUTPUT: &str = "Broadcasting: Intent { act=com.google.android.gms.wearable.EMULATOR flg=0x400000 (has extras) }\n\
Broadcast completed: result=1, data=\"Emulator connection refresh succeeded.\"\n";
/// The phone's `dumpsys package com.google.android.gms | grep versionCode`, and the watch's
/// `dumpsys package com.google.android.wearable.app | grep -E 'versionName|versionCode'`.
const GMS_PHONE: &str = "    versionCode=250834035 minSdk=35 targetSdk=35\n";
const WEAR_APP_WATCH: &str = "    versionCode=790472107 minSdk=33 targetSdk=35\n    versionName=1.31.58.829364773\n";

// provisional: a status with a real peer needs a phone with the companion installed and a Play
// account, which no lane has; replace these with that capture. They are the texts Android
// Studio's pairing assistant parses (its tests' fixtures).
const STATUS_ONE_PEER: &str = "Broadcasting: Intent { act=com.google.android.gms.wearable.EMULATOR flg=0x400000 (has extras) }\n\
Broadcast completed: result=1, data=\"Local:[LocalNodeId]\nPeer:[AnotherNode,false,false]\"";
// provisional: see above.
const STATUS_TWO_PEERS: &str = "Broadcasting: Intent { act=com.google.android.gms.wearable.EMULATOR flg=0x400000 (has extras) }\n\
Broadcast completed: result=1, data=\"Local:[TestNodeId]\nPeer:[TestNodeId,true,true]\nPeer:[AnotherNode,false,false]\"";
// provisional: no `[EMULATOR_PAIRING:*]` line has been captured (the companion needs a Play
// account); this is the format Studio's tests use. Replace it with the device capture.
const LOGCAT_FORMAT: &str = "08-17 11:35:01.797   439  2734 I EmulatorActivity:[EMULATOR_PAIRING:{}]";

fn logcat(state: &str) -> String {
    LOGCAT_FORMAT.replace("{}", state)
}

fn peer(node: Option<&str>, connected: bool, enabled: bool) -> Peer {
    Peer { node: node.map(str::to_owned), connected, enabled }
}

#[test]
fn package_names_follow_androids_grammar() {
    for ok in ["com.example.app", "com.example.app_2", "a.b", PIXEL_COMPANION, LEGACY_COMPANION] {
        assert_eq!(package_name(ok), Ok(ok));
    }
    for bad in [
        "",
        "a",
        "com..app",
        "com.1app",
        "com.example;reboot",
        "$(id).x",
        "com.example app",
        "-n.x",
        "com.exa'mple.x",
        ".a.b",
        "a.b.",
        "com.exa-mple.x",
    ] {
        assert!(package_name(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn node_ids_follow_the_grammar_and_its_length_bounds() {
    for ok in ["a", "LocalNodeId", "8c8d3b1f.x-y_z", &"a".repeat(64)] {
        assert_eq!(node_id(ok), Ok(ok));
    }
    for bad in ["", &"a".repeat(65), "x'; touch /sdcard/PWNED; echo '", "a b", "a\nb", "a$b", "a/b", "nodé"] {
        let err = node_id(bad).expect_err(bad);
        assert!(!err.contains('\n') && err.chars().count() < 200, "{err}");
    }
}

#[test]
fn watch_names_are_text_the_companion_can_show() {
    assert_eq!(emulator_name("Wear_OS_Small_Round"), Ok("Wear_OS_Small_Round"));
    assert_eq!(emulator_name("it's \"mine\""), Ok("it's \"mine\""));
    for bad in ["", "   ", "a\nb", "a\0b", &"x".repeat(129)] {
        assert!(emulator_name(bad).is_err(), "{bad:?}");
    }
    assert!(emulator_name(&"x".repeat(128)).is_ok());
}

#[test]
fn status_with_a_local_node_and_a_peer() {
    let s = parse_pairing_status(STATUS_ONE_PEER);
    assert_eq!(s.local.as_deref(), Some("LocalNodeId"));
    assert_eq!(s.peers, [peer(Some("AnotherNode"), false, false)]);
    assert!(!s.is_connected("AnotherNode"));
}

#[test]
fn a_phone_without_the_companion_has_no_status() {
    // `result=0` and no `data=`: no node, no peers, and not an error.
    assert_eq!(parse_pairing_status(STATUS_PHONE_NO_COMPANION), PairingStatus::default());
}

#[test]
fn an_unpaired_watch_has_its_node_and_a_null_peer() {
    let s = parse_pairing_status(STATUS_WATCH_UNPAIRED);
    assert_eq!(s.local.as_deref(), Some("6973c660"));
    // The slot with no node: enabled, not connected, nobody's.
    assert_eq!(s.peers, [peer(None, false, true)]);
    assert!(!s.is_connected("null") && s.peer("6973c660").is_none());
    assert_eq!(watch_peer_conflict("watch", &s, "any-phone"), None, "an empty slot needs no factory reset");
}

#[test]
fn the_refresh_broadcast_prints_no_status() {
    assert_eq!(parse_pairing_status(REFRESH_OUTPUT), PairingStatus::default());
}

#[test]
fn the_captured_watch_asks_for_the_pixel_companion_through_its_property() {
    // `settings get secure cw.oem_companion_package` printed `null`; the property named the app.
    let c = choose_companion("null\n", || Ok("com.google.android.apps.wear.companion\n".into())).expect("choice");
    assert_eq!((c.package.as_str(), c.source), (PIXEL_COMPANION, CompanionSource::Property));
}

#[test]
fn the_captured_package_versions_parse() {
    let gms = parse_package_version(GMS_PHONE);
    assert_eq!(gms, PackageVersion { code: Some(250_834_035), name: None });
    assert!(Feature::ALL.iter().all(|f| f.in_version(gms.code.expect("code"))), "that Play services has every gate");
    let app = parse_package_version(WEAR_APP_WATCH);
    assert_eq!(app, PackageVersion { code: Some(790_472_107), name: Some("1.31.58.829364773".into()) });
    assert!(companion_has_emulator_activity(LEGACY_COMPANION, app.code));
    // The phone had neither companion: `dumpsys` printed nothing.
    assert!(!parse_package_version("").installed());
}

#[test]
fn status_with_two_peers_finds_the_connected_one_ignoring_case() {
    let s = parse_pairing_status(STATUS_TWO_PEERS);
    assert_eq!(s.local.as_deref(), Some("TestNodeId"));
    assert_eq!(s.peers.len(), 2);
    assert!(s.is_connected("testnodeid"));
    assert!(!s.is_connected("AnotherNode"));
    assert!(s.peer("nobody").is_none());
}

#[test]
fn status_peers_may_have_null_nodes() {
    let s = parse_pairing_status("Local:[me]\nPeer:[null,false,false]\nPeer:[NULL,true,false]\nPeer:[w1,true,true]");
    assert_eq!(s.peers, [peer(None, false, false), peer(None, true, false), peer(Some("w1"), true, true)]);
    // A null node is nobody's: asking for "null" finds no peer.
    assert!(s.peer("null").is_none());
}

#[test]
fn status_enabled_and_connected_are_independent() {
    let s = parse_pairing_status("Local:[me]\nPeer:[a,true,false]\nPeer:[b,false,true]");
    assert!(!s.is_connected("a") && !s.is_connected("b"));
    assert_eq!(s.peers, [peer(Some("a"), true, false), peer(Some("b"), false, true)]);
}

#[test]
fn status_with_nothing_to_read_is_empty() {
    for text in ["", "\n\n", "Broadcast completed: result=0", "Error: Activity not started", "Local:[", "Local:[]"] {
        assert_eq!(parse_pairing_status(text), PairingStatus::default(), "{text:?}");
    }
}

#[test]
fn status_leaves_out_what_is_not_a_node_or_a_flag() {
    let s = parse_pairing_status(
        "Local:[x'; touch /sdcard/PWNED; echo ']\nPeer:[a,maybe,true]\nPeer:[a,true]\nPeer:[a,true,true,true]\nPeer:[,true,true]\nPeer:[bad node,true,true]\nPeer:[ok,true,true]",
    );
    assert_eq!(s.local, None, "a malformed local node is no node");
    assert_eq!(s.peers, [peer(Some("ok"), true, true)]);
}

#[test]
fn status_takes_the_first_local_node_and_reads_lines_anywhere() {
    let s = parse_pairing_status("noise\n  Local:[first]\nLocal:[second]\nPeer:[p,true,true]\"\nmore noise");
    assert_eq!(s.local.as_deref(), Some("first"));
    assert_eq!(s.peers, [peer(Some("p"), true, true)]);
    // CRLF output (an adb that translates newlines) reads the same.
    assert_eq!(parse_pairing_status("Local:[a]\r\nPeer:[b,true,true]\r\n"), parse_pairing_status("Local:[a]\nPeer:[b,true,true]\n"));
}

#[test]
fn every_logcat_state_parses() {
    for (name, want) in [
        ("STARTED", PairingProgress::Started),
        ("CONSENT", PairingProgress::Consent),
        ("PAIRING", PairingProgress::Pairing),
        ("SUCCESS", PairingProgress::Success),
        ("FAILURE", PairingProgress::Failure),
        ("CANCELLED", PairingProgress::Cancelled),
    ] {
        let got = parse_progress_line(&logcat(name)).expect(name);
        assert_eq!(got, want);
        assert_eq!(got.as_str(), name);
    }
}

#[test]
fn unknown_logcat_states_are_unknown_not_errors() {
    let got = parse_progress_line(&logcat("RETRYING")).expect("a state");
    assert_eq!(got, PairingProgress::Unknown("RETRYING".into()));
    assert_eq!(got.as_str(), "RETRYING");
    assert!(!got.finished());
    // Case counts: the companion logs these names in capitals.
    assert_eq!(parse_progress_line("[EMULATOR_PAIRING:success]"), Some(PairingProgress::Unknown("success".into())));
    // A guest's long text is clipped, not echoed whole.
    let long = format!("[EMULATOR_PAIRING:{}]", "X".repeat(500));
    assert!(parse_progress_line(&long).is_some_and(|p| p.as_str().chars().count() <= 65));
}

#[test]
fn lines_without_a_marker_report_nothing() {
    for line in [
        "",
        "08-17 11:35:01.797   439  2734 I ActivityManager: Start proc",
        "[EMULATOR_PAIRING:",
        "[EMULATOR_PAIRING:]",
        "EMULATOR_PAIRING:SUCCESS",
        "[EMULATOR_PAIRING SUCCESS]",
    ] {
        assert_eq!(parse_progress_line(line), None, "{line:?}");
    }
    // The marker is found in any logcat format.
    assert_eq!(parse_progress_line("I/EmulatorActivity( 439): [EMULATOR_PAIRING:PAIRING]"), Some(PairingProgress::Pairing));
}

#[test]
fn only_success_failure_and_cancelled_finish() {
    let finished: Vec<bool> = ["STARTED", "CONSENT", "PAIRING", "SUCCESS", "FAILURE", "CANCELLED", "OTHER"]
        .into_iter()
        .map(|n| PairingProgress::parse(n).finished())
        .collect();
    assert_eq!(finished, [false, false, false, true, true, true, false]);
}

fn no_property() -> Result<String, String> {
    panic!("the property is read only when the setting didn't decide")
}

#[test]
fn the_setting_decides_the_companion_first() {
    let c = choose_companion("com.vendor.watch\n", no_property).expect("choice");
    assert_eq!((c.package.as_str(), c.source), ("com.vendor.watch", CompanionSource::Setting));
    assert!(c.ignored.is_empty());
}

#[test]
fn the_property_decides_when_the_setting_has_no_value() {
    for setting in ["null", "null\n", "", "  "] {
        let c = choose_companion(setting, || Ok("com.vendor.prop\n".into())).expect("choice");
        assert_eq!((c.package.as_str(), c.source), ("com.vendor.prop", CompanionSource::Property), "{setting:?}");
    }
}

#[test]
fn the_legacy_companion_is_the_fallback() {
    for property in ["", "null", "\n"] {
        let c = choose_companion("null", || Ok(property.into())).expect("choice");
        assert_eq!((c.package.as_str(), c.source), (LEGACY_COMPANION, CompanionSource::Fallback));
        assert!(c.ignored.is_empty());
    }
}

#[test]
fn a_malformed_value_is_ignored_and_reported() {
    let c = choose_companion("x; reboot", || Ok("$(id)".into())).expect("choice");
    assert_eq!((c.package.as_str(), c.source), (LEGACY_COMPANION, CompanionSource::Fallback));
    assert_eq!(c.ignored, ["cw.oem_companion_package `x; reboot`", "ro.oem.companion_package `$(id)`"]);
    let c = choose_companion("bad value", || Ok(PIXEL_COMPANION.into())).expect("choice");
    assert_eq!((c.package.as_str(), c.source), (PIXEL_COMPANION, CompanionSource::Property));
    assert_eq!(c.ignored.len(), 1);
}

#[test]
fn an_error_reading_the_property_is_the_choices() {
    assert_eq!(choose_companion("null", || Err("adb died".into())), Err("adb died".into()));
}

#[test]
fn abis_split_on_commas() {
    assert_eq!(parse_abis("arm64-v8a,armeabi-v7a,armeabi\n"), ["arm64-v8a", "armeabi-v7a", "armeabi"]);
    assert_eq!(parse_abis("x86_64"), ["x86_64"]);
    assert!(parse_abis("").is_empty() && parse_abis(" \n").is_empty());
}

#[test]
fn package_versions_read_the_first_of_each() {
    let v = parse_package_version("    versionCode=773385010 minSdk=28 targetSdk=34\n    versionName=1.2.3\n    versionCode=1 minSdk=1\n");
    assert_eq!(v, PackageVersion { code: Some(773_385_010), name: Some("1.2.3".into()) });
    assert!(v.installed());
    // Studio's own fixture: only a code.
    let v = parse_package_version("    versionCode=1 minSdk=23 targetSdk=30 local: localNodeId");
    assert_eq!(v, PackageVersion { code: Some(1), name: None });
    assert!(v.installed());
}

#[test]
fn a_package_that_dumpsys_does_not_know_is_not_installed() {
    for text in ["", "Unable to find package: com.x.y", "versionCode=notanumber"] {
        let v = parse_package_version(text);
        assert_eq!(v.code, None, "{text:?}");
    }
    assert!(!parse_package_version("").installed());
    assert!(!parse_package_version("Unable to find package: com.x.y").installed());
    // A version code over u32 isn't a version this knows.
    assert_eq!(parse_package_version("versionCode=99999999999").code, None);
}

#[test]
fn each_gate_opens_at_its_version_and_not_before() {
    for f in Feature::ALL {
        let min = f.min_version();
        assert!(!f.in_version(min - 1), "{f:?} one below");
        assert!(f.in_version(min), "{f:?} at");
        assert!(f.in_version(min + 1), "{f:?} one above");
        assert!(!f.in_version(0), "{f:?} at none");
        assert!(f.in_version(u32::MAX));
    }
    assert_eq!(
        Feature::ALL.map(Feature::min_version),
        [210_915_000, 213_204_000, 214_412_000, 213_013_000],
        "reverse forwarding, multi-watch, status, refresh"
    );
}

#[test]
fn the_legacy_companion_needs_a_version_for_its_emulator_activity() {
    let at = LEGACY_EMULATOR_ACTIVITY_VERSION;
    assert!(!companion_has_emulator_activity(LEGACY_COMPANION, Some(at - 1)));
    assert!(companion_has_emulator_activity(LEGACY_COMPANION, Some(at)));
    assert!(companion_has_emulator_activity(LEGACY_COMPANION, Some(at + 1)));
    assert!(!companion_has_emulator_activity(LEGACY_COMPANION, None));
    assert!(companion_has_emulator_activity(PIXEL_COMPANION, None));
    assert!(companion_has_emulator_activity("com.vendor.watch", Some(1)));
}

fn member<'a>(name: &'a str, form_factor: FormFactor, api: u32, play_store: bool, gms: Option<u32>) -> Member<'a> {
    Member { name, form_factor, api, play_store, gms }
}

fn good_phone() -> Member<'static> {
    member("phone", FormFactor::Phone, 36, true, Some(Feature::PairingStatus.min_version()))
}

fn good_watch() -> Member<'static> {
    member("watch", FormFactor::Watch, 34, false, Some(Feature::PairingStatus.min_version()))
}

fn problems(phone: &Member<'_>, watch: &Member<'_>) -> Vec<String> {
    preflight(phone, watch).iter().map(Problem::to_string).collect()
}

#[test]
fn a_phone_with_play_and_a_watch_pass() {
    assert!(preflight(&good_phone(), &good_watch()).is_empty());
    for ff in [FormFactor::Tablet, FormFactor::Foldable] {
        assert!(preflight(&Member { form_factor: ff, ..good_phone() }, &good_watch()).is_empty(), "{ff:?}");
    }
}

#[test]
fn the_phone_api_gate_is_30() {
    assert!(preflight(&Member { api: 30, ..good_phone() }, &good_watch()).is_empty());
    let p = problems(&Member { api: 29, ..good_phone() }, &good_watch());
    assert_eq!(p.len(), 1);
    assert!(p[0].contains("API 29") && p[0].contains("API 30") && p[0].contains("google_apis_playstore"), "{p:?}");
}

#[test]
fn the_watch_api_gate_is_28() {
    assert!(preflight(&good_phone(), &Member { api: 28, ..good_watch() }).is_empty());
    let p = problems(&good_phone(), &Member { api: 27, ..good_watch() });
    assert_eq!(p.len(), 1);
    assert!(p[0].contains("API 27") && p[0].contains("API 28") && p[0].contains("Wear OS image"), "{p:?}");
}

#[test]
fn a_phone_without_play_names_the_image_to_use() {
    let p = problems(&Member { play_store: false, ..good_phone() }, &good_watch());
    assert_eq!(p.len(), 1);
    assert!(p[0].contains("no Google Play") && p[0].contains("-tag google_apis_playstore"), "{p:?}");
}

#[test]
fn the_watch_needs_no_play_store() {
    assert!(preflight(&good_phone(), &Member { play_store: false, ..good_watch() }).is_empty());
}

#[test]
fn form_factors_are_checked_for_each_side() {
    for ff in [FormFactor::Watch, FormFactor::Tv, FormFactor::Auto, FormFactor::Vision] {
        let p = problems(&Member { form_factor: ff, ..good_phone() }, &good_watch());
        assert_eq!(p.len(), 1, "{ff:?}: {p:?}");
        assert!(p[0].contains(ff.as_str()) && p[0].contains("not a phone"), "{p:?}");
    }
    for ff in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Tv, FormFactor::Auto] {
        let p = problems(&good_phone(), &Member { form_factor: ff, ..good_watch() });
        assert_eq!(p.len(), 1, "{ff:?}: {p:?}");
        assert!(p[0].contains("not a watch") && p[0].contains("android-wear"), "{p:?}");
    }
}

#[test]
fn a_wrong_form_factor_is_not_also_blamed_for_its_api() {
    // An API 20 TV: one problem (it isn't a phone), not three.
    let tv = member("tv", FormFactor::Tv, 20, false, None);
    assert_eq!(preflight(&tv, &good_watch()).len(), 1);
}

#[test]
fn the_phones_play_services_gates_sit_at_their_versions() {
    // Multi-watch (213204000) and the status (214412000): the phone needs both.
    let at = |v| preflight(&Member { gms: Some(v), ..good_phone() }, &good_watch());
    let (multi, status) = (Feature::MultiWatch.min_version(), Feature::PairingStatus.min_version());
    assert!(at(status).is_empty() && at(status + 1).is_empty());
    let p: Vec<String> = at(status - 1).iter().map(Problem::to_string).collect();
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains(&(status - 1).to_string()) && p[0].contains(&status.to_string()) && p[0].contains("the pairing status"), "{p:?}");
    assert!(p[0].contains("update Google Play services on phone"), "{p:?}");
    // Between the two it lacks only the status; below both, each is named with its version.
    assert_eq!(at(multi).len(), 1);
    let p: Vec<String> = at(multi - 1).iter().map(Problem::to_string).collect();
    assert_eq!(p.len(), 2, "{p:?}");
    assert!(p[0].contains(&multi.to_string()) && p[0].contains("several watches"), "{p:?}");
    assert!(p[1].contains(&status.to_string()) && p[1].contains("the pairing status"), "{p:?}");
}

#[test]
fn the_watchs_play_services_gates_sit_at_their_versions() {
    // Reverse forwarding (210915000), refresh (213013000) and the status (214412000): the watch
    // needs all three, so one between the first and the last fails the handshake later.
    let at = |v| preflight(&good_phone(), &Member { gms: Some(v), ..good_watch() });
    let (reverse, refresh, status) =
        (Feature::ReverseForward.min_version(), Feature::RefreshConnection.min_version(), Feature::PairingStatus.min_version());
    assert!(at(status).is_empty() && at(status + 1).is_empty());
    let named = |v: u32| -> Vec<String> { at(v).iter().map(Problem::to_string).collect() };
    let p = named(status - 1);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains("the pairing status") && p[0].contains(&status.to_string()), "{p:?}");
    let p = named(refresh);
    assert_eq!(p.len(), 1, "at refresh it still lacks the status: {p:?}");
    let p = named(refresh - 1);
    assert_eq!(p.len(), 2, "{p:?}");
    assert!(p[0].contains("refreshing the emulator connection") && p[0].contains(&refresh.to_string()), "{p:?}");
    assert!(p[1].contains("the pairing status"), "{p:?}");
    let p = named(reverse - 1);
    assert_eq!(p.len(), 3, "{p:?}");
    assert!(p[0].contains("reverse port forwarding") && p[0].contains(&reverse.to_string()), "{p:?}");
    assert!(p.iter().all(|m| m.contains("update Google Play services on watch")), "{p:?}");
    assert_eq!(named(reverse).len(), 2);
}

#[test]
fn unreadable_play_services_asks_to_wait_or_update() {
    let p = problems(&Member { gms: None, ..good_phone() }, &Member { gms: None, ..good_watch() });
    assert_eq!(p.len(), 2, "one for each side, not one for each feature");
    assert!(p.iter().all(|m| m.contains("isn't installed or couldn't be read")), "{p:?}");
}

#[test]
fn every_failure_is_listed_with_a_fix() {
    let bad_phone = member("p", FormFactor::Phone, 29, false, Some(1));
    let bad_watch = member("w", FormFactor::Watch, 27, false, Some(1));
    let all = preflight(&bad_phone, &bad_watch);
    // The phone: API, Play Store and two gates; the watch: API and three gates.
    assert_eq!(all.len(), 4 + 4, "{all:?}");
    assert!(all.iter().all(|p| !p.problem.is_empty() && !p.fix.is_empty()));
}

fn installed(code: u32) -> PackageVersion {
    PackageVersion { code: Some(code), name: Some("1.0".into()) }
}

#[test]
fn an_installed_companion_passes() {
    assert_eq!(companion_check("phone", PIXEL_COMPANION, &installed(1), &[]), Ok(()));
    assert_eq!(companion_check("phone", LEGACY_COMPANION, &installed(LEGACY_EMULATOR_ACTIVITY_VERSION), &[]), Ok(()));
    // An installed companion needs no ABI check, and an unknown id that is installed works.
    assert_eq!(companion_check("phone", "com.vendor.watch", &installed(1), &["x86_64".into()]), Ok(()));
}

#[test]
fn an_old_legacy_companion_says_to_pair_by_hand() {
    let err = companion_check("phone", LEGACY_COMPANION, &installed(LEGACY_EMULATOR_ACTIVITY_VERSION - 1), &[]).expect_err("old");
    assert_eq!(err, no_emulator_activity("phone", LEGACY_COMPANION));
    assert!(err.to_string().contains(".EmulatorActivity") && err.to_string().contains("by hand"), "{err}");
}

#[test]
fn a_missing_companion_says_to_install_it_from_play() {
    for package in [PIXEL_COMPANION, LEGACY_COMPANION] {
        let err = companion_check("phone", package, &PackageVersion::default(), &["arm64-v8a".into()]).expect_err("missing");
        assert!(err.problem.contains(package) && err.problem.contains("isn't installed"), "{err}");
        assert!(err.fix.contains("Play Store") && err.fix.contains("phone"), "{err}");
    }
}

#[test]
fn the_legacy_companion_has_no_x86_64_build() {
    let missing = PackageVersion::default();
    let err = companion_check("phone", LEGACY_COMPANION, &missing, &["x86_64".into()]).expect_err("abi");
    assert!(err.problem.contains("no x86_64 build") && err.fix.contains("arm64"), "{err}");
    // Not when the phone has another ABI too, and not for the Pixel companion.
    for abis in [vec!["x86_64".to_owned(), "arm64-v8a".to_owned()], vec![]] {
        let err = companion_check("phone", LEGACY_COMPANION, &missing, &abis).expect_err("missing");
        assert!(err.problem.contains("isn't installed") && !err.problem.contains("x86_64"), "{err}");
    }
    let err = companion_check("phone", PIXEL_COMPANION, &missing, &["x86_64".into()]).expect_err("missing");
    assert!(err.problem.contains("isn't installed"), "{err}");
}

#[test]
fn an_unknown_companion_that_is_missing_is_not_one_to_find() {
    let err = companion_check("phone", "com.vendor.watch", &PackageVersion::default(), &[]).expect_err("unknown");
    assert!(err.problem.contains("com.vendor.watch") && err.problem.contains("isn't one simee knows"), "{err}");
}

#[test]
fn a_watch_with_another_phones_peer_needs_a_factory_reset() {
    let s = parse_pairing_status("Local:[w]\nPeer:[OtherPhone,true,true]");
    let p = watch_peer_conflict("watch", &s, "ThisPhone").expect("conflict");
    assert!(p.problem.contains("OtherPhone") && p.fix.contains("factory reset"), "{p}");
    assert_eq!(watch_peer_conflict("watch", &s, "otherphone"), None, "the phone itself, ignoring case");
    assert_eq!(watch_peer_conflict("watch", &parse_pairing_status("Local:[w]"), "p"), None);
    // A slot with no node is no other phone.
    assert_eq!(watch_peer_conflict("watch", &parse_pairing_status("Local:[w]\nPeer:[null,false,false]"), "p"), None);
}

struct Pair {
    status: Option<PairingStatus>,
    progress: Option<PairingProgress>,
    phone_running: bool,
    watch_running: bool,
    forward: bool,
    reverse: bool,
    companion_installed: Option<bool>,
    watch_node: Option<&'static str>,
}

impl Pair {
    /// A pair that is up: both running, bridged, companion installed, the watch connected.
    fn up() -> Pair {
        Pair {
            status: Some(parse_pairing_status("Local:[phone]\nPeer:[w1,true,true]")),
            progress: None,
            phone_running: true,
            watch_running: true,
            forward: true,
            reverse: true,
            companion_installed: Some(true),
            watch_node: Some("w1"),
        }
    }

    fn state(&self) -> PairState {
        pair_state(&Observed {
            phone: "phone",
            watch: "watch",
            phone_running: self.phone_running,
            watch_running: self.watch_running,
            forward: self.forward,
            reverse: self.reverse,
            companion_installed: self.companion_installed,
            watch_node: self.watch_node,
            status: self.status.as_ref(),
            progress: self.progress.as_ref(),
        })
    }
}

fn parsed(text: &str) -> Option<PairingStatus> {
    Some(parse_pairing_status(text))
}

fn assert_state(p: &Pair, state: LinkState, reason: Option<&str>, needs: Option<&str>) {
    let got = p.state();
    assert_eq!(got.state, state, "{got:?}");
    match reason {
        Some(r) => assert!(got.reason.as_deref().is_some_and(|g| g.contains(r)), "reason {:?} lacks {r:?}", got.reason),
        None => assert_eq!(got.reason, None),
    }
    assert_eq!(got.needs.as_deref(), needs, "{got:?}");
}

#[test]
fn a_connected_enabled_peer_is_active() {
    assert_state(&Pair::up(), LinkState::Active, None, None);
}

#[test]
fn active_beats_a_stale_progress() {
    // After a reboot a handshake from before may be remembered as CONSENT: the status wins.
    let p = Pair { progress: Some(PairingProgress::Consent), ..Pair::up() };
    assert_state(&p, LinkState::Active, None, None);
}

#[test]
fn a_member_that_is_not_running_breaks_the_pair() {
    assert_state(&Pair { phone_running: false, ..Pair::up() }, LinkState::Broken, Some("phone isn't running"), Some("start phone"));
    assert_state(&Pair { watch_running: false, ..Pair::up() }, LinkState::Broken, Some("watch isn't running"), Some("start watch"));
    let both = Pair { phone_running: false, watch_running: false, ..Pair::up() };
    assert_state(&both, LinkState::Broken, Some("phone and watch aren't running"), Some("start phone"));
}

#[test]
fn a_missing_forward_or_reverse_is_a_down_bridge() {
    assert_state(
        &Pair { forward: false, ..Pair::up() },
        LinkState::Broken,
        Some("bridge down: the forward on the phone"),
        Some("rebuild the bridge"),
    );
    assert_state(
        &Pair { reverse: false, ..Pair::up() },
        LinkState::Broken,
        Some("bridge down: the reverse on the watch"),
        Some("rebuild the bridge"),
    );
    assert_state(
        &Pair { forward: false, reverse: false, ..Pair::up() },
        LinkState::Broken,
        Some("bridge down"),
        Some("rebuild the bridge"),
    );
}

#[test]
fn a_stopped_member_outranks_a_down_bridge() {
    let p = Pair { phone_running: false, forward: false, ..Pair::up() };
    assert_state(&p, LinkState::Broken, Some("phone isn't running"), Some("start phone"));
}

#[test]
fn a_missing_companion_is_pending_and_needs_it_on_the_phone() {
    let p = Pair { companion_installed: Some(false), status: parsed("Local:[phone]"), ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("companion app isn't installed"), Some("companion app on phone"));
    // Not checked is not missing.
    let p = Pair { companion_installed: None, ..Pair::up() };
    assert_state(&p, LinkState::Active, None, None);
}

#[test]
fn consent_is_pending_and_needs_a_person_on_the_phone() {
    let p = Pair { progress: Some(PairingProgress::Consent), status: parsed("Local:[phone]"), ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("waiting for consent"), Some("consent on phone"));
}

#[test]
fn started_and_pairing_are_pending() {
    for progress in [PairingProgress::Started, PairingProgress::Pairing] {
        let p = Pair { progress: Some(progress), status: parsed("Local:[phone]"), ..Pair::up() };
        assert_state(&p, LinkState::Pending, Some("pairing in progress"), None);
    }
}

#[test]
fn failure_and_cancelled_are_broken_and_say_to_pair_again() {
    for (progress, reason) in [(PairingProgress::Failure, "failure"), (PairingProgress::Cancelled, "cancelled")] {
        let p = Pair { progress: Some(progress), status: parsed("Local:[phone]"), ..Pair::up() };
        assert_state(&p, LinkState::Broken, Some(reason), Some("pair again"));
    }
}

#[test]
fn success_with_a_peer_not_yet_connected_is_pending() {
    let p = Pair { progress: Some(PairingProgress::Success), status: parsed("Local:[phone]\nPeer:[w1,false,true]"), ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("waiting for the watch to connect"), None);
}

#[test]
fn a_listed_peer_that_is_not_connected_with_no_handshake_is_disconnected() {
    for text in ["Local:[phone]\nPeer:[w1,false,true]", "Local:[phone]\nPeer:[w1,true,false]", "Local:[phone]\nPeer:[w1,false,false]"] {
        let p = Pair { status: parsed(text), ..Pair::up() };
        assert_state(&p, LinkState::Broken, Some("disconnected"), Some("pair again if it stays down"));
    }
}

#[test]
fn a_watch_the_phone_does_not_list_is_pending() {
    let p = Pair { status: parsed("Local:[phone]\nPeer:[someone-else,true,true]"), ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("doesn't list the watch"), None);
    // Without the watch's node there is nothing to look for.
    let p = Pair { watch_node: None, ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("doesn't list the watch"), None);
}

#[test]
fn an_unreadable_status_is_pending_not_broken() {
    let p = Pair { status: None, ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("can't be read yet"), None);
}

#[test]
fn success_without_any_status_is_still_pending() {
    let p = Pair { progress: Some(PairingProgress::Success), status: None, ..Pair::up() };
    assert_state(&p, LinkState::Pending, Some("can't be read yet"), None);
}

#[test]
fn problems_read_as_problem_then_fix() {
    let p = Problem { problem: "a".into(), fix: "b".into() };
    assert_eq!(p.to_string(), "a: b");
}
