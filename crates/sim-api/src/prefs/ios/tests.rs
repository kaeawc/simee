use std::path::{Path, PathBuf};

use super::super::plist::{self, Plist};
use super::super::{Op, PrefValue, Store, StoreKind, fake::unique};
use super::*;

const UDID: &str = "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000";
const APP: &str = "com.example.app";
/// cfprefsd's file for a test app on iOS 27.2 (`plist.rs`'s fixture).
const EVERY_TYPE: &[u8] = include_bytes!("../fixtures/cfprefsd-every-type.plist");

fn metadata(id: &str) -> Vec<u8> {
    plist::encode(&Plist::Dict(vec![
        ("MCMMetadataIdentifier".into(), Plist::String(id.into())),
        ("MCMMetadataVersion".into(), Plist::Integer(11)),
    ]))
    .expect("encode")
}

fn container(dir: &Path, id: &str) {
    std::fs::create_dir_all(dir.join("Library/Preferences")).expect("dir");
    std::fs::write(dir.join(METADATA), metadata(id)).expect("metadata");
}

fn defaults(entries: &[(&str, Plist)]) -> Vec<u8> {
    plist::encode(&Plist::Dict(entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())).expect("encode")
}

/// Writes a defaults file the way cfprefsd replaces one: a new file renamed over it.
fn replace(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// A device set with one booted device: the app (entitled to `group.com.example`, with a
/// defaults file of every type, another domain and a suite named like a group it isn't entitled
/// to), another app, and the group's container.
fn device_set(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("sim-api-prefs-ios-{}", unique(tag)));
    let dev = root.join(UDID);
    std::fs::create_dir_all(&dev).expect("dev");
    // As CoreSimulator writes it: XML, with a date (so not a flat dictionary of scalars).
    std::fs::write(
        dev.join("device.plist"),
        "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>isDeleted</key>\n\t<false/>\n\t<key>lastUsedAt</key>\n\
         \t<date>2026-11-27T12:02:24Z</date>\n\t<key>state</key>\n\t<integer>3</integer>\n</dict>\n</plist>\n",
    )
    .expect("device.plist");
    let data = dev.join("data");
    let app = data.join("Containers/Data/Application/AAAA");
    container(&app, APP);
    std::fs::write(app.join(format!("Library/Preferences/{APP}.plist")), EVERY_TYPE).expect("app plist");
    std::fs::write(app.join("Library/Preferences/other.domain.plist"), defaults(&[("k", Plist::Bool(true))])).expect("other");
    std::fs::write(app.join("Library/Preferences/group.unentitled.plist"), defaults(&[("u", Plist::Integer(1))])).expect("suite");
    container(&data.join("Containers/Data/Application/BBBB"), "com.other");
    let bundle = data.join("Containers/Bundle/Application/CCCC");
    container(&bundle, APP);
    std::fs::create_dir_all(bundle.join("Example.app")).expect("app dir");
    std::fs::write(bundle.join("Example.app/Info.plist"), defaults(&[("CFBundleExecutable", Plist::String("Example".into()))]))
        .expect("info");
    let mut exe = b"\xcf\xfa\xed\xfe junk \x00\x01".to_vec();
    exe.extend_from_slice(
        b"<?xml version=\"1.0\"?><plist><dict><key>com.apple.security.application-groups</key>\n\t<array>\n\t\t<string>group.com.example</string>\n\t</array></dict></plist>\x00more",
    );
    std::fs::write(bundle.join("Example.app/Example"), exe).expect("exe");
    let group = data.join("Containers/Shared/AppGroup/GGGG");
    container(&group, "group.com.example");
    std::fs::write(group.join("Library/Preferences/group.com.example.plist"), defaults(&[("g", Plist::String("shared".into()))]))
        .expect("group");
    container(&data.join("Containers/Shared/AppGroup/HHHH"), "group.unentitled");
    (root, app)
}

fn ids(stores: &[Store]) -> Vec<(String, &'static str)> {
    stores.iter().map(|s| (s.id(), s.kind.as_str())).collect()
}

#[test]
fn stores_are_the_app_domain_its_entitled_groups_and_its_other_domains() {
    let (root, _) = device_set("stores");
    let mut ios = Ios::with_root(root.clone());
    let stores = ios.list(UDID, APP).expect("stores");
    assert_eq!(
        ids(&stores),
        [
            (APP.to_owned(), "app_domain"),
            ("group.com.example".to_owned(), "app_group"),
            ("group.unentitled".to_owned(), "app_group"),
            ("other.domain".to_owned(), "domain"),
        ]
    );
    let err = ios.list(UDID, "com.absent").expect_err("absent");
    assert_eq!(err, "`com.absent` has no data container on this device: is it installed?");
    assert!(ios.list("1111AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", APP).expect_err("no device").contains("doesn't exist"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn reads_decode_the_file_cfprefsd_uses_for_each_store() {
    let (root, app) = device_set("reads");
    let mut ios = Ios::with_root(root.clone());
    let read = |ios: &mut Ios, id: &str| ios.read_host(UDID, &Store::parse(Platform::Ios, APP, id).expect("store")).expect(id);
    let own = read(&mut ios, APP);
    assert_eq!(own.get("t_long"), Some(&PrefValue::Long(1_099_511_627_776)));
    assert_eq!(own.len(), 9);
    assert_eq!(read(&mut ios, "group.com.example").get("g"), Some(&PrefValue::String("shared".into())), "the group's container");
    assert_eq!(read(&mut ios, "group.unentitled").get("u"), Some(&PrefValue::Long(1)), "not entitled: the app's own container");
    assert_eq!(read(&mut ios, "never.written"), Entries::new(), "nothing written yet");
    // A reinstalled app gets a new container: found again, not read from the old path.
    let moved = app.with_file_name("ZZZZ");
    std::fs::rename(&app, &moved).expect("move");
    assert_eq!(read(&mut ios, APP).len(), 9);
    std::fs::write(moved.join("Library/Preferences/bad.plist"), b"<?xml junk").expect("bad");
    let err = ios.read_host(UDID, &Store::new(StoreKind::Domain, APP, "bad")).expect_err("bad");
    assert!(err.ends_with("not a binary property list (bplist00) (`fresh: true` reads it through cfprefsd)"), "{err}");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_tracker_reports_each_changed_key_as_files_are_replaced() {
    let (root, app) = device_set("tracker");
    let mut ios = Ios::with_root(root.clone());
    let mut tracker = ios.tracker(UDID, APP, None).expect("tracker");
    let prefs = app.join("Library/Preferences");
    let paths = tracker.paths();
    assert!(paths.contains(&prefs) && paths.contains(&prefs.join(format!("{APP}.plist"))), "{paths:?}");
    let at = Date::from_unix(1_800_000_000, 0);
    assert_eq!(tracker.poll(at), (vec![], false), "nothing changed");

    // The app's file replaced: one key changed, one added, the rest the same.
    let mut entries = plist::decode(EVERY_TYPE).expect("decode");
    if let Plist::Dict(e) = &mut entries {
        e.retain(|(k, _)| k != "t_bool");
        e.iter_mut().filter(|(k, _)| k == "app_tick").for_each(|(_, v)| *v = Plist::Integer(50));
        e.push(("added".into(), Plist::Real(0.5)));
    }
    replace(&prefs.join(format!("{APP}.plist")), &plist::encode(&entries).expect("encode"));
    let (changes, grew) = tracker.poll(at);
    let got: Vec<(String, Option<PrefValue>, Option<PrefValue>)> = changes.into_iter().map(|c| (c.key, c.old, c.new)).collect();
    assert_eq!(
        got,
        [
            ("added".to_owned(), None, Some(PrefValue::Double(0.5))),
            ("app_tick".to_owned(), Some(PrefValue::Long(49)), Some(PrefValue::Long(50))),
            ("t_bool".to_owned(), Some(PrefValue::Bool(true)), None),
        ]
    );
    assert!(!grew);

    // A file half-written (it can't be read) is read again on the next look.
    std::fs::write(prefs.join("other.domain.plist"), b"bplist00").expect("torn");
    assert_eq!(tracker.poll(at).0, vec![]);
    replace(&prefs.join("other.domain.plist"), &defaults(&[("k", Plist::Bool(false))]));
    let (changes, _) = tracker.poll(at);
    assert_eq!(changes.iter().map(|c| (c.store.id(), c.key.as_str())).collect::<Vec<_>>(), [("other.domain".to_owned(), "k")]);

    // A new domain is followed from empty; a removed file removes its keys.
    std::fs::write(prefs.join("new.domain.plist"), defaults(&[("n", Plist::Integer(1))])).expect("new");
    let (changes, grew) = tracker.poll(at);
    assert!(grew, "a new store to watch");
    assert_eq!(
        changes.iter().map(|c| (c.store.id(), c.key.as_str(), c.old.is_none())).collect::<Vec<_>>(),
        [("new.domain".to_owned(), "n", true)]
    );
    std::fs::remove_file(prefs.join("new.domain.plist")).expect("rm");
    let (changes, _) = tracker.poll(at);
    assert_eq!(changes.iter().map(|c| (c.key.as_str(), c.new.is_none())).collect::<Vec<_>>(), [("n", true)]);

    // One store's tracker sees only its store.
    let group = Store::parse(Platform::Ios, APP, "group.com.example").expect("store");
    let mut one = ios.tracker(UDID, APP, Some(&group)).expect("tracker");
    replace(&prefs.join("other.domain.plist"), &defaults(&[("k", Plist::Bool(true))]));
    assert_eq!(one.poll(at), (vec![], false));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn entitled_groups_come_from_every_entitlements_copy_in_the_executable() {
    let exe = b"\x00<key>com.apple.security.application-groups</key><array><string>group.b</string>\n<string>group.a</string></array>\
        \x00\x01<key>com.apple.security.application-groups</key>\r\n<array>\t<string>group.a</string></array>\
        <key>com.apple.security.application-groups</key><string>not-an-array</string>";
    assert_eq!(entitled_groups(exe), ["group.a", "group.b"]);
    assert_eq!(entitled_groups(b"no entitlements"), Vec::<String>::new());
}

#[test]
fn a_write_request_carries_every_op_for_the_helper() {
    let store = Store::new(StoreKind::AppDomain, APP, APP);
    let ops = [
        Op::Set { key: "f".into(), value: PrefValue::Float(1.5) },
        Op::Set { key: "a".into(), value: PrefValue::Array(vec![PrefValue::Int(1)]) },
        Op::Delete { key: "gone".into() },
    ];
    let request = plist::decode(&write_request(&store, Path::new("/c"), &ops).expect("request")).expect("decode");
    assert_eq!(
        request,
        Plist::Dict(vec![
            ("domain".into(), Plist::String(APP.into())),
            ("container".into(), Plist::String("/c".into())),
            ("set".into(), Plist::Dict(vec![("f".into(), Plist::Real(1.5)), ("a".into(), Plist::Array(vec![Plist::Integer(1)]))])),
            ("delete".into(), Plist::Array(vec![Plist::String("gone".into())])),
        ])
    );
}

#[test]
fn the_helpers_read_reply_is_decoded_and_its_errors_said() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(EVERY_TYPE);
    assert_eq!(parse_read_reply(&format!("ok {b64}\n")).expect("ok").len(), 9);
    assert_eq!(parse_read_reply("error this runtime has no X\n").expect_err("err"), "cfprefsd: this runtime has no X");
    assert_eq!(parse_read_reply("").expect_err("empty"), "cfprefsd: the helper said nothing");
}

#[test]
#[cfg(target_os = "macos")]
fn a_failed_final_synchronize_names_the_keys_it_sent() {
    let ops = [Op::Set { key: "a".into(), value: PrefValue::Bool(true) }, Op::Delete { key: "b".into() }];
    let err = write_failure("error cfprefsd didn't synchronize com.x after taking the keys\n", &ops);
    assert!(err.contains("[a, b]") && err.contains("already be live"), "{err}");
    // A refusal before any key was sent says only what the helper said.
    assert_eq!(write_failure("error the request has no domain\n", &ops), "cfprefsd: the request has no domain");
}

#[test]
fn a_device_is_booted_by_its_device_plist() {
    let (root, _) = device_set("booted");
    let dev = root.join(UDID);
    assert!(booted(&dev));
    let shutdown = std::fs::read_to_string(dev.join("device.plist")).expect("read").replace("<integer>3</integer>", "<integer>1</integer>");
    std::fs::write(dev.join("device.plist"), shutdown).expect("write");
    assert!(!booted(&dev));
    let binary = plist::encode(&Plist::Dict(vec![("state".into(), Plist::Integer(3))])).expect("encode");
    std::fs::write(dev.join("device.plist"), binary).expect("write");
    assert!(booted(&dev), "a binary device.plist");
    assert!(!booted(&root.join("gone")), "a deleted device");
    std::fs::remove_dir_all(root).ok();
}

// ---- The in-device observer (#235) ----

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A feed's files as a test drives them: each wait is a short tick, counted; `hold` keeps the
/// watcher in its wait (`held` once it is there); `ended` once the feed's watcher has dropped them
/// (its thread ended).
struct FakeFiles {
    ended: Arc<AtomicBool>,
    waits: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
}

impl Files for FakeFiles {
    fn wait(&mut self) {
        self.waits.fetch_add(1, SeqCst);
        while self.hold.load(SeqCst) {
            self.held.store(true, SeqCst);
            std::thread::sleep(Duration::from_millis(1));
        }
        self.held.store(false, SeqCst);
        std::thread::sleep(Duration::from_millis(5));
    }

    fn set(&mut self, _: &[PathBuf]) {}
}

impl Drop for FakeFiles {
    fn drop(&mut self) {
        self.ended.store(true, SeqCst);
    }
}

/// The fake notification source: an observer the test speaks for through a pipe. Its stop is
/// recorded and closes the pipe, as a killed helper's output closes.
struct FakeObserver {
    out: Arc<Mutex<Option<std::io::PipeWriter>>>,
    stopped: Arc<AtomicBool>,
}

impl FakeObserver {
    fn new() -> (FakeObserver, Observer) {
        let (reader, writer) = std::io::pipe().expect("pipe");
        let out = Arc::new(Mutex::new(Some(writer)));
        let stopped = Arc::new(AtomicBool::new(false));
        let (o, s) = (out.clone(), stopped.clone());
        let stop = Box::new(move || {
            s.store(true, SeqCst);
            o.lock().expect("lock").take();
        });
        (FakeObserver { out, stopped }, Observer { reader: Box::new(reader), stop })
    }

    fn say(&self, line: &str) {
        use std::io::Write as _;
        let mut out = self.out.lock().expect("lock");
        writeln!(out.as_mut().expect("the observer is running"), "{line}").expect("write");
    }

    /// Says `line` from another thread, which may block on a full pipe until the feed stops reading.
    fn say_from_a_thread(&self, line: String) -> std::thread::JoinHandle<()> {
        let out = self.out.clone();
        std::thread::spawn(move || {
            use std::io::Write as _;
            if let Some(out) = out.lock().expect("lock").as_mut() {
                // The feed may stop reading partway (and close its end): that write fails.
                writeln!(out, "{line}").ok();
            }
        })
    }

    /// The helper exits on its own.
    fn exit(&self) {
        self.out.lock().expect("lock").take();
    }

    fn stopped(&self) -> bool {
        self.stopped.load(SeqCst)
    }
}

type Seen = (String, String, Option<PrefValue>, Option<PrefValue>);

/// A feed a test drives: its source, its watcher's files, and what it said on stderr.
struct Feed {
    source: Option<super::super::Source>,
    ended: Arc<AtomicBool>,
    waits: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    held: Arc<AtomicBool>,
    notices: Arc<Mutex<Vec<String>>>,
}

impl Feed {
    fn source(&self) -> &super::super::Source {
        self.source.as_ref().expect("the feed is open")
    }

    /// The consumer drops the feed.
    fn close(&mut self) {
        self.source.take();
    }

    /// The feed's next change, which the test expects (so a deadline, not a measure of "none").
    fn next(&self) -> Option<Seen> {
        let c = self.source().rx.recv_timeout(Duration::from_secs(5)).ok()?;
        Some((c.store.id(), c.key, c.old, c.new))
    }

    /// Nothing done before this brought a change: the watcher has looked at the files twice since
    /// (the second look starts after the first one's changes are sent).
    fn quiet(&self, why: &str) {
        let from = self.waits.load(SeqCst);
        until("the watcher to look at the files again", || self.waits.load(SeqCst) >= from + 2);
        let extra = self.source().rx.try_recv().ok().map(|c| (c.store.id(), c.key, c.old, c.new));
        assert_eq!(extra, None, "{why}");
    }

    /// Holds the watcher in its wait until `let_go`, so it doesn't see the feed end.
    fn hold(&self) {
        self.hold.store(true, SeqCst);
        until("the watcher to be held", || self.held.load(SeqCst));
    }

    fn let_go(&self) {
        self.hold.store(false, SeqCst);
    }

    /// The feed's notice on stderr that contains `what`.
    fn notice(&self, what: &str) -> String {
        let find = || self.notices.lock().expect("lock").iter().find(|n| n.contains(what)).cloned();
        until(&format!("a notice saying {what:?}"), || find().is_some());
        find().unwrap_or_default()
    }

    fn notices(&self) -> Vec<String> {
        self.notices.lock().expect("lock").clone()
    }
}

/// A feed of every store of the app, with `observer`.
fn feed(ios: &mut Ios, alive: impl Fn() -> bool + Send + Sync + 'static, observer: Option<Observer>) -> Feed {
    let (sink, source) = super::super::sink();
    let [ended, hold, held] = [(); 3].map(|()| Arc::new(AtomicBool::new(false)));
    let waits = Arc::new(AtomicUsize::new(0));
    let notices = Arc::new(Mutex::new(Vec::new()));
    let files = FakeFiles { ended: ended.clone(), waits: waits.clone(), hold: hold.clone(), held: held.clone() };
    let n = notices.clone();
    let say = move |line| n.lock().expect("lock").push(line);
    run_feed(ios.tracker(UDID, APP, None).expect("tracker"), sink, files, alive, observer, say).expect("feed");
    Feed { source: Some(source), ended, waits, hold, held, notices }
}

fn until(what: &str, done: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The app's own store as `EVERY_TYPE` has it (`app_tick` 49), with `app_tick` set to `tick`.
fn app_with_tick(tick: i64) -> Vec<u8> {
    let mut entries = plist::decode(EVERY_TYPE).expect("decode");
    if let Plist::Dict(e) = &mut entries {
        e.iter_mut().filter(|(k, _)| k == "app_tick").for_each(|(_, v)| *v = Plist::Integer(tick.into()));
    }
    plist::encode(&entries).expect("encode")
}

/// The observer's line for store `i` holding the binary plist `bytes`.
fn snap(i: usize, bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!("snap {i} {}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn long(n: i64) -> Option<PrefValue> {
    Some(PrefValue::Long(n))
}

#[test]
fn the_observer_is_given_each_stores_domain_and_container() {
    let (root, app) = device_set("observer-args");
    let mut ios = Ios::with_root(root.clone());
    let tracker = ios.tracker(UDID, APP, None).expect("tracker");
    let group = root.join(UDID).join("data/Containers/Shared/AppGroup/GGGG").display().to_string();
    let app = app.display().to_string();
    let want: Vec<String> =
        [APP, &app, "group.com.example", &group, "group.unentitled", &app, "other.domain", &app].map(str::to_owned).into();
    assert_eq!(tracker.observer_args(), want);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_observers_lines_are_decoded_and_anything_else_refused() {
    assert_eq!(observer_line("ready"), Ok(ObserverLine::Ready));
    assert_eq!(observer_line("error this runtime has no X\n"), Ok(ObserverLine::Error("this runtime has no X".into())));
    let Ok(ObserverLine::Snap(1, entries)) = observer_line(&snap(1, &defaults(&[("k", Plist::Integer(2))]))) else { panic!("not a snap") };
    assert_eq!(entries.get("k"), Some(&PrefValue::Long(2)));
    assert_eq!(observer_line("large 2"), Ok(ObserverLine::Large(2)));
    for bad in ["snap x AAAA", "snap 1 !!", "snap 1", "large", "large x", "hello", ""] {
        assert!(observer_line(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_change_cfprefsd_announces_reaches_the_feed_before_the_file_and_the_file_adds_nothing() {
    let (root, app) = device_set("observer");
    let mut ios = Ios::with_root(root.clone());
    let (fake, observer) = FakeObserver::new();
    let mut feed = feed(&mut ios, || true, Some(observer));
    // Its first look at the store matches the file: nothing to report, so the first change the
    // feed has is the write's (the observer's lines are followed in order).
    fake.say(&snap(0, EVERY_TYPE));
    fake.say("ready");
    // The app writes: cfprefsd tells the observer, and the feed has it with no file written.
    fake.say(&snap(0, &app_with_tick(50)));
    assert_eq!(feed.next(), Some((APP.to_owned(), "app_tick".to_owned(), long(49), long(50))));
    // An older flush landing after the write, then cfprefsd's file catching up: neither is a change.
    let file = app.join(format!("Library/Preferences/{APP}.plist"));
    replace(&file, EVERY_TYPE);
    feed.quiet("a file behind cfprefsd isn't read while the observer follows its store");
    replace(&file, &app_with_tick(50));
    feed.quiet("cfprefsd's file catching up isn't a change");
    // A store the observer hasn't reported is still followed by its file.
    replace(&app.join("Library/Preferences/other.domain.plist"), &defaults(&[("k", Plist::Bool(false))]));
    assert_eq!(feed.next().map(|c| (c.0, c.1)), Some(("other.domain".to_owned(), "k".to_owned())));
    assert_eq!(feed.notices(), Vec::<String>::new(), "a feed on its observer says nothing");
    feed.close();
    until("the feed to end", || feed.ended.load(SeqCst));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_helper_that_cant_load_or_exits_leaves_the_feed_on_the_files() {
    let (root, app) = device_set("fallback");
    let file = app.join(format!("Library/Preferences/{APP}.plist"));
    let mut ios = Ios::with_root(root.clone());

    // No observer at all (it couldn't be built or spawned): the feed follows the files, as before.
    let mut feed1 = feed(&mut ios, || true, None);
    replace(&file, &app_with_tick(60));
    assert_eq!(feed1.next().map(|c| (c.1, c.3)), Some(("app_tick".to_owned(), long(60))));
    feed1.close();
    until("the feed to end", || feed1.ended.load(SeqCst));

    // A helper that can't follow the stores (a runtime without the SPI) says so and exits.
    let (fake, observer) = FakeObserver::new();
    let mut feed2 = feed(&mut ios, || true, Some(observer));
    fake.say("error this runtime's CoreFoundation has no handleChangeNotificationForDomainIdentifier:isRemote:");
    fake.exit();
    assert!(feed2.notice("follows the files from now on").ends_with("has no handleChangeNotificationForDomainIdentifier:isRemote:"));
    replace(&file, &app_with_tick(61));
    assert_eq!(feed2.next().map(|c| (c.1, c.3)), Some(("app_tick".to_owned(), long(61))));
    feed2.close();
    until("the feed to end", || feed2.ended.load(SeqCst));
    assert!(!fake.stopped(), "a helper that exited isn't signalled (its pid may be another process's by now)");

    // A helper that followed the store and then died: the feed says so in one line, and the files
    // take over from the values it last reported (the file it left behind is older than them, and
    // isn't a change back).
    let (fake, observer) = FakeObserver::new();
    let mut feed3 = feed(&mut ios, || true, Some(observer));
    fake.say("ready");
    fake.say(&snap(0, &app_with_tick(62)));
    assert_eq!(feed3.next().map(|c| (c.2, c.3)), Some((long(61), long(62))));
    replace(&file, &app_with_tick(61));
    fake.exit();
    assert!(feed3.notice("follows the files from now on").ends_with("the preferences observer exited"));
    feed3.quiet("the stale file isn't read as a change back");
    replace(&file, &app_with_tick(63));
    assert_eq!(feed3.next().map(|c| (c.2, c.3)), Some((long(62), long(63))));
    feed3.close();
    until("the feed to end", || feed3.ended.load(SeqCst));
    assert!(!fake.stopped(), "a helper that exited isn't signalled");
    assert_eq!(feed3.notices().len(), 1, "one line: {:?}", feed3.notices());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_observer_stops_when_the_feed_ends_or_the_device_stops() {
    let (root, _) = device_set("stop");
    let mut ios = Ios::with_root(root.clone());
    let (fake, observer) = FakeObserver::new();
    let mut feed1 = feed(&mut ios, || true, Some(observer));
    fake.say("ready");
    fake.say(&snap(0, &app_with_tick(90)));
    assert_eq!(feed1.next().map(|c| c.3), Some(long(90)));
    feed1.quiet("nothing else changed");
    assert!(!fake.stopped(), "it runs while the feed does");
    feed1.close();
    until("the observer to be stopped", || fake.stopped());
    until("the feed to end", || feed1.ended.load(SeqCst));
    assert_eq!(feed1.notices(), Vec::<String>::new(), "a feed that ended says nothing");

    // The device shut down: the feed ends, and its observer is stopped with it.
    let booted = Arc::new(AtomicBool::new(true));
    let b = booted.clone();
    let (fake, observer) = FakeObserver::new();
    let feed2 = feed(&mut ios, move || b.load(SeqCst), Some(observer));
    fake.say("ready");
    booted.store(false, SeqCst);
    until("the observer to be stopped", || fake.stopped());
    until("the feed to end", || feed2.ended.load(SeqCst));
    assert_eq!(feed2.notices(), Vec::<String>::new(), "an observer stopped with its feed isn't a fallback");
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn the_helper_is_stopped_when_its_reader_ends_first() {
    let (root, _) = device_set("reader-ends");
    let mut ios = Ios::with_root(root.clone());

    // The consumer drops the feed, and the app writes before the watcher next wakes: the reader
    // can't hand the change on and ends with the helper still running. It stops the helper.
    let (fake, observer) = FakeObserver::new();
    let mut feed1 = feed(&mut ios, || true, Some(observer));
    fake.say("ready");
    feed1.hold();
    feed1.close();
    fake.say(&snap(0, &app_with_tick(70)));
    until("the reader to stop the helper", || fake.stopped());
    feed1.let_go();
    until("the feed to end", || feed1.ended.load(SeqCst));
    assert_eq!(feed1.notices(), Vec::<String>::new(), "a feed that ended says nothing");

    // A line the feed can't read: it stops the helper, says why, and follows the files.
    let (fake, observer) = FakeObserver::new();
    let mut feed2 = feed(&mut ios, || true, Some(observer));
    fake.say("ready");
    feed2.hold();
    fake.say("hello");
    until("the reader to stop the helper", || fake.stopped());
    assert!(feed2.notice("follows the files from now on").ends_with("the preferences observer printed \"hello\""));
    feed2.let_go();
    feed2.close();
    until("the feed to end", || feed2.ended.load(SeqCst));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_store_too_large_to_send_is_followed_by_its_file() {
    let (root, app) = device_set("large");
    let file = app.join(format!("Library/Preferences/{APP}.plist"));
    let mut ios = Ios::with_root(root.clone());

    // The helper won't send a store past its cap (`large <i>`): that store goes back to its file,
    // with a notice, and the others stay on the observer.
    let (fake, observer) = FakeObserver::new();
    let mut feed1 = feed(&mut ios, || true, Some(observer));
    fake.say(&snap(0, EVERY_TYPE));
    fake.say("ready");
    fake.say("large 0");
    assert!(feed1.notice("follows its file").contains(APP), "{:?}", feed1.notices());
    replace(&file, &app_with_tick(80));
    assert_eq!(feed1.next().map(|c| (c.0, c.1, c.3)), Some((APP.to_owned(), "app_tick".to_owned(), long(80))));
    fake.say(&snap(3, &defaults(&[("k", Plist::Bool(false))])));
    assert_eq!(feed1.next().map(|c| (c.0, c.1)), Some(("other.domain".to_owned(), "k".to_owned())));
    assert!(!fake.stopped(), "the observer still follows the other stores");
    // It fits again: the observer follows it again.
    fake.say(&snap(0, &app_with_tick(81)));
    assert_eq!(feed1.next().map(|c| (c.2, c.3)), Some((long(80), long(81))));
    feed1.close();
    until("the feed to end", || feed1.ended.load(SeqCst));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn a_line_past_the_hosts_cap_ends_the_observer() {
    let (root, _) = device_set("long-line");
    let mut ios = Ios::with_root(root.clone());
    // A line past the host's cap (the helper never prints one) isn't read whole: the feed stops
    // the helper, says why and follows the files.
    let (fake, observer) = FakeObserver::new();
    let mut feed2 = feed(&mut ios, || true, Some(observer));
    fake.say("ready");
    let huge = snap(0, &defaults(&[("blob", Plist::Data(vec![7; 1 << 20]))]));
    let writer = fake.say_from_a_thread(huge);
    until("the reader to stop the helper", || fake.stopped());
    assert!(feed2.notice("follows the files from now on").contains("longer than"), "{:?}", feed2.notices());
    feed2.quiet("nothing of the line past the cap is read");
    writer.join().expect("writer");
    feed2.close();
    until("the feed to end", || feed2.ended.load(SeqCst));
    std::fs::remove_dir_all(root).ok();
}

/// `prefswatch_test.m`, built for the host with the observer's own wrapping: a subclass's change
/// handler that calls `super` runs each class's original once instead of recursing.
#[cfg(target_os = "macos")]
#[test]
fn the_observers_wrapper_runs_each_class_handler_once() {
    let dir = std::env::temp_dir().join(format!("sim-api-prefswatch-{}", unique("wrap")));
    std::fs::create_dir_all(&dir).expect("dir");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/prefs/ios/prefswatch_test.m");
    let bin = dir.join("prefswatch_test");
    let build = std::process::Command::new("/usr/bin/xcrun")
        .args(["clang", "-fobjc-arc", "-framework", "Foundation", "-o"])
        .arg(&bin)
        .arg(&source)
        .output()
        .expect("xcrun");
    assert!(build.status.success(), "build: {}", String::from_utf8_lossy(&build.stderr));
    let run = std::process::Command::new(&bin).output().expect("run");
    let out = String::from_utf8_lossy(&run.stdout);
    assert!(run.status.success() && out == "ok\n", "{:?}: {out}", run.status);
    std::fs::remove_dir_all(dir).ok();
}
