//! iOS user defaults: an app's domain, its app-group suites and its other domains, read from the
//! simulator's files on the host, written through the device's cfprefsd, and followed by watching
//! the files (docs/ios.md "App preferences").
//!
//! - **Where they live.** An app's defaults are `Library/Preferences/<domain>.plist` in its data
//!   container (`data/Containers/Data/Application/<uuid>`); an app-group suite's are in the
//!   group's container (`data/Containers/Shared/AppGroup/<uuid>`) when the app is entitled to the
//!   group, else in the app's own (where cfprefsd then keeps them for that app). A container says
//!   whose it is in its `.com.apple.mobile_container_manager.metadata.plist`; an app's groups are
//!   in its executable's entitlements (the simulator's `__entitlements` section, or the code
//!   signature's). All of it is read and decoded in process: no `plutil`, no spawn.
//! - **Reads** decode the binary plist cfprefsd wrote. cfprefsd keeps a write in memory and
//!   writes the file later (about 1 to 10 s on iOS 27.2), so a read can be behind the app;
//!   `fresh: true` asks cfprefsd instead (one in-device call).
//! - **Writes** go through cfprefsd with `native/prefstool.m`, one spawn per batch, so a running
//!   app's `UserDefaults` sees them at once (its KVO observers fire; `didChangeNotification` does
//!   not, as for any change made by another process).
//! - **The feed** watches the store files and their directories with kqueue
//!   ([`sim_coresim::FileWatch`]) and reports each key whose decoded value changed, so a change
//!   shows when cfprefsd writes the file.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sim_core::platform::Platform;

use super::plist::{self, Plist};
use super::{Change, Date, Entries, Store, StoreKind};

/// A container's metadata file.
const METADATA: &str = ".com.apple.mobile_container_manager.metadata.plist";
/// The entitlement that lists an app's groups.
const GROUPS_ENTITLEMENT: &str = "com.apple.security.application-groups";

/// The directories of the default device set: `<root>/<UDID>/data` is a device's filesystem.
pub fn default_devices_root() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    home.join("Library/Developer/CoreSimulator/Devices")
}

/// The identifier a container's metadata names, read without a spawn.
fn container_id(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(METADATA)).ok()?;
    plist::decode(&bytes).ok()?.get("MCMMetadataIdentifier")?.as_str().map(str::to_owned)
}

/// The containers of one kind on one device, by identifier, found once and checked on each use
/// (an app reinstalled gets a new container).
#[derive(Default)]
struct Containers {
    by_id: HashMap<String, PathBuf>,
}

impl Containers {
    /// The container of `id` under `base` (one of `data/Containers/...`).
    fn find(&mut self, base: &Path, id: &str) -> Option<PathBuf> {
        if let Some(dir) = self.by_id.get(id)
            && container_id(dir).as_deref() == Some(id)
        {
            return Some(dir.clone());
        }
        self.by_id = std::fs::read_dir(base)
            .map(|it| it.flatten().filter_map(|e| Some((container_id(&e.path())?, e.path()))).collect())
            .unwrap_or_default();
        self.by_id.get(id).cloned()
    }
}

/// The app groups an executable is entitled to: the strings of each
/// `com.apple.security.application-groups` array in its entitlements (XML, wherever it is).
pub fn entitled_groups(executable: &[u8]) -> Vec<String> {
    let key = format!("<key>{GROUPS_ENTITLEMENT}</key>");
    let mut groups = Vec::new();
    let mut rest = executable;
    while let Some(at) = find(rest, key.as_bytes()) {
        rest = &rest[at + key.len()..];
        let body = trim_start(rest);
        let Some(mut list) = body.strip_prefix(b"<array>") else { continue };
        loop {
            list = trim_start(list);
            let Some(after) = list.strip_prefix(b"<string>") else { break };
            let Some(end) = find(after, b"</string>") else { break };
            if let Ok(group) = std::str::from_utf8(&after[..end])
                && !groups.iter().any(|g| g == group)
            {
                groups.push(group.to_owned());
            }
            list = &after[end + b"</string>".len()..];
        }
    }
    groups.sort();
    groups
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trim_start(bytes: &[u8]) -> &[u8] {
    let n = bytes.iter().take_while(|b| b.is_ascii_whitespace()).count();
    &bytes[n..]
}

/// The executable of the app bundle in `bundle_container` (its `*.app`'s `CFBundleExecutable`).
fn executable(bundle_container: &Path) -> Option<PathBuf> {
    let app = std::fs::read_dir(bundle_container).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "app"))?;
    let info = std::fs::read(app.join("Info.plist")).ok();
    let named = info.and_then(|bytes| {
        if plist::is_binary(&bytes) {
            plist::decode(&bytes).ok()?.get("CFBundleExecutable")?.as_str().map(str::to_owned)
        } else {
            // An XML Info.plist: the string after the key.
            let text = String::from_utf8(bytes).ok()?;
            let after = text.split_once("<key>CFBundleExecutable</key>")?.1.trim_start();
            Some(after.strip_prefix("<string>")?.split_once("</string>")?.0.to_owned())
        }
    });
    let name = named.or_else(|| Some(app.file_stem()?.to_string_lossy().into_owned()))?;
    Some(app.join(name))
}

/// One device's files: where each app's and group's defaults live.
#[derive(Default)]
struct Device {
    apps: Containers,
    groups: Containers,
    bundles: Containers,
    /// Each app's groups, by its executable's path, size and modification time.
    entitled: HashMap<String, (PathBuf, u64, i64, Vec<String>)>,
}

impl Device {
    fn app_container(&mut self, data: &Path, app: &str) -> Result<PathBuf, String> {
        self.apps
            .find(&data.join("Containers/Data/Application"), app)
            .ok_or_else(|| format!("`{app}` has no data container on this device: is it installed?"))
    }

    /// The groups `app` is entitled to (none when its bundle or executable can't be read).
    fn groups_of(&mut self, data: &Path, app: &str) -> Vec<String> {
        let Some(bundle) = self.bundles.find(&data.join("Containers/Bundle/Application"), app) else { return Vec::new() };
        let Some(exe) = executable(&bundle) else { return Vec::new() };
        let Ok(meta) = std::fs::metadata(&exe) else { return Vec::new() };
        if let Some((path, size, mtime, groups)) = self.entitled.get(app)
            && *path == exe
            && *size == meta.size()
            && *mtime == meta.mtime()
        {
            return groups.clone();
        }
        let groups = std::fs::read(&exe).map(|bytes| entitled_groups(&bytes)).unwrap_or_default();
        self.entitled.insert(app.to_owned(), (exe, meta.size(), meta.mtime(), groups.clone()));
        groups
    }

    /// The container whose `Library/Preferences` holds `store`, as cfprefsd picks it for the
    /// store's app: a group's own when the app is entitled to it and it exists, else the app's.
    fn container(&mut self, data: &Path, store: &Store) -> Result<PathBuf, String> {
        if store.kind == StoreKind::AppGroup
            && self.groups_of(data, &store.app).contains(&store.name)
            && let Some(dir) = self.groups.find(&data.join("Containers/Shared/AppGroup"), &store.name)
        {
            return Ok(dir);
        }
        self.app_container(data, &store.app)
    }

    /// The app's stores: its domain, the groups it is entitled to, and every other domain with
    /// a file in its container.
    fn stores(&mut self, data: &Path, app: &str) -> Result<Vec<Store>, String> {
        let container = self.app_container(data, app)?;
        let mut stores = vec![Store::new(StoreKind::AppDomain, app, app)];
        stores.extend(self.groups_of(data, app).iter().map(|g| Store::new(StoreKind::AppGroup, app, g)));
        for name in domains_in(&container.join("Library/Preferences")) {
            let store = Store::parse(Platform::Ios, app, &name)?;
            if !stores.contains(&store) {
                stores.push(store);
            }
        }
        Ok(stores)
    }
}

/// The domains with a defaults file in a `Library/Preferences` directory, sorted.
fn domains_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter_map(|e| e.file_name().to_str()?.strip_suffix(".plist").map(str::to_owned))
                .filter(|n| !n.is_empty() && !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A store's defaults file in `container`.
fn store_file(container: &Path, store: &Store) -> PathBuf {
    container.join("Library/Preferences").join(format!("{}.plist", store.name))
}

/// The keys of a defaults file: none when it doesn't exist (nothing written yet).
pub fn read_file(path: &Path) -> Result<Entries, String> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(Entries::new()),
        Ok(bytes) => plist::entries(&bytes).map_err(|e| format!("{}: {e} (`fresh: true` reads it through cfprefsd)", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Entries::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// What tells a file was replaced or written: inode, size and modification time.
type Stamp = (u64, u64, i64, i64);

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.ino(), m.size(), m.mtime(), m.mtime_nsec()))
}

/// A feed's view of the stores it follows: each one's file with what was last read from it.
pub struct Tracker {
    /// Every store of the app (new domains are picked up as their files appear), or one.
    all: bool,
    app: String,
    /// The app's container's `Library/Preferences`, where new domains appear.
    app_prefs: PathBuf,
    stores: Vec<Tracked>,
}

struct Tracked {
    store: Store,
    file: PathBuf,
    stamp: Option<Stamp>,
    entries: Entries,
}

impl Tracker {
    /// Follows `stores` of `app` (whose own defaults are in `app_prefs`), reading each now.
    fn new(app: &str, app_prefs: PathBuf, all: bool, stores: Vec<(Store, PathBuf)>) -> Tracker {
        let stores = stores
            .into_iter()
            .map(|(store, file)| {
                let stamp = stamp(&file);
                let entries = read_file(&file).unwrap_or_default();
                Tracked { store, file, stamp, entries }
            })
            .collect();
        Tracker { all, app: app.to_owned(), app_prefs, stores }
    }

    /// The paths to watch: each store's file and the directories they are in.
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = Vec::new();
        let dirs = self.stores.iter().filter_map(|t| t.file.parent().map(Path::to_path_buf));
        for p in dirs.chain(self.all.then(|| self.app_prefs.clone())).chain(self.stores.iter().map(|t| t.file.clone())) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        paths
    }

    /// The changes since the last look, `at` now: each store whose file changed is read again and
    /// compared by key (a file being replaced as it is read is read on the next look); a domain
    /// whose file appeared in the app's container is followed from empty. Whether it follows
    /// more stores than before (more paths to watch).
    pub fn poll(&mut self, at: Date) -> (Vec<Change>, bool) {
        let mut grew = false;
        if self.all {
            for name in domains_in(&self.app_prefs) {
                let Ok(store) = Store::parse(Platform::Ios, &self.app, &name) else { continue };
                let file = self.app_prefs.join(format!("{name}.plist"));
                if !self.stores.iter().any(|t| t.store == store || t.file == file) {
                    self.stores.push(Tracked { store, file, stamp: None, entries: Entries::new() });
                    grew = true;
                }
            }
        }
        let mut changes = Vec::new();
        for tracked in &mut self.stores {
            let now = stamp(&tracked.file);
            if now == tracked.stamp {
                continue;
            }
            let Ok(entries) = read_file(&tracked.file) else { continue };
            changes.extend(super::changes(&tracked.store, &tracked.entries, &entries, at));
            tracked.entries = entries;
            tracked.stamp = now;
        }
        (changes, grew)
    }
}

/// Whether the device whose directory is `device_dir` is booted, from the `state` in its
/// `device.plist` (an XML plist with a `<date>` in it, or a binary one). A device that is gone
/// isn't; one whose state can't be read is taken as booted.
pub fn booted(device_dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(device_dir.join("device.plist")) else { return device_dir.exists() };
    let state = if plist::is_binary(&bytes) {
        match plist::decode(&bytes).ok().as_ref().and_then(|p| p.get("state")) {
            Some(Plist::Integer(n)) => u64::try_from(*n).ok(),
            _ => None,
        }
    } else {
        String::from_utf8_lossy(&bytes)
            .split_once("<key>state</key>")
            .and_then(|(_, rest)| rest.trim_start().strip_prefix("<integer>")?.split_once("</integer>")?.0.trim().parse().ok())
    };
    state.is_none_or(|n| sim_core::device::DeviceState::from_raw(n) == sim_core::device::DeviceState::Booted)
}

/// The request the in-device helper takes for a batch: the domain, its container, the values to
/// set and the keys to remove.
pub fn write_request(store: &Store, container: &Path, ops: &[super::Op]) -> Result<Vec<u8>, String> {
    let mut set = Vec::new();
    let mut delete = Vec::new();
    for op in ops {
        match op {
            super::Op::Set { key, value } => set.push((key.clone(), plist::from_pref(value)?)),
            super::Op::Delete { key } => delete.push(Plist::String(key.clone())),
        }
    }
    plist::encode(&Plist::Dict(vec![
        ("domain".into(), Plist::String(store.name.clone())),
        ("container".into(), Plist::String(container.to_string_lossy().into_owned())),
        ("set".into(), Plist::Dict(set)),
        ("delete".into(), Plist::Array(delete)),
    ]))
}

/// The helper's answer to `read`: `ok <base64 binary plist>`.
pub fn parse_read_reply(stdout: &str) -> Result<Entries, String> {
    use base64::Engine as _;
    let line = stdout.lines().next().unwrap_or("").trim();
    let Some(b64) = line.strip_prefix("ok ") else { return Err(helper_error(stdout)) };
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("prefstool read: {e}"))?;
    plist::entries(&bytes)
}

fn helper_error(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("").trim();
    format!("cfprefsd: {}", line.strip_prefix("error ").unwrap_or(if line.is_empty() { "the helper said nothing" } else { line }))
}

/// The error for a refused `write`. A refusal after the helper handed the keys to cfprefsd
/// (the final synchronize) is not "nothing changed": a running app may already see them.
#[cfg(target_os = "macos")]
fn write_failure(stdout: &str, ops: &[super::Op]) -> String {
    let error = helper_error(stdout);
    if !error.contains("after taking the keys") {
        return error;
    }
    let keys: Vec<&str> = ops.iter().map(super::Op::key).collect();
    format!("{error}: some of [{}] may already be live in the running app", keys.join(", "))
}

/// iOS preferences: files on the host, cfprefsd in the device.
pub struct Ios {
    /// Where devices' directories are (`<root>/<UDID>/data`).
    devices: PathBuf,
    by_device: HashMap<String, Device>,
    /// The device set `devices` is, when it isn't the default set (#409).
    set: Option<String>,
    /// How to connect to the simulator service on that set (the calling session's way).
    #[cfg(target_os = "macos")]
    open: Option<crate::ios::Opener>,
    #[cfg(target_os = "macos")]
    ctx: Option<sim_coresim::Context>,
}

impl Default for Ios {
    fn default() -> Ios {
        Ios::with_root(default_devices_root())
    }
}

impl Ios {
    /// Devices under `devices` instead of the default set (tests).
    pub fn with_root(devices: PathBuf) -> Ios {
        Ios {
            devices,
            by_device: HashMap::new(),
            set: None,
            #[cfg(target_os = "macos")]
            open: None,
            #[cfg(target_os = "macos")]
            ctx: None,
        }
    }

    /// The devices of the device set at `set` (#409), whose directory holds them as the default
    /// set's does, connecting to the service through `open`.
    #[cfg(target_os = "macos")]
    pub(crate) fn in_set(set: String, open: crate::ios::Opener) -> Ios {
        Ios { set: Some(set.clone()), open: Some(open), ..Ios::with_root(PathBuf::from(set)) }
    }

    fn data(&self, udid: &str) -> Result<PathBuf, String> {
        let data = self.devices.join(udid).join("data");
        if data.is_dir() {
            Ok(data)
        } else {
            let set = self.set.as_deref().map_or_else(|| "the default device set".to_owned(), |set| format!("the device set {set}"));
            Err(format!("{} doesn't exist: is `{udid}` a simulator of {set}?", data.display()))
        }
    }

    fn device(&mut self, udid: &str) -> &mut Device {
        self.by_device.entry(udid.to_owned()).or_default()
    }

    /// The app's stores, read from the device's files.
    pub fn list(&mut self, udid: &str, app: &str) -> Result<Vec<Store>, String> {
        let data = self.data(udid)?;
        self.device(udid).stores(&data, app)
    }

    /// A store's container and file.
    pub fn locate(&mut self, udid: &str, store: &Store) -> Result<(PathBuf, PathBuf), String> {
        let data = self.data(udid)?;
        let container = self.device(udid).container(&data, store)?;
        let file = store_file(&container, store);
        Ok((container, file))
    }

    /// A store's keys, decoded from its file on the host.
    pub fn read_host(&mut self, udid: &str, store: &Store) -> Result<Entries, String> {
        read_file(&self.locate(udid, store)?.1)
    }

    /// A tracker of `store` (every store of `app` when `None`), with each read now.
    pub fn tracker(&mut self, udid: &str, app: &str, store: Option<&Store>) -> Result<Tracker, String> {
        let data = self.data(udid)?;
        let app_prefs = self.device(udid).app_container(&data, app)?.join("Library/Preferences");
        let stores = match store {
            Some(store) => vec![store.clone()],
            None => self.list(udid, app)?,
        };
        let mut located = Vec::with_capacity(stores.len());
        for store in stores {
            let (_, file) = self.locate(udid, &store)?;
            located.push((store, file));
        }
        Ok(Tracker::new(app, app_prefs, store.is_none(), located))
    }
}

#[cfg(target_os = "macos")]
mod device {
    use std::time::Duration;

    use super::super::{AppState, Backend, Entries, Op, Sink, Store, Visibility, WriteResult};
    use super::{Date, Ios, parse_read_reply, write_request};

    /// The in-device helper's source (`native/prefstool.m`).
    const SOURCE: &str = include_str!("../../native/prefstool.m");
    /// How often a feed's watcher wakes with nothing changed, to see whether its feed or device
    /// has ended (and to catch a store whose directory didn't exist to be watched).
    const TICK: Duration = Duration::from_millis(500);

    impl Ios {
        fn sim(&mut self, udid: &str) -> Result<sim_coresim::Device, String> {
            if self.ctx.is_none() {
                self.ctx = Some(match &self.open {
                    Some(open) => open(self.set.as_deref())?,
                    None => sim_coresim::Context::new().map_err(|e| e.to_string())?,
                });
            }
            let ctx = self.ctx.as_ref().ok_or("no service connection")?;
            let device = ctx.device(udid).map_err(|e| e.to_string())?;
            if device.info().state != sim_core::device::DeviceState::Booted {
                return Err(format!(
                    "{} is {}: iOS preference writes and fresh reads go through the device's cfprefsd, so boot it first",
                    device.info().name,
                    device.info().state.as_str()
                ));
            }
            Ok(device)
        }

        /// Runs the helper in the device; its stdout.
        fn helper(&mut self, udid: &str, args: &[String]) -> Result<String, String> {
            let device = self.sim(udid)?;
            let sdk = sim_netshape::Sdk::for_runtime(&device.info().runtime_id).map_err(|_| {
                format!("app preferences are written on iOS, iPadOS and tvOS devices; `{}` isn't one", device.info().runtime_id)
            })?;
            let tool = sim_netshape::objc_tool(sdk, "prefstool", SOURCE, &["-framework", "Foundation"])?;
            let out = device.exec(&tool.display().to_string(), args, &[]).map_err(|e| e.to_string())?;
            Ok(out.stdout)
        }
    }

    impl Backend for Ios {
        fn stores(&mut self, device: &str, app: &str) -> Result<Vec<Store>, String> {
            self.list(device, app)
        }

        fn read(&mut self, device: &str, store: &Store) -> Result<Entries, String> {
            self.read_host(device, store)
        }

        fn read_fresh(&mut self, device: &str, store: &Store) -> Result<Entries, String> {
            let (container, _) = self.locate(device, store)?;
            let args = ["read".to_owned(), store.name.clone(), container.to_string_lossy().into_owned()];
            parse_read_reply(&self.helper(device, &args)?)
        }

        fn write_batch(&mut self, device: &str, store: &Store, ops: &[Op]) -> Result<WriteResult, String> {
            let (container, _) = self.locate(device, store)?;
            let request = write_request(store, &container, ops)?;
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("simee-prefs-{}-{n}.plist", std::process::id()));
            std::fs::write(&path, &request).map_err(|e| format!("{}: {e}", path.display()))?;
            let out = self.helper(device, &["write".to_owned(), path.to_string_lossy().into_owned()]);
            std::fs::remove_file(&path).ok();
            let out = out?;
            if out.lines().next().map(str::trim) != Some("ok") {
                return Err(super::write_failure(&out, ops));
            }
            // cfprefsd hands the values to every process using the domain: a running app's
            // UserDefaults has them now (its KVO observers fire), one not running reads them at launch.
            Ok(WriteResult { visible: Visibility::Now, app: AppState::Untouched, note: None })
        }

        fn watch(&mut self, device: &str, app: &str, store: Option<&Store>, sink: Sink) -> Result<(), String> {
            let mut tracker = self.tracker(device, app, store)?;
            let device_dir = self.devices.join(device);
            let mut watch = sim_coresim::FileWatch::new().map_err(|e| format!("kqueue: {e}"))?;
            watch.set(&tracker.paths()).map_err(|e| format!("kqueue: {e}"))?;
            std::thread::Builder::new()
                .name("simee-prefs-ios".into())
                .spawn(move || {
                    while sink.is_open() && super::booted(&device_dir) {
                        // A failed wait (it never has) falls back to looking every tick.
                        watch.wait(TICK).ok();
                        // Watched again before looking: a replaced file is a new inode, and a
                        // change made from here on wakes the next wait.
                        watch.set(&tracker.paths()).ok();
                        let (changes, grew) = tracker.poll(Date::now());
                        if !changes.into_iter().all(|change| sink.send(change)) {
                            return;
                        }
                        if grew {
                            watch.set(&tracker.paths()).ok();
                        }
                    }
                })
                .map_err(|e| format!("can't start the preferences watcher: {e}"))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
