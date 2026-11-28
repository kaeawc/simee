//! The window's settings: a small versioned JSON file in the platform's app-support directory
//! (`~/Library/Application Support/simee/desktop.json` on macOS, `$XDG_CONFIG_HOME/simee/` on
//! Linux, `%APPDATA%\simee\` on Windows; `SIMEE_DESKTOP_SETTINGS` names another file).
//!
//! It holds the panes' widths and visibility, the device list's organization (collapsed groups,
//! hidden devices and tools, order, favorites), the last selected device and debug tab, the
//! window's frame, each popped-out device window's frame ([`crate::popout`]) and the screenshot
//! folder. It is read once at start; changes are written a
//! moment after they stop ([`DEBOUNCE`]), and once more as the window closes. Every write goes
//! to a temporary file beside it that is then renamed over it, so a crash never leaves half a
//! file.
//!
//! A file that can't be read, isn't JSON of this shape, or comes from a newer version falls back
//! to the defaults with one warning. It is copied aside first (`desktop.json.<why>-<secs>.bak`)
//! and never deleted; when even the copy fails, nothing is written this session, so the file is
//! still there for the version that wrote it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The settings format this build writes. A file with no `version` is version 0: hand-written,
/// or older than the field.
pub const VERSION: u32 = 1;

/// Seconds of quiet after the last change before it is written: a pane drag writes once.
pub const DEBOUNCE: f64 = 0.5;

/// The file's name in its directory.
pub const FILE: &str = "desktop.json";

/// One side pane: its width in points (0 is the default width) and whether it shows.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pane {
    pub width: f32,
    pub visible: bool,
}

impl Default for Pane {
    fn default() -> Self {
        Pane { width: 0.0, visible: true }
    }
}

/// Where the window was: its frame's top-left in desktop pixels (`WindowOptions::position`'s
/// space) and its content size in points.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub x: i32,
    pub y: i32,
    pub width: f32,
    pub height: f32,
}

/// Everything the window keeps between runs. Fields missing from a file take their defaults;
/// fields this build doesn't know are ignored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    /// The device list on the left.
    pub sidebar: Pane,
    /// The platform panel on the right (iOS: debug tabs; Android: the guest log).
    pub inspector: Pane,
    /// Device-list groups shown collapsed (a platform or runtime id).
    pub collapsed_groups: Vec<String>,
    /// Devices left out of the list, by UDID or AVD name.
    pub hidden_devices: Vec<String>,
    /// Toolbar controls left out, by key (`rotate-button`).
    pub hidden_tools: Vec<String>,
    /// The device list's order, by UDID; devices not named follow in scan order.
    pub device_order: Vec<String>,
    /// Devices pinned to the top of the list.
    pub favorites: Vec<String>,
    pub last_device: Option<String>,
    /// The debug panel's tab (`device`, `network`, `storage`, `processes`, `logs`).
    pub last_tab: Option<String>,
    pub window: Option<WindowFrame>,
    /// Where each device's pop-out window was last ([`crate::popout`]), by UDID or AVD name:
    /// popping it out again, in this run or the next, opens it there.
    pub popouts: BTreeMap<String, WindowFrame>,
    /// Where ⌘S saves; the Desktop when unset.
    pub screenshot_dir: Option<PathBuf>,
    /// The resource panel's visibility and sort ([`crate::resources`]).
    pub resources: crate::resources::Prefs,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            version: VERSION,
            sidebar: Pane::default(),
            inspector: Pane::default(),
            collapsed_groups: Vec::new(),
            hidden_devices: Vec::new(),
            hidden_tools: Vec::new(),
            device_order: Vec::new(),
            favorites: Vec::new(),
            last_device: None,
            last_tab: None,
            window: None,
            popouts: BTreeMap::new(),
            screenshot_dir: None,
            resources: crate::resources::Prefs::default(),
        }
    }
}

/// Why a file fell back to the defaults.
#[derive(Clone, Debug, PartialEq)]
pub enum Fallback {
    /// It exists but couldn't be read.
    Unreadable(String),
    /// Not JSON, or not this shape.
    Corrupt(String),
    /// Written by a newer build.
    Future(u32),
}

impl Fallback {
    fn tag(&self) -> &'static str {
        match self {
            Fallback::Unreadable(_) => "unreadable",
            Fallback::Corrupt(_) => "corrupt",
            Fallback::Future(_) => "future",
        }
    }

    fn describe(&self) -> String {
        match self {
            Fallback::Unreadable(e) => format!("can't be read ({e})"),
            Fallback::Corrupt(e) => format!("isn't valid ({e})"),
            Fallback::Future(v) => format!("is version {v}, newer than this build's {VERSION}"),
        }
    }
}

/// Parses a settings file's text: migrates an older version, refuses a newer one.
pub fn parse(text: &str) -> Result<Settings, Fallback> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| Fallback::Corrupt(e.to_string()))?;
    if !value.is_object() {
        return Err(Fallback::Corrupt("not a JSON object".into()));
    }
    let version = match value.get("version") {
        None => 0,
        Some(v) => v.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| Fallback::Corrupt(format!("version {v}")))?,
    };
    if version > VERSION {
        return Err(Fallback::Future(version));
    }
    migrate(value, version)
}

/// Brings a version-`from` file up to [`VERSION`]. Version 0 has the same fields as 1 (it is a
/// file with no `version`); a later format change adds its step here.
fn migrate(value: serde_json::Value, from: u32) -> Result<Settings, Fallback> {
    let mut settings: Settings = match from {
        0 | 1 => serde_json::from_value(value).map_err(|e| Fallback::Corrupt(e.to_string()))?,
        _ => unreachable!("newer versions are refused before migrating"),
    };
    settings.version = VERSION;
    Ok(settings)
}

/// The settings file for this platform, from the environment `var` reads:
/// `SIMEE_DESKTOP_SETTINGS`, else the app-support (macOS), roaming app-data (Windows) or XDG
/// config (elsewhere) directory's `simee/desktop.json`. `None` without a home directory.
pub fn default_path(os: &str, var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |k: &str| var(k).filter(|v| !v.is_empty());
    if let Some(p) = var("SIMEE_DESKTOP_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    let dir = match os {
        "macos" => PathBuf::from(var("HOME")?).join("Library/Application Support"),
        "windows" => PathBuf::from(var("APPDATA")?),
        _ => var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))?,
    };
    Some(dir.join("simee").join(FILE))
}

/// A free name beside `path` for a copy of it: `desktop.json.<tag>-<secs>.bak`, then `-2`, `-3`….
fn backup_path(path: &Path, tag: &str) -> PathBuf {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let name = path.file_name().map_or_else(|| FILE.into(), |n| n.to_string_lossy().into_owned());
    let at = |n: u32| {
        let suffix = if n == 1 { String::new() } else { format!("-{n}") };
        path.with_file_name(format!("{name}.{tag}-{secs}{suffix}.bak"))
    };
    (1..).map(at).find(|p| !p.exists()).expect("a free name")
}

/// Writes `settings` to `path` atomically: a temporary file in the same directory, flushed, then
/// renamed over `path`. Creates the directory.
pub fn write(path: &Path, settings: &Settings) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map_or_else(|| FILE.into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let json = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // Only our own temporary file; the settings file is untouched.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The settings in use, and where and when they're written.
#[derive(Debug)]
pub struct Store {
    /// The file; `None` never touches the disk (headless scenes, tests, a file kept for a newer
    /// build that couldn't be copied aside).
    path: Option<PathBuf>,
    /// What the window uses now; change it freely, [`Store::tick`] writes it.
    pub current: Settings,
    /// What the file holds.
    saved: Settings,
    /// `current` as the last tick saw it, to tell a new change from one still waiting.
    seen: Settings,
    /// When the waiting change is written.
    due: Option<f64>,
    /// One line for the status bar: a fallback at load, a failed write.
    warning: Option<String>,
}

impl Default for Store {
    fn default() -> Self {
        Store::memory(Settings::default())
    }
}

impl Store {
    /// Settings that are never read from or written to disk.
    pub fn memory(settings: Settings) -> Store {
        Store { path: None, saved: settings.clone(), seen: settings.clone(), current: settings, due: None, warning: None }
    }

    /// The platform's settings file ([`default_path`]).
    pub fn open_default() -> Store {
        match default_path(std::env::consts::OS, |k| std::env::var(k).ok()) {
            Some(path) => Store::open(path),
            None => Store { warning: Some("settings: no home directory; using defaults and not saving".into()), ..Store::default() },
        }
    }

    /// Reads `path`. A missing file is the defaults; one that fails ([`Fallback`]) is copied aside
    /// and the defaults are used with a warning; when the copy fails too, nothing is written.
    pub fn open(path: PathBuf) -> Store {
        let read = match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(Fallback::Unreadable(e.to_string())),
        };
        let fallback = match read {
            Ok(settings) => {
                let mut store = Store::memory(settings);
                store.path = Some(path);
                return store;
            }
            Err(f) => f,
        };
        let mut store = Store::default();
        let backup = backup_path(&path, fallback.tag());
        let what = format!("settings {} {}", path.display(), fallback.describe());
        store.warning = Some(match std::fs::copy(&path, &backup) {
            Ok(_) => {
                store.path = Some(path);
                format!("{what}: using defaults; the old file is at {}", backup.display())
            }
            Err(e) => format!("{what}: using defaults and not saving this session (can't copy it aside: {e})"),
        });
        store
    }

    /// Whether changes reach a file.
    pub fn persistent(&self) -> bool {
        self.path.is_some()
    }

    /// The warning waiting for the status bar, once.
    pub fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }

    /// Call every poll at time `now` (seconds): a change starts (or restarts) the [`DEBOUNCE`]
    /// wait, and a wait that has run out writes. Returns the seconds until the next write is
    /// due, for the window's wake.
    pub fn tick(&mut self, now: f64) -> Option<f64> {
        if self.path.is_none() {
            // Nothing to write: no wake for it.
            if self.current != self.saved {
                self.saved = self.current.clone();
                self.seen = self.current.clone();
            }
            return None;
        }
        if self.current != self.seen {
            self.seen = self.current.clone();
            self.due = (self.current != self.saved).then_some(now + DEBOUNCE);
        }
        let due = self.due?;
        if now >= due {
            self.flush();
            return None;
        }
        Some(due - now)
    }

    /// Writes a waiting change now (the window is closing). A failed write warns once and isn't
    /// retried until the next change.
    pub fn flush(&mut self) {
        self.due = None;
        if self.current == self.saved {
            return;
        }
        self.saved = self.current.clone();
        self.seen = self.current.clone();
        let Some(path) = &self.path else { return };
        if let Err(e) = write(path, &self.current) {
            self.warning = Some(format!("settings: can't save {}: {e}", path.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory under the system temp dir, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!("simee-settings-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join("simee").join(FILE)
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.0.join("simee"))
                .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample() -> Settings {
        Settings {
            sidebar: Pane { width: 300.0, visible: false },
            inspector: Pane { width: 420.0, visible: true },
            collapsed_groups: vec!["android".into()],
            hidden_devices: vec!["ABC".into()],
            hidden_tools: vec!["rotate-button".into()],
            device_order: vec!["B".into(), "A".into()],
            favorites: vec!["A".into()],
            last_device: Some("A".into()),
            last_tab: Some("network".into()),
            window: Some(WindowFrame { x: -1200, y: 40, width: 1300.0, height: 900.0 }),
            popouts: BTreeMap::from([("A".into(), WindowFrame { x: 1600, y: 80, width: 430.0, height: 900.0 })]),
            screenshot_dir: Some(PathBuf::from("/tmp/shots")),
            ..Settings::default()
        }
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = TempDir::new();
        write(&dir.file(), &sample()).unwrap();
        let store = Store::open(dir.file());
        assert_eq!(store.current, sample());
        assert!(store.persistent());
        assert_eq!(dir.entries(), [FILE], "no temporary file is left behind");
        let text = std::fs::read_to_string(dir.file()).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    #[test]
    fn a_missing_file_is_the_defaults_without_a_warning() {
        let dir = TempDir::new();
        let mut store = Store::open(dir.file());
        assert_eq!(store.current, Settings::default());
        assert!(store.persistent());
        assert_eq!(store.take_warning(), None);
    }

    #[test]
    fn a_corrupt_file_falls_back_once_and_is_copied_aside() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
        std::fs::write(dir.file(), "{\"sidebar\": {\"width\": 3").unwrap();
        let mut store = Store::open(dir.file());
        assert_eq!(store.current, Settings::default());
        let warning = store.take_warning().expect("a warning");
        assert!(warning.contains("isn't valid") && warning.contains(".bak"), "{warning}");
        assert_eq!(store.take_warning(), None, "warned once");
        let names = dir.entries();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names[1].starts_with("desktop.json.corrupt-") && names[1].ends_with(".bak"), "{names:?}");
        let backup = dir.0.join("simee").join(&names[1]);
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "{\"sidebar\": {\"width\": 3", "the copy is the file as it was");
        // The next save replaces it; the copy stays.
        store.current.sidebar.visible = false;
        store.flush();
        assert!(!Store::open(dir.file()).current.sidebar.visible);
        assert_eq!(dir.entries().len(), 2);
    }

    #[test]
    fn wrong_shapes_are_corrupt() {
        for text in ["[]", "null", "{\"version\": \"one\"}", "{\"favorites\": 5}", "{\"version\": -1}", ""] {
            assert!(matches!(parse(text), Err(Fallback::Corrupt(_))), "{text:?}: {:?}", parse(text));
        }
    }

    #[test]
    fn an_old_file_migrates_and_keeps_what_it_had() {
        // Version 0: no version field, and only some of the fields.
        let old = parse("{\"sidebar\": {\"width\": 280}, \"favorites\": [\"X\"], \"someday\": true}").unwrap();
        assert_eq!(old.version, VERSION);
        assert_eq!(old.sidebar, Pane { width: 280.0, visible: true });
        assert_eq!(old.favorites, ["X"]);
        assert_eq!(old.inspector, Pane::default());
        assert_eq!(parse("{}").unwrap(), Settings::default());
    }

    /// Pop-out frames are kept by device: a file written before them (#280's) has none, and one
    /// that names them keeps every device's through a save.
    #[test]
    fn pop_out_frames_are_kept_by_device() {
        let before = parse("{\"version\": 1, \"window\": {\"x\": 1, \"y\": 2, \"width\": 1240, \"height\": 870}}").unwrap();
        assert!(before.popouts.is_empty());
        let text = "{\"version\": 1, \"popouts\": {\"A1\": {\"x\": 10, \"y\": 20, \"width\": 430, \"height\": 900}, \
                    \"pixel_9\": {\"x\": -800, \"y\": 0, \"width\": 400, \"height\": 860}}}";
        let s = parse(text).unwrap();
        assert_eq!(s.popouts.len(), 2);
        assert_eq!(s.popouts["pixel_9"], WindowFrame { x: -800, y: 0, width: 400.0, height: 860.0 });
        let dir = TempDir::new();
        write(&dir.file(), &s).unwrap();
        assert_eq!(Store::open(dir.file()).current.popouts, s.popouts);
    }

    #[test]
    fn a_newer_file_is_kept_for_its_build() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
        let newer = "{\"version\": 7, \"sidebar\": {\"width\": 250}}";
        std::fs::write(dir.file(), newer).unwrap();
        let mut store = Store::open(dir.file());
        assert_eq!(store.current, Settings::default());
        let warning = store.take_warning().unwrap();
        assert!(warning.contains("version 7"), "{warning}");
        let names = dir.entries();
        assert!(names.iter().any(|n| n.starts_with("desktop.json.future-")), "{names:?}");
        assert_eq!(parse(newer), Err(Fallback::Future(7)));
    }

    #[test]
    fn an_unreadable_file_is_never_overwritten() {
        let dir = TempDir::new();
        // A directory where the file should be: it can't be read, nor copied aside.
        std::fs::create_dir_all(dir.file()).unwrap();
        let mut store = Store::open(dir.file());
        assert!(!store.persistent(), "nothing is written this session");
        let warning = store.take_warning().unwrap();
        assert!(warning.contains("can't be read") && warning.contains("not saving"), "{warning}");
        store.current.sidebar.visible = false;
        assert_eq!(store.tick(0.0), None, "no write to wake for");
        store.flush();
        assert!(dir.file().is_dir(), "left as it was");
    }

    #[test]
    fn changes_are_written_once_they_stop() {
        let dir = TempDir::new();
        let mut store = Store::open(dir.file());
        assert_eq!(store.tick(0.0), None, "nothing changed: no wake");
        store.current.sidebar.width = 250.0;
        assert_eq!(store.tick(1.0), Some(DEBOUNCE));
        // A drag keeps changing it: each change restarts the wait.
        store.current.sidebar.width = 260.0;
        assert_eq!(store.tick(1.3), Some(DEBOUNCE));
        assert!(store.tick(1.6).is_some_and(|d| (d - 0.2).abs() < 1e-9));
        assert!(!dir.file().exists(), "not written while it's still changing");
        assert_eq!(store.tick(1.8), None);
        assert_eq!(Store::open(dir.file()).current.sidebar.width, 260.0, "written once it stopped");
        // A change undone before its write is nothing to write.
        store.current.sidebar.width = 270.0;
        store.tick(2.0);
        store.current.sidebar.width = 260.0;
        assert_eq!(store.tick(2.1), None);
        // Closing writes what's waiting at once.
        store.current.favorites.push("A".into());
        assert!(store.tick(3.0).is_some());
        store.flush();
        assert_eq!(Store::open(dir.file()).current.favorites, ["A"]);
        assert_eq!(store.tick(3.1), None);
    }

    #[test]
    fn memory_settings_never_touch_the_disk() {
        let mut store = Store::default();
        store.current.sidebar.visible = false;
        store.tick(0.0);
        store.tick(1.0);
        store.flush();
        assert!(!store.persistent());
        assert_eq!(store.take_warning(), None);
    }

    #[test]
    fn the_file_lives_in_the_platforms_settings_directory() {
        let env =
            |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let home: &'static [(&str, &str)] = &[("HOME", "/Users/me")];
        assert_eq!(default_path("macos", env(home)), Some(PathBuf::from("/Users/me/Library/Application Support/simee/desktop.json")));
        assert_eq!(default_path("linux", env(home)), Some(PathBuf::from("/Users/me/.config/simee/desktop.json")));
        assert_eq!(
            default_path("linux", env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/cfg")])),
            Some(PathBuf::from("/cfg/simee/desktop.json"))
        );
        // A relative XDG directory is ignored, as the spec says.
        assert_eq!(
            default_path("linux", env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "cfg")])),
            Some(PathBuf::from("/h/.config/simee/desktop.json"))
        );
        assert_eq!(default_path("windows", env(&[("APPDATA", "C:\\A")])), Some(PathBuf::from("C:\\A").join("simee").join(FILE)));
        assert_eq!(default_path("macos", env(&[("SIMEE_DESKTOP_SETTINGS", "/x.json"), ("HOME", "/h")])), Some(PathBuf::from("/x.json")));
        assert_eq!(default_path("macos", env(&[])), None);
    }
}
