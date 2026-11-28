//! App preferences on both platforms through one typed API, without an SDK in the app: an app's
//! stores (Android SharedPreferences XML files and DataStore preferences, iOS the app's domain, its
//! app-group suites and any named domain), every key with a typed value ([`PrefValue`]), writes
//! and deletes (one key or a batch), and a feed of changes.
//!
//! Each platform implements [`Backend`]; the methods here parse and check every parameter (types,
//! what a store can hold, a batch's keys) before a backend sees it. iOS devices are answered by
//! [`ios::Ios`] on a macOS host (docs/ios.md "App preferences"); until a platform's backend lands,
//! [`Unavailable`] answers its devices with a clear error.
//!
//! - A **batch** (`prefs_batch`) is all or nothing: every op is checked first, and the backend
//!   applies them as one write of the store or none at all. Its result has one visibility.
//! - A **write** says when the running app sees it ([`Visibility`]) and what was done to the app to
//!   get it there ([`AppState`]): a platform that can't reach a running app's in-memory copy says
//!   so instead of a silent no-op. On Android a running app writes its whole in-memory copy back on
//!   its next `apply`/`commit` (SharedPreferences) or update (single-process DataStore), over a
//!   write made under it, so a backend there stops the app, writes, and says whether it relaunched
//!   it, rather than promising `after_relaunch`.
//! - **`prefs_watch`** follows `logs`' `stream`: the first call starts one feed for the device, app
//!   and store in this (long-running) process and answers with a cursor; later calls with that
//!   cursor answer the changes since, waiting up to `wait_ms` for one. A feed's thread takes the
//!   backend's changes into a ring of at most 16 MiB as they come, waiting callers sleep on a
//!   condition variable (never holding the feed's lock, so callers don't hold each other up), and
//!   a feed nobody reads for 5 minutes stops on its own. A backend sends a feed's changes into a
//!   [`Sink`], which says when the feed has ended (`is_open`), so a backend's watcher stops then
//!   even if nothing changes. A backend's `watch` starts outside the lock on the process's feeds
//!   (it may take a moment: an iOS feed reads every store first), and at most [`MAX_FEEDS`] run
//!   at once.
//! - **`prefs_read {fresh: true}`** asks the backend for the values as the platform's preferences
//!   service has them now, where a plain read may see a file the service hasn't written yet (iOS:
//!   cfprefsd keeps a write in memory for up to about 10 s).

pub mod ios;
mod plist;
mod value;

#[cfg(test)]
pub(crate) mod fake;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_core::platform::Platform;

pub use value::{Date, PrefValue, Type};

use crate::params::{need_str, opt_bool};

/// The kinds of preference store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StoreKind {
    /// Android `shared_prefs/<name>.xml`.
    SharedPreferences,
    /// Android Jetpack DataStore preferences, `datastore/<name>.preferences_pb`.
    DataStore,
    /// iOS: the app's own domain (its bundle id).
    AppDomain,
    /// iOS: an app-group suite (`group.…`).
    AppGroup,
    /// iOS: any other named domain.
    Domain,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreKind::SharedPreferences => "shared_preferences",
            StoreKind::DataStore => "datastore",
            StoreKind::AppDomain => "app_domain",
            StoreKind::AppGroup => "app_group",
            StoreKind::Domain => "domain",
        }
    }

    pub fn platform(self) -> Platform {
        match self {
            StoreKind::SharedPreferences | StoreKind::DataStore => Platform::Android,
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => Platform::Ios,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StoreKind::SharedPreferences => "Android SharedPreferences",
            StoreKind::DataStore => "Android DataStore preferences",
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => "iOS user defaults",
        }
    }

    /// The types a store of this kind holds, at the top level and (iOS) nested. DataStore's
    /// `data` is `byteArrayPreferencesKey` (DataStore 1.1's `bytes`).
    pub fn types(self) -> &'static [Type] {
        use Type::*;
        match self {
            StoreKind::SharedPreferences => &[String, Int, Long, Float, Bool, StringSet],
            StoreKind::DataStore => &[String, Int, Long, Float, Double, Bool, StringSet, Data],
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => {
                &[String, Int, Long, Float, Double, Bool, Date, Data, Array, Dictionary]
            }
        }
    }

    /// Refuses a value (or a value nested in it) of a type this kind doesn't hold; `at` names it.
    pub fn check(self, value: &PrefValue, at: &str) -> Result<(), String> {
        if !self.types().contains(&value.ty()) {
            let names: Vec<&str> = self.types().iter().map(|t| t.as_str()).collect();
            return Err(format!(
                "`{at}` is a `{}`, which {} can't hold (it holds {})",
                value.ty().as_str(),
                self.label(),
                names.join(", ")
            ));
        }
        value.children().try_for_each(|child| self.check(child, &format!("{at} (nested)")))
    }
}

/// One preference store of an app.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Store {
    pub kind: StoreKind,
    /// The app's package name or bundle id (`bundle_id` on the wire).
    pub app: String,
    /// Android: the file's name without its directory and extension; iOS: the domain.
    pub name: String,
}

impl Store {
    pub fn new(kind: StoreKind, app: &str, name: &str) -> Store {
        Store { kind, app: app.to_owned(), name: name.to_owned() }
    }

    /// The id a call names the store by: Android the file under the app's data directory
    /// (`shared_prefs/settings.xml`, `datastore/settings.preferences_pb`), iOS the domain.
    pub fn id(&self) -> String {
        match self.kind {
            StoreKind::SharedPreferences => format!("shared_prefs/{}.xml", self.name),
            StoreKind::DataStore => format!("datastore/{}.preferences_pb", self.name),
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => self.name.clone(),
        }
    }

    /// The store `id` names for `app` on a `platform` device. An iOS id is the app's domain when
    /// it is the app's bundle id, an app-group suite when it starts `group.`, else a named domain.
    pub fn parse(platform: Platform, app: &str, id: &str) -> Result<Store, String> {
        let plain = |name: &str| !name.is_empty() && !name.contains('/') && name != "." && name != "..";
        match platform {
            Platform::Android => {
                let shared = id.strip_prefix("shared_prefs/").and_then(|r| r.strip_suffix(".xml")).filter(|n| plain(n));
                let datastore = id.strip_prefix("datastore/").and_then(|r| r.strip_suffix(".preferences_pb")).filter(|n| plain(n));
                match (shared, datastore) {
                    (Some(name), _) => Ok(Store::new(StoreKind::SharedPreferences, app, name)),
                    (_, Some(name)) => Ok(Store::new(StoreKind::DataStore, app, name)),
                    _ => Err(format!(
                        "`{id}` isn't an Android preferences store: name one as `shared_prefs/<name>.xml` or \
                         `datastore/<name>.preferences_pb` (prefs_stores lists the app's)"
                    )),
                }
            }
            Platform::Ios if !plain(id) => Err(format!("`{id}` isn't an iOS defaults domain: a domain is a name, not a path")),
            Platform::Ios if id == app => Ok(Store::new(StoreKind::AppDomain, app, id)),
            Platform::Ios if id.starts_with("group.") => Ok(Store::new(StoreKind::AppGroup, app, id)),
            Platform::Ios => Ok(Store::new(StoreKind::Domain, app, id)),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"id": self.id(), "kind": self.kind.as_str(), "name": self.name, "bundle_id": self.app, "platform": self.kind.platform().as_str()})
    }
}

/// Every key of a store with its value.
pub type Entries = BTreeMap<String, PrefValue>;

pub fn entries_json(entries: &Entries) -> Value {
    Value::Object(entries.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
}

/// One change a write makes.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Op {
    Set { key: String, value: PrefValue },
    Delete { key: String },
}

impl Op {
    pub fn key(&self) -> &str {
        match self {
            Op::Set { key, .. } | Op::Delete { key } => key,
        }
    }
}

/// When the app sees a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Visibility {
    /// The app sees it now: it reached a running app, or the app wasn't running (or was stopped
    /// for the write, [`AppState`]) and reads it when it starts.
    Now,
    /// The app sees it once it is relaunched; until then a running copy keeps its old values in
    /// memory. Only for a platform whose running app never writes that copy back over the store
    /// (not Android: see [`AppState`]).
    AfterRelaunch,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Now => "now",
            Visibility::AfterRelaunch => "after_relaunch",
        }
    }
}

/// What a write did to the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppState {
    /// Nothing: it wasn't running, or the write reached it where it runs.
    Untouched,
    /// It was running and was stopped for the write (a running Android app would write its old
    /// values back over it); it stays stopped.
    Stopped,
    /// It was stopped for the write and launched again afterwards.
    Relaunched,
}

impl AppState {
    pub fn as_str(self) -> &'static str {
        match self {
            AppState::Untouched => "untouched",
            AppState::Stopped => "stopped",
            AppState::Relaunched => "relaunched",
        }
    }
}

/// What a write (or a batch) did.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteResult {
    pub visible: Visibility,
    pub app: AppState,
    /// Why, when the app sees it only after a relaunch or was stopped for it, and what to do.
    pub note: Option<String>,
}

/// One change to a key, as a feed reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub store: Store,
    pub key: String,
    /// `None`: the key was added.
    pub old: Option<PrefValue>,
    /// `None`: the key was removed.
    pub new: Option<PrefValue>,
    pub at: Date,
}

impl Change {
    /// About how many bytes it takes, for a feed's budget.
    pub fn size(&self) -> usize {
        64 + self.store.name.len()
            + self.store.app.len()
            + self.key.len()
            + self.old.as_ref().map_or(0, PrefValue::size)
            + self.new.as_ref().map_or(0, PrefValue::size)
    }

    pub fn to_json(&self) -> Value {
        let typed = |v: &Option<PrefValue>| v.as_ref().map_or(Value::Null, PrefValue::to_json);
        json!({"store": self.store.id(), "key": self.key, "old": typed(&self.old), "new": typed(&self.new), "at": self.at.to_iso8601()})
    }
}

/// The changes from `before` to `after`, by key: for a backend that sees a store's whole contents
/// before and after (a file it re-reads).
pub fn changes(store: &Store, before: &Entries, after: &Entries, at: Date) -> Vec<Change> {
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|k| Change { store: store.clone(), key: k.clone(), old: before.get(k).cloned(), new: after.get(k).cloned(), at })
        .collect()
}

/// One platform's preferences. `device` is the device's id as the dispatcher resolved it (a
/// simulator's UDID, an AVD's name). Every parameter has been checked before a backend sees it:
/// a store is the device's platform's, each value is of a type its store holds, a batch names
/// each key once.
pub trait Backend {
    /// The app's stores.
    fn stores(&mut self, device: &str, app: &str) -> Result<Vec<Store>, String>;

    /// Every key of `store` with its value.
    fn read(&mut self, device: &str, store: &Store) -> Result<Entries, String>;

    /// Every key of `store` with its value as the platform's preferences service has it now
    /// (`prefs_read {fresh: true}`): for a backend whose plain read may see a file the service
    /// hasn't written yet. Otherwise a plain read.
    fn read_fresh(&mut self, device: &str, store: &Store) -> Result<Entries, String> {
        self.read(device, store)
    }

    /// Applies `ops` in order, all or nothing: one write of the store, or an error and no change.
    fn write_batch(&mut self, device: &str, store: &Store, ops: &[Op]) -> Result<WriteResult, String>;

    fn write(&mut self, device: &str, store: &Store, key: &str, value: &PrefValue) -> Result<WriteResult, String> {
        self.write_batch(device, store, &[Op::Set { key: key.to_owned(), value: value.clone() }])
    }

    fn delete(&mut self, device: &str, store: &Store, key: &str) -> Result<WriteResult, String> {
        self.write_batch(device, store, &[Op::Delete { key: key.to_owned() }])
    }

    /// Starts sending the changes to `store` (every store of the app when `None`) into `sink`,
    /// from whoever makes them (the app, this API, another tool), in the order they happen. The
    /// backend stops when the feed ends (`sink.is_open()` turns false, or a send fails), and drops
    /// the sink when the device stops, which ends the feed.
    fn watch(&mut self, device: &str, app: &str, store: Option<&Store>, sink: Sink) -> Result<(), String>;
}

/// Where a backend sends a feed's changes.
pub struct Sink {
    tx: Sender<Change>,
    open: Arc<AtomicBool>,
}

impl Sink {
    /// Sends one change; false once the feed has ended (stop watching then).
    pub fn send(&self, change: Change) -> bool {
        self.is_open() && self.tx.send(change).is_ok()
    }

    /// Whether the feed still takes changes. A backend that waits for changes checks it as it
    /// wakes, so it stops when the feed ends even if nothing changes.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}

/// A feed's end of a [`Sink`]: dropping it (the feed's thread ending) closes the sink.
struct Source {
    rx: Receiver<Change>,
    open: Arc<AtomicBool>,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.open.store(false, Ordering::Release);
    }
}

/// A connected sink and source.
fn sink() -> (Sink, Source) {
    let (tx, rx) = std::sync::mpsc::channel();
    let open = Arc::new(AtomicBool::new(true));
    (Sink { tx, open: open.clone() }, Source { rx, open })
}

/// The backend of a platform whose preferences haven't landed yet: every call says so.
pub struct Unavailable(pub Platform);

impl Unavailable {
    fn refuse<T>(&self) -> Result<T, String> {
        Err(format!("app preferences (`prefs_*`) are not yet available on {} devices", self.0.label()))
    }
}

impl Backend for Unavailable {
    fn stores(&mut self, _: &str, _: &str) -> Result<Vec<Store>, String> {
        self.refuse()
    }

    fn read(&mut self, _: &str, _: &Store) -> Result<Entries, String> {
        self.refuse()
    }

    fn write_batch(&mut self, _: &str, _: &Store, _: &[Op]) -> Result<WriteResult, String> {
        self.refuse()
    }

    fn watch(&mut self, _: &str, _: &str, _: Option<&Store>, _: Sink) -> Result<(), String> {
        self.refuse()
    }
}

/// Each platform's backend, per connection.
pub struct Session {
    pub(crate) ios: Box<dyn Backend>,
    pub(crate) android: Box<dyn Backend>,
    /// The iOS backend of each other device set a call has named (#409), by its directory.
    #[cfg(target_os = "macos")]
    sets: HashMap<String, Box<dyn Backend>>,
}

impl Default for Session {
    fn default() -> Session {
        #[cfg(target_os = "macos")]
        let ios: Box<dyn Backend> = Box::new(ios::Ios::default());
        #[cfg(not(target_os = "macos"))]
        let ios: Box<dyn Backend> = Box::new(Unavailable(Platform::Ios));
        Session {
            ios,
            android: Box::new(Unavailable(Platform::Android)),
            #[cfg(target_os = "macos")]
            sets: HashMap::new(),
        }
    }
}

impl Session {
    /// The backend for a device of `platform` in the iOS device set `ios`'s call uses (#409): a
    /// set of its own keeps its devices' preference files in its own directory, and connects to
    /// the simulator service on that set.
    pub(crate) fn backend_on(&mut self, platform: Platform, ios: &crate::ios::Session) -> Result<&mut dyn Backend, String> {
        #[cfg(target_os = "macos")]
        if platform == Platform::Ios
            && let Some(set) = ios.set()?
        {
            let backend = self.sets.entry(set.clone()).or_insert_with(|| Box::new(ios::Ios::in_set(set, ios.opener())));
            return Ok(&mut **backend);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = ios;
        Ok(self.backend(platform))
    }

    pub(crate) fn backend(&mut self, platform: Platform) -> &mut dyn Backend {
        match platform {
            Platform::Ios => &mut *self.ios,
            Platform::Android => &mut *self.android,
        }
    }
}

/// The preference methods.
pub(crate) const METHODS: &[&str] = &["prefs_stores", "prefs_read", "prefs_write", "prefs_delete", "prefs_batch", "prefs_watch"];

pub(crate) fn is_method(method: &str) -> bool {
    METHODS.contains(&method)
}

/// The longest `prefs_watch` waits for a change.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// An optional string parameter: absent or null is `None`, any other non-string is refused.
fn opt_string<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(format!("`{key}` must be a string, not {other}")),
    }
}

/// `value` and `type` of a write (both required), read as a typed value for `store`.
fn typed_value(store: &Store, ty: &str, value: &Value, at: &str) -> Result<PrefValue, String> {
    let value = PrefValue::from_json(Type::parse(ty)?, value, at)?;
    store.kind.check(&value, at)?;
    Ok(value)
}

fn write_json(store: &Store, keys: Value, done: &WriteResult) -> Value {
    let mut out = json!({"store": store.id(), "visible": done.visible.as_str(), "app": done.app.as_str()});
    match keys {
        Value::String(key) => out["key"] = Value::String(key),
        keys => out["keys"] = keys,
    }
    if let Some(note) = &done.note {
        out["note"] = Value::String(note.clone());
    }
    out
}

/// A batch's `ops`: `{key, type, value}` sets and `{key, delete: true}` deletes, each key once.
fn batch_ops(store: &Store, ops: &[Value]) -> Result<Vec<Op>, String> {
    if ops.is_empty() {
        return Err("`ops` is empty: give at least one {key, type, value} or {key, delete: true}".into());
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let at = format!("ops[{i}]");
        let Some(obj) = op.as_object() else {
            return Err(format!("`{at}` must be {{key, type, value}} or {{key, delete: true}}, not {op}"));
        };
        let Some(key) = obj.get("key").and_then(Value::as_str) else { return Err(format!("`{at}` has no string `key`")) };
        if let Some(extra) = obj.keys().find(|k| !["key", "type", "value", "delete"].contains(&k.as_str())) {
            return Err(format!("`{at}` has `{extra}`; an op is {{key, type, value}} or {{key, delete: true}}"));
        }
        let delete = match obj.get("delete") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(other) => return Err(format!("`{at}.delete` must be true or false, not {other}")),
        };
        let op = match (delete, obj.get("type"), obj.get("value")) {
            (true, None, None) => Op::Delete { key: key.to_owned() },
            (true, ..) => return Err(format!("`{at}` deletes `{key}`, so it takes no `type` or `value`")),
            (false, Some(Value::String(ty)), Some(value)) => {
                Op::Set { key: key.to_owned(), value: typed_value(store, ty, value, &format!("{at}.value"))? }
            }
            (false, ..) => return Err(format!("`{at}` sets `{key}`: give a string `type` and a `value` (or `delete: true`)")),
        };
        if !seen.insert(key.to_owned()) {
            return Err(format!("`ops` names `{key}` twice; a batch changes each key once"));
        }
        out.push(op);
    }
    Ok(out)
}

/// Runs one preference method on the `platform` device `device` (resolved by the dispatcher).
pub(crate) fn call(backend: &mut dyn Backend, platform: Platform, device: &str, method: &str, p: &Value) -> Result<Value, String> {
    let app = need_str(p, "bundle_id")?;
    // Every parameter's presence is checked before any is read, so a call missing one says so.
    match method {
        "prefs_stores" => Ok(backend.stores(device, app)?.iter().map(Store::to_json).collect()),
        "prefs_read" => {
            // One store: its values or the call's error. Every store: each one's values or its own
            // error, so one unreadable store doesn't hide the rest.
            let fresh = opt_bool(p, "fresh")?.unwrap_or(false);
            let read = |backend: &mut dyn Backend, store: &Store| {
                if fresh { backend.read_fresh(device, store) } else { backend.read(device, store) }
            };
            if let Some(id) = opt_string(p, "store")? {
                let store = Store::parse(platform, app, id)?;
                let entries = read(backend, &store)?;
                return Ok(json!([{"store": store.to_json(), "values": entries_json(&entries)}]));
            }
            let stores = backend.stores(device, app)?;
            Ok(stores
                .iter()
                .map(|store| match read(backend, store) {
                    Ok(entries) => json!({"store": store.to_json(), "values": entries_json(&entries)}),
                    Err(error) => json!({"store": store.to_json(), "error": error}),
                })
                .collect())
        }
        "prefs_write" => {
            let (id, key, ty) = (need_str(p, "store")?, need_str(p, "key")?, need_str(p, "type")?);
            let value = p.get("value").filter(|v| !v.is_null()).ok_or("missing parameter `value`")?;
            let store = Store::parse(platform, app, id)?;
            let value = typed_value(&store, ty, value, "value")?;
            let done = backend.write(device, &store, key, &value)?;
            Ok(write_json(&store, Value::String(key.to_owned()), &done))
        }
        "prefs_delete" => {
            let (id, key) = (need_str(p, "store")?, need_str(p, "key")?);
            let store = Store::parse(platform, app, id)?;
            let done = backend.delete(device, &store, key)?;
            Ok(write_json(&store, Value::String(key.to_owned()), &done))
        }
        "prefs_batch" => {
            let id = need_str(p, "store")?;
            let ops = match p.get("ops") {
                None | Some(Value::Null) => return Err("missing array parameter `ops`".into()),
                Some(Value::Array(ops)) => ops,
                Some(other) => return Err(format!("`ops` must be an array of {{key, type, value}} or {{key, delete: true}}, not {other}")),
            };
            let store = Store::parse(platform, app, id)?;
            let ops = batch_ops(&store, ops)?;
            let done = backend.write_batch(device, &store, &ops)?;
            Ok(write_json(&store, ops.iter().map(|op| Value::String(op.key().to_owned())).collect(), &done))
        }
        "prefs_watch" => {
            let store = opt_string(p, "store")?.map(|id| Store::parse(platform, app, id)).transpose()?;
            let cursor = opt_string(p, "cursor")?;
            let feed_id = cursor.and_then(parse_cursor).map(|(id, _)| id);
            let wait = match p.get("wait_ms") {
                None | Some(Value::Null) => Duration::ZERO,
                Some(v) => {
                    Duration::from_millis(v.as_u64().ok_or_else(|| format!("`wait_ms` must be a whole number of milliseconds, not {v}"))?)
                }
            };
            let key = FeedKey { platform, device: device.to_owned(), app: app.to_owned(), store: store.as_ref().map(Store::id) };
            let feed = feed_for(&FEEDS, key, feed_id, |sink| backend.watch(device, app, store.as_ref(), sink))?;
            Ok(feed.poll(cursor, wait.min(MAX_WAIT)))
        }
        other => Err(format!("`{other}` isn't a preferences method")),
    }
}

/// How long a feed nobody reads keeps running.
const FEED_IDLE: Duration = Duration::from_secs(300);

/// The bytes of changes a feed keeps for callers that are behind (about [`Change::size`] each,
/// as `logs` keeps 16 MiB of lines); the oldest go first and are counted as `missed`.
const FEED_BYTES: usize = 16 << 20;

/// The most feeds one process runs at once (each has a thread, and an iOS one a watcher thread
/// and a few descriptors); a call that would start another is refused until one ends.
pub const MAX_FEEDS: usize = 64;

static FEEDS: Mutex<Option<Feeds>> = Mutex::new(None);
static NEXT_FEED: AtomicU64 = AtomicU64::new(1);

fn with_feeds<T>(feeds: &Mutex<Option<Feeds>>, f: impl FnOnce(&mut Feeds) -> T) -> T {
    let mut guard = feeds.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(|| Feeds::new(FEED_BYTES, FEED_IDLE, MAX_FEEDS)))
}

/// The feed for `key` in `feeds`, started with `start` when there is none ([`Feeds::find`]).
/// `start` (the backend's `watch`) runs without the lock, so a backend that takes a moment to
/// start doesn't hold up every other feed's callers; when two calls start the same feed at once,
/// the first one in is kept and the other's sink closes.
fn feed_for(
    feeds: &Mutex<Option<Feeds>>,
    key: FeedKey,
    id: Option<u64>,
    start: impl FnOnce(Sink) -> Result<(), String>,
) -> Result<Arc<Feed>, String> {
    if let Some(feed) = with_feeds(feeds, |f| f.find(&key, id))? {
        return Ok(feed);
    }
    let (sink, source) = sink();
    start(sink)?;
    with_feeds(feeds, |f| f.insert(key, id, source))
}

/// What a feed follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FeedKey {
    platform: Platform,
    device: String,
    app: String,
    /// `None`: every store of the app.
    store: Option<String>,
}

/// The feeds of this process, one per [`FeedKey`].
struct Feeds {
    feeds: HashMap<FeedKey, Arc<Feed>>,
    capacity: usize,
    idle: Duration,
    /// The most feeds running (not ended) at once.
    most: usize,
}

impl Feeds {
    fn new(capacity: usize, idle: Duration, most: usize) -> Feeds {
        Feeds { feeds: HashMap::new(), capacity, idle, most }
    }

    /// The feed for `key` a caller gets: a running one, or an ended one when the caller holds
    /// its cursor (`id`: it reads what the feed had and that it ended). `None`: start one, which
    /// is refused when `most` are running. Ended feeds nobody has read for `idle` are let go.
    fn find(&mut self, key: &FeedKey, id: Option<u64>) -> Result<Option<Arc<Feed>>, String> {
        let idle = self.idle;
        self.feeds.retain(|_, feed| {
            let state = feed.state();
            !state.ended || state.used.elapsed() < idle
        });
        if let Some(feed) = self.feeds.get(key) {
            let mut state = feed.state();
            if !state.ended || id == Some(feed.id) {
                // Used now, under its lock: the idle stop can't end it between this lookup and
                // the caller's poll (a first call would otherwise get `ended` and retry).
                state.used = Instant::now();
                drop(state);
                return Ok(Some(feed.clone()));
            }
        }
        let running = self.feeds.values().filter(|feed| !feed.state().ended).count();
        if running >= self.most {
            return Err(format!(
                "{running} preference feeds are running in this process, the most it runs: poll one with its cursor, or let one \
                 end (a feed nobody reads stops after 5 minutes)"
            ));
        }
        Ok(None)
    }

    /// Adds the feed a backend started for `key` (its `source`), unless another call added one
    /// while it started: then that one is the caller's, and `source` is dropped (its sink closes).
    fn insert(&mut self, key: FeedKey, id: Option<u64>, source: Source) -> Result<Arc<Feed>, String> {
        if let Some(feed) = self.find(&key, id)? {
            return Ok(feed);
        }
        let feed = Feed::start(source, self.capacity, self.idle)?;
        self.feeds.insert(key, feed.clone());
        Ok(feed)
    }
}

/// One feed: its thread takes the backend's changes into `state` as they come; callers read
/// them from there, and wait on `changed`, which never holds the lock while it waits.
struct Feed {
    id: u64,
    state: Mutex<FeedState>,
    changed: Condvar,
}

struct FeedState {
    /// Each kept change with its sequence number (1, 2, ...) and size, oldest first.
    events: VecDeque<(u64, usize, Change)>,
    bytes: usize,
    capacity: usize,
    /// The sequence number of the last change received.
    head: u64,
    used: Instant,
    /// Callers waiting for a change: a feed being waited on isn't idle.
    waiting: usize,
    /// The backend closed it (the device stopped) or nobody read it for the idle time.
    ended: bool,
}

impl FeedState {
    fn push(&mut self, change: Change) {
        let size = change.size();
        self.head += 1;
        self.bytes += size;
        self.events.push_back((self.head, size, change));
        while self.bytes > self.capacity {
            let Some((_, size, _)) = self.events.pop_front() else { break };
            self.bytes -= size;
        }
    }
}

impl Feed {
    fn start(source: Source, capacity: usize, idle: Duration) -> Result<Arc<Feed>, String> {
        let state = FeedState { events: VecDeque::new(), bytes: 0, capacity, head: 0, used: Instant::now(), waiting: 0, ended: false };
        let feed = Arc::new(Feed { id: NEXT_FEED.fetch_add(1, Ordering::Relaxed), state: Mutex::new(state), changed: Condvar::new() });
        let reading = feed.clone();
        std::thread::Builder::new()
            .name("simee-prefs-feed".into())
            // The source drops when the thread ends, which closes the backend's sink.
            .spawn(move || reading.run(&source.rx, idle))
            .map_err(|e| format!("can't start a preferences feed: {e}"))?;
        Ok(feed)
    }

    fn state(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The feed's thread: takes each change as the backend sends it, and ends the feed when the
    /// backend closes it or nobody has read (or waited on) it for `idle`. It wakes at least every
    /// `tick` for that, and returning drops the source, which closes the backend's sink.
    fn run(&self, rx: &Receiver<Change>, idle: Duration) {
        let tick = (idle / 4).clamp(Duration::from_millis(5), Duration::from_secs(1));
        loop {
            let got = rx.recv_timeout(tick);
            let mut state = self.state();
            if state.ended {
                return;
            }
            let mut wake = true;
            match got {
                Ok(change) => state.push(change),
                Err(RecvTimeoutError::Disconnected) => state.ended = true,
                Err(RecvTimeoutError::Timeout) => wake = false,
            }
            if !state.ended && state.waiting == 0 && state.used.elapsed() >= idle {
                // Nobody is reading: what it kept goes too, as an idle `logs` stream's lines do.
                state.ended = true;
                state.events.clear();
                state.bytes = 0;
                wake = true;
            }
            let ended = state.ended;
            drop(state);
            if wake {
                self.changed.notify_all();
            }
            if ended {
                return;
            }
        }
    }

    /// The answer to one `prefs_watch` call.
    ///
    /// - No cursor: the changes from now on are this caller's; `{cursor, events: []}`.
    /// - A cursor of this feed: the changes after it, waiting up to `wait` for one;
    ///   `{cursor, events, missed?}`, `missed` counting changes dropped before they were read.
    /// - A cursor of another feed (one that has gone): `reset: true` and a new cursor; changes in
    ///   between may be lost, so re-read the store.
    ///
    /// `ended: true` says the feed has ended (the device stopped, or nobody read it for 5
    /// minutes): a call without a cursor starts a new one.
    fn poll(&self, cursor: Option<&str>, wait: Duration) -> Value {
        let mut state = self.state();
        state.used = Instant::now();
        let answer = |state: &FeedState, events: Vec<Value>| {
            let mut out = json!({"cursor": format!("{}:{}", self.id, state.head), "events": events});
            if state.ended {
                out["ended"] = true.into();
            }
            out
        };
        let after = match cursor.map(parse_cursor) {
            None => return answer(&state, Vec::new()),
            Some(Some((id, seq))) if id == self.id && seq <= state.head => seq,
            Some(_) => {
                let mut out = answer(&state, Vec::new());
                out["reset"] = true.into();
                return out;
            }
        };
        if state.head == after && !state.ended && !wait.is_zero() {
            let deadline = Instant::now() + wait;
            state.waiting += 1;
            while state.head == after && !state.ended {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                state = self.changed.wait_timeout(state, left).unwrap_or_else(PoisonError::into_inner).0;
            }
            state.waiting -= 1;
            state.used = Instant::now();
        }
        let oldest = state.events.front().map_or(state.head + 1, |(seq, ..)| *seq);
        let events = state.events.iter().filter(|(seq, ..)| *seq > after).map(|(.., c)| c.to_json()).collect();
        let mut out = answer(&state, events);
        if oldest > after + 1 {
            out["missed"] = (oldest - after - 1).into();
        }
        out
    }
}

/// `<feed>:<seq>`.
fn parse_cursor(cursor: &str) -> Option<(u64, u64)> {
    let (id, seq) = cursor.split_once(':')?;
    Some((id.parse().ok()?, seq.parse().ok()?))
}

#[cfg(test)]
mod tests;
