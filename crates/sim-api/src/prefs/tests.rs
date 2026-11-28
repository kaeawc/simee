use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_core::platform::Platform;

use super::fake::{Fake, unique};
use super::*;
use crate::methods::{self, METHODS, call as api};

/// A session with one Android device, `pixel` (an AVD, not running: the fake backend needs none),
/// whose preferences are `fake`'s; `app` is unique to the test (feeds are per process).
fn session(tag: &str) -> (methods::Session, Fake, String, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("sim-api-prefs-{}", unique(tag)));
    let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
    std::fs::create_dir_all(&image).expect("image dir");
    std::fs::write(image.join("system.img"), b"s").expect("system.img");
    std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
    let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
    let mut android = sim_android::Android::new(env);
    android.running_dir = Some(root.join("running"));
    android.create("pixel", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
    let app = unique("com.example.app");
    let fake = Fake::default()
        .with_store(
            Store::new(StoreKind::SharedPreferences, &app, &format!("{app}_preferences")),
            [("name".to_owned(), PrefValue::String("Ada".into())), ("launches".to_owned(), PrefValue::Int(3))].into(),
        )
        .with_store(
            Store::new(StoreKind::DataStore, &app, "settings"),
            [("tags".to_owned(), PrefValue::StringSet(["a".to_owned()].into()))].into(),
        );
    let mut s = methods::Session::with_android(android);
    s.prefs.android = Box::new(fake.clone());
    (s, fake, app, root)
}

fn ok(s: &mut methods::Session, method: &str, p: Value) -> Value {
    api(s, method, &p).unwrap_or_else(|e| panic!("{method} {p}: {e}"))
}

/// `base` with `extra`'s fields added.
fn with(base: Value, extra: Value) -> Value {
    let mut p = base;
    if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
        p.extend(extra.clone());
    }
    p
}

/// Follows `prefs_watch` from `cursor` until `n` changes have come (or 5 s pass), as a client
/// does: the feed's thread takes them as the backend sends them, so one call may see fewer.
fn follow(s: &mut methods::Session, params: &Value, cursor: &str, n: usize) -> (Vec<Value>, Value) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    let mut last = json!({"cursor": cursor});
    while events.len() < n && Instant::now() < deadline && last.get("ended").is_none() {
        last = ok(s, "prefs_watch", with(params.clone(), json!({"cursor": last["cursor"], "wait_ms": 1000})));
        events.extend(last["events"].as_array().cloned().unwrap_or_default());
    }
    (events, last)
}

#[test]
fn help_lists_every_prefs_method_on_both_platforms() {
    let help = ok(&mut methods::Session::default(), "help", Value::Null);
    for m in super::METHODS {
        let row = help.as_array().and_then(|a| a.iter().find(|x| x["method"] == *m)).unwrap_or_else(|| panic!("{m} in help"));
        assert_eq!(row["platforms"], json!(["android", "ios"]), "{m}");
        assert!(METHODS.iter().any(|(name, ..)| name == m));
    }
}

#[test]
fn stores_have_ids_the_other_methods_take() {
    let parse = |platform, id: &str| Store::parse(platform, "com.example", id);
    let shared = parse(Platform::Android, "shared_prefs/com.example_preferences.xml").expect("shared");
    assert_eq!((shared.kind, shared.name.as_str()), (StoreKind::SharedPreferences, "com.example_preferences"));
    assert_eq!(shared.id(), "shared_prefs/com.example_preferences.xml");
    let ds = parse(Platform::Android, "datastore/settings.preferences_pb").expect("datastore");
    assert_eq!((ds.kind, ds.id()), (StoreKind::DataStore, "datastore/settings.preferences_pb".to_owned()));
    for bad in ["settings", "shared_prefs/.xml", "shared_prefs/a/b.xml", "shared_prefs/../x.xml", "datastore/x.pb", "com.example"] {
        let err = parse(Platform::Android, bad).expect_err(bad);
        assert!(err.starts_with(&format!("`{bad}` isn't an Android preferences store")), "{err}");
    }
    assert_eq!(parse(Platform::Ios, "com.example").expect("app").kind, StoreKind::AppDomain);
    assert_eq!(parse(Platform::Ios, "group.com.example.shared").expect("group").kind, StoreKind::AppGroup);
    assert_eq!(parse(Platform::Ios, "com.apple.Preferences").expect("named").kind, StoreKind::Domain);
    assert!(parse(Platform::Ios, "../etc").is_err() && parse(Platform::Ios, "").is_err());
    assert_eq!(
        shared.to_json(),
        json!({"id": "shared_prefs/com.example_preferences.xml", "kind": "shared_preferences", "name": "com.example_preferences",
            "bundle_id": "com.example", "platform": "android"})
    );
}

#[test]
fn a_store_refuses_types_it_cannot_hold() {
    let shared = StoreKind::SharedPreferences;
    assert!(shared.check(&PrefValue::Float(1.0), "value").is_ok());
    assert_eq!(
        shared.check(&PrefValue::Double(1.0), "value").expect_err("double"),
        "`value` is a `double`, which Android SharedPreferences can't hold (it holds string, int, long, float, bool, string_set)"
    );
    assert!(shared.check(&PrefValue::Data(vec![1]), "value").is_err());
    assert!(StoreKind::DataStore.check(&PrefValue::Double(1.0), "value").is_ok());
    // DataStore 1.1's `bytes` (byteArrayPreferencesKey).
    assert!(StoreKind::DataStore.check(&PrefValue::Data(vec![1, 2]), "value").is_ok());
    assert!(StoreKind::DataStore.check(&PrefValue::Date(Date::from_unix(0, 0)), "value").is_err());
    assert!(StoreKind::AppDomain.check(&PrefValue::StringSet(Default::default()), "value").is_err());
    // Nested values are checked too: an iOS array can't hold an Android string set.
    let nested = PrefValue::Array(vec![PrefValue::Int(1), PrefValue::StringSet(Default::default())]);
    assert!(StoreKind::AppGroup.check(&nested, "value").expect_err("nested").contains("`string_set`"));
    assert!(StoreKind::Domain.check(&PrefValue::Array(vec![PrefValue::Data(vec![1])]), "value").is_ok());
    // No store holds an unsupported value: it's only read.
    let unsupported = PrefValue::Unsupported { native: "null".into(), raw: String::new() };
    assert!(StoreKind::Domain.check(&unsupported, "value").is_err() && StoreKind::SharedPreferences.check(&unsupported, "value").is_err());
}

#[test]
fn stores_and_reads_come_back_typed() {
    let (mut s, _fake, app, root) = session("read");
    let stores = ok(&mut s, "prefs_stores", json!({"device": "pixel", "bundle_id": app}));
    let ids: Vec<&str> = stores.as_array().expect("array").iter().filter_map(|st| st["id"].as_str()).collect();
    let shared_id = format!("shared_prefs/{app}_preferences.xml");
    assert_eq!(ids, [shared_id.as_str(), "datastore/settings.preferences_pb"]);

    let all = ok(&mut s, "prefs_read", json!({"device": "pixel", "bundle_id": app}));
    assert_eq!(all.as_array().map(Vec::len), Some(2));
    assert_eq!(
        all[0]["values"],
        json!({"launches": {"type": "int", "value": 3}, "name": {"type": "string", "value": "Ada"}}),
        "every key with its type"
    );
    assert_eq!(all[1]["values"]["tags"], json!({"type": "string_set", "value": ["a"]}));
    let one = ok(&mut s, "prefs_read", json!({"device": "pixel", "bundle_id": app, "store": "datastore/settings.preferences_pb"}));
    assert_eq!((one.as_array().map(Vec::len), one[0]["store"]["kind"].clone()), (Some(1), json!("datastore")));
    let err = api(&mut s, "prefs_read", &json!({"device": "pixel", "bundle_id": app, "store": 3})).expect_err("not a string");
    assert_eq!(err, "`store` must be a string, not 3");
    assert_eq!(api(&mut s, "prefs_read", &json!({"device": "pixel"})).expect_err("no app"), "missing string parameter `bundle_id`");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn one_unreadable_store_or_unsupported_value_hides_nothing_else() {
    let (mut s, fake, app, root) = session("partial");
    let shared = Store::new(StoreKind::SharedPreferences, &app, &format!("{app}_preferences"));
    // Elements SharedPreferences' own API never writes, as another tool might leave them.
    let odd = Store::new(StoreKind::SharedPreferences, &app, "odd");
    let fake = fake.with_store(
        odd.clone(),
        [
            ("ratio".to_owned(), PrefValue::Unsupported { native: "double".into(), raw: "1.5".into() }),
            ("nothing".to_owned(), PrefValue::Unsupported { native: "null".into(), raw: String::new() }),
            ("fine".to_owned(), PrefValue::Bool(true)),
        ]
        .into(),
    );
    fake.unreadable(&shared);
    let all = ok(&mut s, "prefs_read", json!({"device": "pixel", "bundle_id": app}));
    let rows = all.as_array().expect("array");
    assert_eq!(rows.len(), 3, "every store answers");
    assert_eq!(rows[0]["error"], format!("the fake can't read `shared_prefs/{app}_preferences.xml`"));
    assert_eq!(rows[0].get("values"), None);
    assert_eq!(rows[1]["values"]["tags"]["type"], "string_set", "the next store still reads");
    assert_eq!(rows[2]["values"]["ratio"], json!({"type": "unsupported", "value": {"native": "double", "raw": "1.5"}}));
    assert_eq!(rows[2]["values"]["nothing"], json!({"type": "unsupported", "value": {"native": "null", "raw": ""}}));
    assert_eq!(rows[2]["values"]["fine"], json!({"type": "bool", "value": true}));
    // Naming the unreadable store fails the call, with the same error.
    let err = api(&mut s, "prefs_read", &json!({"device": "pixel", "bundle_id": app, "store": shared.id()})).expect_err("one");
    assert!(err.starts_with("the fake can't read"), "{err}");
    // An unsupported value is never written back.
    let p = json!({"device": "pixel", "bundle_id": app, "store": odd.id(), "key": "ratio", "type": "unsupported", "value": {"native": "double", "raw": "2"}});
    assert!(api(&mut s, "prefs_write", &p).expect_err("read-only").starts_with("`unsupported` values are read-only"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn writes_are_typed_and_checked_before_the_backend_sees_them() {
    let (mut s, fake, app, root) = session("write");
    let store_id = format!("shared_prefs/{app}_preferences.xml");
    let store = Store::parse(Platform::Android, &app, &store_id).expect("store");
    let write = |s: &mut methods::Session, key: &str, ty: &str, value: Value| {
        api(s, "prefs_write", &json!({"device": "pixel", "bundle_id": app, "store": store_id, "key": key, "type": ty, "value": value}))
    };
    assert_eq!(
        write(&mut s, "launches", "long", json!(4_000_000_000_i64)),
        Ok(json!({"store": store_id, "key": "launches", "visible": "now", "app": "untouched"}))
    );
    assert_eq!(fake.entries(&store).expect("store")["launches"], PrefValue::Long(4_000_000_000));
    write(&mut s, "ratio", "float", json!(0.25)).expect("float");
    write(&mut s, "seen", "string_set", json!(["b", "a"])).expect("set");

    let before = fake.entries(&store);
    let refused = [
        ("int", json!(1.5), "`value` must be a whole number"),
        ("int", json!("3"), "`value` must be a whole number"),
        ("bool", json!(1), "`value` must be true or false"),
        ("double", json!(1.5), "which Android SharedPreferences can't hold"),
        ("date", json!("2026-11-27T10:00:00Z"), "which Android SharedPreferences can't hold"),
        ("data", json!("AAEC"), "which Android SharedPreferences can't hold"),
        ("dictionary", json!({}), "which Android SharedPreferences can't hold"),
        ("number", json!(1), "unknown type `number`"),
    ];
    for (ty, value, want) in refused {
        let err = write(&mut s, "k", ty, value.clone()).expect_err(ty);
        assert!(err.contains(want), "{ty} {value}: {err}");
    }
    assert_eq!(fake.entries(&store), before, "nothing refused reached the store");
    let err = api(&mut s, "prefs_write", &json!({"device": "pixel", "bundle_id": app, "store": store_id, "key": "k", "type": "int"}));
    assert_eq!(err.expect_err("no value"), "missing parameter `value`");

    // A running Android app would write its old values back: the backend stops it, writes, and
    // relaunches it, and says so.
    fake.reports(WriteResult {
        visible: Visibility::Now,
        app: AppState::Relaunched,
        note: Some("the app was running: stopped, written, relaunched".into()),
    });
    let done = write(&mut s, "name", "string", json!("Grace")).expect("write");
    assert_eq!(
        (done["visible"].clone(), done["app"].clone(), done["note"].clone()),
        (json!("now"), json!("relaunched"), json!("the app was running: stopped, written, relaunched"))
    );
    fake.reports(WriteResult { visible: Visibility::Now, app: AppState::Stopped, note: None });
    assert_eq!(write(&mut s, "name", "string", json!("Ada")).expect("write")["app"], "stopped");

    let gone = ok(&mut s, "prefs_delete", json!({"device": "pixel", "bundle_id": app, "store": store_id, "key": "name"}));
    assert_eq!((gone["key"].clone(), gone["app"].clone()), (json!("name"), json!("stopped")));
    assert!(!fake.entries(&store).expect("store").contains_key("name"));
    // DataStore holds bytes.
    let p = json!({"device": "pixel", "bundle_id": app, "store": "datastore/settings.preferences_pb", "key": "blob", "type": "data", "value": "AAEC"});
    ok(&mut s, "prefs_write", p);
    let ds = Store::parse(Platform::Android, &app, "datastore/settings.preferences_pb").expect("store");
    assert_eq!(fake.entries(&ds).expect("store")["blob"], PrefValue::Data(vec![0, 1, 2]));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_batch_is_all_or_nothing() {
    let (mut s, fake, app, root) = session("batch");
    let store_id = "datastore/settings.preferences_pb";
    let store = Store::parse(Platform::Android, &app, store_id).expect("store");
    let batch = |s: &mut methods::Session, ops: Value| {
        api(s, "prefs_batch", &json!({"device": "pixel", "bundle_id": app, "store": store_id, "ops": ops}))
    };
    let done = batch(
        &mut s,
        json!([{"key": "volume", "type": "double", "value": 0.5}, {"key": "tags", "delete": true}, {"key": "count", "type": "int", "value": 7}]),
    )
    .expect("batch");
    assert_eq!(done, json!({"store": store_id, "keys": ["volume", "tags", "count"], "visible": "now", "app": "untouched"}));
    let after = fake.entries(&store).expect("store");
    assert_eq!(after, [("count".to_owned(), PrefValue::Int(7)), ("volume".to_owned(), PrefValue::Double(0.5))].into());

    // One bad op refuses the whole batch before anything is written.
    let refused = [
        (
            json!([{"key": "a", "type": "int", "value": 1}, {"key": "b", "type": "int", "value": "x"}]),
            "`ops[1].value` must be a whole number",
        ),
        (json!([{"key": "a", "type": "int", "value": 1}, {"key": "a", "delete": true}]), "`ops` names `a` twice"),
        (json!([{"key": "a", "type": "date", "value": "2026-11-27T10:00:00Z"}]), "can't hold"),
        (json!([{"key": "a", "delete": true, "value": 1}]), "takes no `type` or `value`"),
        (json!([{"key": "a", "type": "int"}]), "give a string `type` and a `value`"),
        (json!([{"type": "int", "value": 1}]), "`ops[0]` has no string `key`"),
        (json!([{"key": "a", "delete": "yes"}]), "`ops[0].delete` must be true or false"),
        (json!([{"key": "a", "type": "int", "value": 1, "extra": 1}]), "`ops[0]` has `extra`"),
        (json!([{"key": "a", "type": "unsupported", "value": {}}]), "read-only"),
        (json!(["a"]), "`ops[0]` must be"),
        (json!([]), "`ops` is empty"),
        (json!({"key": "a"}), "`ops` must be an array"),
    ];
    for (ops, want) in refused {
        let err = batch(&mut s, ops.clone()).expect_err(&ops.to_string());
        assert!(err.contains(want), "{ops}: {err}");
        assert_eq!(fake.entries(&store).as_ref(), Some(&after), "{ops} wrote something");
    }
    // A backend failing part-way changes nothing either.
    fake.fail_on("b");
    let err = batch(&mut s, json!([{"key": "a", "type": "int", "value": 1}, {"key": "b", "type": "int", "value": 2}])).expect_err("fails");
    assert_eq!(err, "the fake failed writing `b`");
    assert_eq!(fake.entries(&store), Some(after));
    let err = api(&mut s, "prefs_batch", &json!({"device": "pixel", "bundle_id": app, "store": store_id})).expect_err("no ops");
    assert_eq!(err, "missing array parameter `ops`");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_watch_feed_reports_changes_in_order_with_old_and_new() {
    let (mut s, fake, app, root) = session("watch");
    let store_id = format!("shared_prefs/{app}_preferences.xml");
    let all = json!({"device": "pixel", "bundle_id": app});
    let first = ok(&mut s, "prefs_watch", all.clone());
    assert_eq!(first["events"], json!([]), "the first call is the baseline");
    let cursor = first["cursor"].as_str().expect("cursor").to_owned();
    assert_eq!(ok(&mut s, "prefs_watch", with(all.clone(), json!({"cursor": cursor})))["events"], json!([]), "nothing changed");

    let set = |s: &mut methods::Session, store: &str, key: &str, ty: &str, value: Value| {
        ok(s, "prefs_write", json!({"device": "pixel", "bundle_id": app, "store": store, "key": key, "type": ty, "value": value}));
    };
    set(&mut s, &store_id, "launches", "int", json!(4));
    set(&mut s, &store_id, "fresh", "bool", json!(true));
    set(&mut s, &store_id, "launches", "int", json!(4));
    ok(&mut s, "prefs_delete", json!({"device": "pixel", "bundle_id": app, "store": store_id, "key": "name"}));
    set(&mut s, "datastore/settings.preferences_pb", "x", "long", json!(1));

    let (events, last) = follow(&mut s, &all, &cursor, 4);
    let rows: Vec<(String, String, Value, Value)> = events
        .iter()
        .map(|e| {
            (e["store"].as_str().unwrap_or("").to_owned(), e["key"].as_str().unwrap_or("").to_owned(), e["old"].clone(), e["new"].clone())
        })
        .collect();
    let int = |n: i64| json!({"type": "int", "value": n});
    assert_eq!(
        rows,
        [
            (store_id.clone(), "launches".to_owned(), int(3), int(4)),
            (store_id.clone(), "fresh".to_owned(), Value::Null, json!({"type": "bool", "value": true})),
            (store_id.clone(), "name".to_owned(), json!({"type": "string", "value": "Ada"}), Value::Null),
            ("datastore/settings.preferences_pb".to_owned(), "x".to_owned(), Value::Null, json!({"type": "long", "value": 1})),
        ],
        "in order, an unchanged write reports nothing, every store of the app"
    );
    let ats: Vec<&str> = events.iter().filter_map(|e| e["at"].as_str()).collect();
    assert_eq!(ats[0], "2027-01-15T08:00:00Z", "ISO 8601");
    assert!(ats.windows(2).all(|w| Date::parse_iso8601(w[0]).ok() < Date::parse_iso8601(w[1]).ok()));
    let next = last["cursor"].as_str().expect("cursor").to_owned();
    assert_eq!(ok(&mut s, "prefs_watch", with(all.clone(), json!({"cursor": next})))["events"], json!([]), "read once, past the cursor");
    // An old cursor reads the same changes again (while the feed keeps them).
    assert_eq!(ok(&mut s, "prefs_watch", with(all.clone(), json!({"cursor": cursor})))["events"].as_array().map(Vec::len), Some(4));

    // One store's feed is its own, and sees only that store.
    let ds = with(all.clone(), json!({"store": "datastore/settings.preferences_pb"}));
    let one = ok(&mut s, "prefs_watch", ds.clone());
    set(&mut s, &store_id, "launches", "int", json!(5));
    set(&mut s, "datastore/settings.preferences_pb", "x", "long", json!(2));
    let (only, _) = follow(&mut s, &ds, one["cursor"].as_str().expect("cursor"), 1);
    assert_eq!(only.iter().map(|e| e["key"].clone()).collect::<Vec<_>>(), [json!("x")]);

    // A cursor that isn't this feed's starts over.
    assert_eq!(ok(&mut s, "prefs_watch", with(all.clone(), json!({"cursor": "999999:1"})))["reset"], true);
    assert_eq!(ok(&mut s, "prefs_watch", with(all.clone(), json!({"cursor": format!("{next}9")})))["reset"], true, "past the head");
    let err = api(&mut s, "prefs_watch", &with(all.clone(), json!({"wait_ms": -1}))).expect_err("wait");
    assert!(err.starts_with("`wait_ms` must be a whole number"), "{err}");

    // The device stopping closes the feed: what was sent is still read, then `ended`; a call
    // without a cursor starts a new feed.
    // The feed's thread takes changes as the backend sends them, so a fresh cursor can sit before
    // writes already made: follow to the head (the `launches` and `x` writes above) first.
    let (caught_up, head) = follow(&mut s, &all, &next, 2);
    assert_eq!(caught_up.len(), 2, "the writes since `next`");
    let before = head["cursor"].as_str().expect("cursor").to_owned();
    set(&mut s, &store_id, "launches", "int", json!(6));
    fake.close_feeds();
    let (late, ended) = follow(&mut s, &all, &before, 2);
    assert_eq!((late.len(), ended["ended"].clone()), (1, json!(true)), "{ended}");
    let again = ok(&mut s, "prefs_watch", all.clone());
    assert_eq!(again.get("ended"), None, "a new feed");
    assert_ne!(again["cursor"].as_str().and_then(|c| c.split(':').next()), before.split(':').next());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_watch_waits_for_the_next_change() {
    let (mut s, fake, app, root) = session("wait");
    let store = Store::parse(Platform::Android, &app, "datastore/settings.preferences_pb").expect("store");
    let cursor = ok(&mut s, "prefs_watch", json!({"device": "pixel", "bundle_id": app}))["cursor"].clone();
    let start = Instant::now();
    let quiet = ok(&mut s, "prefs_watch", json!({"device": "pixel", "bundle_id": app, "cursor": cursor, "wait_ms": 50}));
    assert_eq!(quiet["events"], json!([]));
    assert!(start.elapsed() >= Duration::from_millis(50), "{:?}", start.elapsed());
    // Another tool (the app) writes while the call waits.
    let mut other = fake.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        other.write("pixel", &store, "late", &PrefValue::Bool(true)).expect("write");
    });
    let start = Instant::now();
    let woke = ok(&mut s, "prefs_watch", json!({"device": "pixel", "bundle_id": app, "cursor": quiet["cursor"], "wait_ms": 10_000}));
    writer.join().expect("writer");
    assert_eq!(woke["events"][0]["key"], "late");
    assert!(start.elapsed() < Duration::from_secs(5), "answered when the change came: {:?}", start.elapsed());
    std::fs::remove_dir_all(root).ok();
}

fn change(key: &str, value: PrefValue) -> Change {
    Change {
        store: Store::new(StoreKind::AppDomain, "com.example", "com.example"),
        key: key.to_owned(),
        old: None,
        new: Some(value),
        at: Date::from_unix(0, 0),
    }
}

/// Polls `feed` from `cursor` until its head is `n` (the feed's thread has taken `n` changes).
fn until_head(feed: &Feed, cursor: &str, n: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let got = feed.poll(Some(cursor), Duration::from_millis(100));
        if got["cursor"].as_str().and_then(|c| c.split(':').nth(1)) == Some(&n.to_string()) {
            return;
        }
    }
    panic!("the feed never reached {n}");
}

#[test]
fn two_watchers_on_one_feed_do_not_hold_each_other_up() {
    let (tx, source) = sink();
    let feed = Feed::start(source, FEED_BYTES, Duration::from_secs(60)).expect("feed");
    let cursor = feed.poll(None, Duration::ZERO)["cursor"].as_str().expect("cursor").to_owned();
    // One watcher long-polls...
    let (waiting, from) = (feed.clone(), cursor.clone());
    let long = std::thread::spawn(move || {
        let start = Instant::now();
        (waiting.poll(Some(&from), Duration::from_secs(10)), start.elapsed())
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while feed.state().waiting == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(feed.state().waiting, 1, "the long poll is waiting");
    // ...while another reads with its cursor and comes straight back.
    let start = Instant::now();
    assert_eq!(feed.poll(Some(&cursor), Duration::ZERO)["events"], json!([]));
    assert!(start.elapsed() < Duration::from_millis(500), "not held by the waiting watcher: {:?}", start.elapsed());
    assert!(tx.send(change("k", PrefValue::Int(1))), "send");
    let (got, waited) = long.join().expect("long poll");
    assert_eq!(got["events"][0]["key"], "k");
    assert!(waited < Duration::from_secs(5), "woke on the change: {waited:?}");
    assert_eq!(feed.poll(Some(&cursor), Duration::ZERO)["events"].as_array().map(Vec::len), Some(1), "both see it");
}

#[test]
fn a_feed_keeps_its_byte_budget_and_counts_what_it_dropped() {
    let (tx, source) = sink();
    let each = change("k0", PrefValue::String("x".repeat(400))).size();
    let feed = Feed::start(source, each * 3 + each / 2, Duration::from_secs(60)).expect("feed");
    let cursor = feed.poll(None, Duration::ZERO)["cursor"].as_str().expect("cursor").to_owned();
    for i in 0..10 {
        assert!(tx.send(change(&format!("k{i}"), PrefValue::String("x".repeat(400)))), "send");
    }
    until_head(&feed, &cursor, 10);
    let got = feed.poll(Some(&cursor), Duration::ZERO);
    let keys: Vec<&str> = got["events"].as_array().expect("events").iter().filter_map(|e| e["key"].as_str()).collect();
    assert_eq!(keys, ["k7", "k8", "k9"], "the newest that fit");
    assert_eq!(got["missed"], 7);
    let state = feed.state();
    assert!(state.bytes <= state.capacity && state.bytes == each * 3, "{} of {}", state.bytes, state.capacity);
}

#[test]
fn an_idle_feed_stops_on_its_own_and_its_backend_stops_sending() {
    let fake = Fake::default();
    let mut backend = fake.clone();
    let store = Store::new(StoreKind::AppDomain, "com.example", "com.example");
    let feeds = Mutex::new(Some(Feeds::new(FEED_BYTES, Duration::from_millis(200), MAX_FEEDS)));
    let key = FeedKey { platform: Platform::Ios, device: "d".into(), app: "com.example".into(), store: None };
    let first = feed_for(&feeds, key.clone(), None, |sink| backend.watch("d", "com.example", None, sink)).expect("feed");
    // Nobody polls: its thread ends it, drops the receiver, and the backend's next send fails.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut n = 0;
    while fake.watchers() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        n += 1;
        backend.write("d", &store, "k", &PrefValue::Long(n)).expect("write");
    }
    assert_eq!(fake.watchers(), 0, "the backend stopped sending");
    let state = first.state();
    assert!(state.ended && state.events.is_empty(), "ended, and what it kept let go");
    drop(state);
    assert_eq!(first.poll(Some(&format!("{}:0", first.id)), Duration::ZERO)["ended"], true);
    // A call with its cursor still finds it ended; one without starts a new feed.
    let held = feed_for(&feeds, key.clone(), Some(first.id), |sink| backend.watch("d", "com.example", None, sink)).expect("feed");
    assert_eq!(held.id, first.id);
    let fresh = feed_for(&feeds, key, None, |sink| backend.watch("d", "com.example", None, sink)).expect("feed");
    assert_ne!(fresh.id, first.id);
    assert_eq!(fake.watchers(), 1);
}

#[test]
fn changes_between_two_reads_are_by_key() {
    let store = Store::new(StoreKind::SharedPreferences, "a", "a");
    let before: Entries =
        [("same".into(), PrefValue::Int(1)), ("gone".into(), PrefValue::Bool(true)), ("moved".into(), PrefValue::Int(1))].into();
    let after: Entries =
        [("same".into(), PrefValue::Int(1)), ("moved".into(), PrefValue::Long(1)), ("new".into(), PrefValue::Float(0.5))].into();
    let at = Date::from_unix(5, 0);
    let got: Vec<(String, Option<PrefValue>, Option<PrefValue>)> =
        changes(&store, &before, &after, at).into_iter().map(|c| (c.key, c.old, c.new)).collect();
    assert_eq!(
        got,
        [
            ("gone".to_owned(), Some(PrefValue::Bool(true)), None),
            ("moved".to_owned(), Some(PrefValue::Int(1)), Some(PrefValue::Long(1))),
            ("new".to_owned(), None, Some(PrefValue::Float(0.5))),
        ],
        "an int that became a long is a change"
    );
}

#[test]
fn devices_without_a_backend_yet_say_so_after_checking_parameters() {
    let (mut s, _fake, app, root) = session("unavailable");
    s.prefs = Session::default();
    for (method, extra) in [
        ("prefs_stores", json!({})),
        ("prefs_read", json!({})),
        ("prefs_write", json!({"store": "shared_prefs/a.xml", "key": "k", "type": "int", "value": 1})),
        ("prefs_delete", json!({"store": "shared_prefs/a.xml", "key": "k"})),
        ("prefs_batch", json!({"store": "shared_prefs/a.xml", "ops": [{"key": "k", "delete": true}]})),
        ("prefs_watch", json!({})),
    ] {
        let p = with(json!({"device": "pixel", "bundle_id": app}), extra);
        assert_eq!(
            api(&mut s, method, &p).expect_err(method),
            "app preferences (`prefs_*`) are not yet available on Android devices",
            "{method}"
        );
    }
    // A bad parameter is still reported as that, before the platform's gap.
    let err =
        api(&mut s, "prefs_write", &json!({"device": "pixel", "bundle_id": app, "store": "x", "key": "k", "type": "int", "value": 1}));
    assert!(err.expect_err("bad store").starts_with("`x` isn't an Android preferences store"));
    let mut ios = Unavailable(Platform::Ios);
    let err = call(&mut ios, Platform::Ios, "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "prefs_read", &json!({"bundle_id": "com.example"}));
    assert_eq!(err.expect_err("ios"), "app preferences (`prefs_*`) are not yet available on iOS devices");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn defaults_read_and_write_are_unchanged() {
    let row = |m: &str| METHODS.iter().find(|(name, ..)| *name == m).copied().unwrap_or_else(|| panic!("{m}"));
    // Unchanged but for the device set every iOS method takes (#409).
    assert_eq!(row("defaults_read"), ("defaults_read", "device, domain, set?", "Read a preferences domain"));
    assert_eq!(
        row("defaults_write"),
        ("defaults_write", "device, domain, key, value, type?, set?", "Write a preference (type: string|int|float|bool)")
    );
    for m in ["defaults_read", "defaults_write"] {
        assert_eq!(methods::platforms(m), [Platform::Ios], "{m}");
    }
    assert_eq!(crate::input_schema("defaults_read").expect("schema")["required"], json!(["device", "domain"]));
    let write = crate::input_schema("defaults_write").expect("schema");
    assert_eq!(write["required"], json!(["device", "domain", "key", "value"]));
    assert_eq!(write["properties"]["type"], json!({"type": "string"}));
    // Still the iOS backend's: on an Android device, refused as before, not by the prefs methods.
    let (mut s, _fake, _app, root) = session("defaults");
    let err = api(&mut s, "defaults_read", &json!({"device": "pixel", "domain": "x"})).expect_err("iOS only");
    assert_eq!(err, "`defaults_read` isn't available on Android devices (it works on iOS)");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_prefs_schemas_type_every_parameter() {
    let write = crate::input_schema("prefs_write").expect("schema");
    assert_eq!(write["required"], json!(["device", "bundle_id", "store", "key", "value", "type"]));
    let types: Vec<&str> = Type::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(write["properties"]["type"]["enum"], json!(types), "the grammar names every writable type");
    assert_eq!(crate::input_schema("prefs_read").expect("schema")["required"], json!(["device", "bundle_id"]));
    let batch = crate::input_schema("prefs_batch").expect("schema");
    assert_eq!(batch["required"], json!(["device", "bundle_id", "store", "ops"]));
    assert_eq!(batch["properties"]["ops"], json!({"type": "array", "items": {"type": "object"}}));
    let watch = crate::input_schema("prefs_watch").expect("schema");
    assert_eq!(watch["required"], json!(["device", "bundle_id"]));
    assert_eq!(
        (watch["properties"]["cursor"]["type"].clone(), watch["properties"]["wait_ms"]["type"].clone()),
        (json!("string"), json!("integer"))
    );
}

#[test]
fn a_backend_learns_its_feed_ended_without_sending_anything() {
    let feeds = Mutex::new(Some(Feeds::new(FEED_BYTES, Duration::from_millis(100), MAX_FEEDS)));
    let key = FeedKey { platform: Platform::Ios, device: "d".into(), app: "com.example".into(), store: None };
    let mut kept = None;
    let feed = feed_for(&feeds, key, None, |sink| {
        kept = Some(sink);
        Ok(())
    })
    .expect("feed");
    let sink = kept.expect("the backend got a sink");
    assert!(sink.is_open());
    // Nobody reads it and nothing changes: the feed ends, and the sink says so.
    let deadline = Instant::now() + Duration::from_secs(5);
    while sink.is_open() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!sink.is_open() && feed.state().ended, "closed by the feed ending, with no send");
    assert!(!sink.send(change("k", PrefValue::Int(1))), "and a send says so too");
}

#[test]
fn a_backends_watch_starts_without_holding_the_feeds_lock() {
    let feeds = Mutex::new(Some(Feeds::new(FEED_BYTES, FEED_IDLE, MAX_FEEDS)));
    let key = FeedKey { platform: Platform::Ios, device: "d".into(), app: "com.example".into(), store: None };
    let mut free = false;
    let mut kept = None;
    feed_for(&feeds, key.clone(), None, |sink| {
        // Another feed's caller could take the lock now.
        free = feeds.try_lock().is_ok();
        kept = Some(sink);
        Ok(())
    })
    .expect("feed");
    assert!(free, "the lock is free while the backend starts");
    // Two starts of one feed at once: the first one in is kept, the other's sink closes.
    let (late, source) = sink();
    let first = with_feeds(&feeds, |f| f.find(&key, None)).expect("find").expect("running");
    let again = with_feeds(&feeds, |f| f.insert(key.clone(), None, source)).expect("insert");
    assert_eq!(again.id, first.id);
    assert!(!late.is_open(), "the second start's sink is closed");
    assert!(kept.is_some_and(|s| s.is_open()), "the kept feed's isn't");
}

#[test]
fn a_process_runs_at_most_so_many_feeds() {
    let feeds = Mutex::new(Some(Feeds::new(FEED_BYTES, FEED_IDLE, 2)));
    let key = |app: &str| FeedKey { platform: Platform::Ios, device: "d".into(), app: app.into(), store: None };
    let mut sinks = Vec::new();
    for app in ["a", "b"] {
        feed_for(&feeds, key(app), None, |sink| {
            sinks.push(sink);
            Ok(())
        })
        .expect(app);
    }
    let mut started = false;
    let Err(err) = feed_for(&feeds, key("c"), None, |_| {
        started = true;
        Ok(())
    }) else {
        panic!("a third feed started")
    };
    assert!(err.starts_with("2 preference feeds are running in this process, the most it runs"), "{err}");
    assert!(!started, "refused before the backend starts watching");
    // A running feed is still found by its callers.
    feed_for(&feeds, key("a"), None, |_| Err("not started again".into())).expect("found");
}

#[test]
fn a_fresh_read_asks_the_backend_for_one() {
    struct Fresh;
    impl Backend for Fresh {
        fn stores(&mut self, _: &str, app: &str) -> Result<Vec<Store>, String> {
            Ok(vec![Store::new(StoreKind::AppDomain, app, app)])
        }
        fn read(&mut self, _: &str, _: &Store) -> Result<Entries, String> {
            Ok([("from".to_owned(), PrefValue::String("file".into()))].into())
        }
        fn read_fresh(&mut self, _: &str, _: &Store) -> Result<Entries, String> {
            Ok([("from".to_owned(), PrefValue::String("cfprefsd".into()))].into())
        }
        fn write_batch(&mut self, _: &str, _: &Store, _: &[Op]) -> Result<WriteResult, String> {
            Err("no".into())
        }
        fn watch(&mut self, _: &str, _: &str, _: Option<&Store>, _: Sink) -> Result<(), String> {
            Err("no".into())
        }
    }
    let read = |p: Value| call(&mut Fresh, Platform::Ios, "D", "prefs_read", &p).expect("read")[0]["values"]["from"]["value"].clone();
    assert_eq!(read(json!({"bundle_id": "com.example"})), "file");
    assert_eq!(read(json!({"bundle_id": "com.example", "fresh": true})), "cfprefsd");
    assert_eq!(read(json!({"bundle_id": "com.example", "store": "com.example", "fresh": true})), "cfprefsd");
    let err = call(&mut Fresh, Platform::Ios, "D", "prefs_read", &json!({"bundle_id": "com.example", "fresh": "yes"}));
    assert!(err.is_err(), "fresh is a bool");
}

#[test]
fn android_devices_still_answer_not_yet_available_and_ios_has_a_backend() {
    let mut session = Session::default();
    let err = call(&mut *session.android, Platform::Android, "pixel", "prefs_stores", &json!({"bundle_id": "com.example"}));
    assert_eq!(err.expect_err("android"), "app preferences (`prefs_*`) are not yet available on Android devices");
    // The iOS backend answers from the device's files (here: a device that doesn't exist).
    let udid = "1111AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";
    let err = call(&mut *session.ios, Platform::Ios, udid, "prefs_stores", &json!({"bundle_id": "com.example"})).expect_err("no device");
    #[cfg(target_os = "macos")]
    assert!(err.ends_with(&format!("doesn't exist: is `{udid}` a simulator of the default device set?")), "{err}");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(err, "app preferences (`prefs_*`) are not yet available on iOS devices");
}

/// #409: a long-lived server keeps the preference backends of the few device sets it was asked
/// about last, not one per set any client ever named.
#[cfg(target_os = "macos")]
#[test]
fn a_session_keeps_the_backends_of_a_bounded_number_of_device_sets() {
    let mut prefs = Session::default();
    let mut ios = crate::ios::Session::default();
    for n in 0..40 {
        ios.use_set(Ok(Some(format!("/nonexistent/simee-409/set-{n}"))));
        prefs.backend_on(Platform::Ios, &ios).expect("a backend");
    }
    assert!(prefs.sets.len() <= crate::ios::MAX_SETS, "{} backends kept", prefs.sets.len());
}
