//! iOS simulators as portable bundles (#421): a shut-down simulator's data directory (its
//! settings, its installed apps and their containers) less what the system regenerates
//! ([`left_out`]), as `data/` in the bundle with its directories, symlinks and modes in the
//! manifest's tree; and its device type and runtime by identifier and build, which an import
//! finds on the receiving host or refuses.
//!
//! The simulator service is behind [`Service`] (`sim-coresim` implements it over CoreSimulator,
//! tests over a fake), so all of it builds and tests on any host.

use std::fs;
use std::path::{Path, PathBuf};

use sim_core::hostfs::PermissionsExt;
use sim_core::platform::Platform;

use crate::{
    COLD_BOOT, Captures, Contents, Device, Entry, EntryKind, Error, Extracted, FORMAT, GuestIdentity, Identity, IosSimulator, Manifest,
    Options, Reader, Result, Secret, Summary, safe_path,
};

/// Where the device's data directory goes in the bundle.
pub const DATA: &str = "data";

/// What an import keeps of the exported device that makes it the same device to its apps.
pub const GUEST_IDENTIFIERS: [&str; 2] = ["keychain items", "identifiers apps keep in their containers and preferences"];

/// What an import says about guest identity, in its result.
pub const DUPLICATED_IDENTITY: &str = "the import has a new UDID, but its apps see the device they were exported from: the \
     keychain and every app's container and preferences come along";

/// Where a directory's contents stay out of a bundle (the directory itself goes in, empty): the
/// system's and each app's `Library/Caches`, the device's `tmp` and each container's, all of which
/// iOS may purge and rebuilds.
pub fn left_out(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    matches!(parts.as_slice(), [.., "Library", "Caches"] | ["tmp"] | ["Containers", _, _, _, "tmp"])
}

/// A simulator as an export or import sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Simulator {
    pub udid: String,
    pub name: String,
    pub shut_down: bool,
    /// Its state, for messages (`Booted`).
    pub state: String,
    /// Its device type's identifier.
    pub device_type: String,
    /// Its runtime's identifier.
    pub runtime: String,
    pub form_factor: Option<String>,
    /// Its data directory on this host (`<set>/<udid>/data`).
    pub data_dir: PathBuf,
}

/// An installed runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    pub identifier: String,
    pub name: String,
    pub version: String,
    pub build: String,
    pub available: bool,
}

/// The simulator service, on one device set.
pub trait Service {
    /// The device with UDID `udid`, as it is now.
    fn device(&self, udid: &str) -> std::result::Result<Simulator, String>;
    /// Every device's name in the set.
    fn names(&self) -> std::result::Result<Vec<String>, String>;
    fn runtimes(&self) -> std::result::Result<Vec<Runtime>, String>;
    /// Creates a device (shut down, never booted) of `device_type` on `runtime` (identifiers).
    fn create(&self, name: &str, device_type: &str, runtime: &str) -> std::result::Result<Simulator, String>;
    fn delete(&self, udid: &str) -> std::result::Result<(), String>;
}

/// What an export wrote.
#[derive(Clone, Debug)]
pub struct Exported {
    pub manifest: Manifest,
    pub summary: Summary,
    /// The directories whose contents stayed out ([`left_out`]), at their paths in the bundle.
    pub left_out: Vec<String>,
}

/// What an import made.
#[derive(Clone, Debug)]
pub struct Imported {
    pub device: Simulator,
    pub runtime: Runtime,
    pub extracted: Extracted,
    pub manifest: Manifest,
}

#[derive(Default)]
struct Walk {
    disks: Vec<(String, PathBuf)>,
    tree: Vec<Entry>,
    dropped: Vec<String>,
    left_out: Vec<String>,
}

fn io(p: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Io(p.to_path_buf(), e)
}

fn mode(meta: &fs::Metadata) -> Option<u32> {
    Some(meta.permissions().mode() & 0o7777)
}

/// Absolute link targets that name this host rather than the device: its users' homes, volumes
/// and temporary directories.
const HOST_ROOTS: [&str; 6] = ["/Users/", "/Volumes/", "/private/var/folders/", "/var/folders/", "/private/tmp/", "/tmp/"];

/// What the bundle holds for a symlink at `rel` (relative to the data directory `root`) to
/// `target`: a relative target as it is; an absolute one into the data directory as the same
/// place relative to the link; another absolute one as it is, unless it names the host (under
/// the device's own directory or [`HOST_ROOTS`]), which is `None`: left out.
fn portable_target(root: &Path, rel: &str, target: &Path) -> Option<String> {
    let t = target.to_str()?;
    if !target.is_absolute() {
        return Some(t.to_owned());
    }
    if let Ok(inside) = target.strip_prefix(root) {
        let up = "../".repeat(rel.matches('/').count());
        let inside = inside.to_str()?;
        return Some(match (up.as_str(), inside) {
            ("", "") => ".".to_owned(),
            (up, "") => up.trim_end_matches('/').to_owned(),
            (up, inside) => format!("{up}{inside}"),
        });
    }
    let device_dir = root.parent().filter(|d| d.parent().is_some());
    if device_dir.is_some_and(|d| target.starts_with(d)) || HOST_ROOTS.iter().any(|h| t.starts_with(h)) {
        return None;
    }
    Some(t.to_owned())
}

/// The data directory `root` as bundle contents: every regular file a disk at `data/<path>`,
/// every directory and symlink and every file's mode an entry, in a stable (sorted) order.
fn walk(root: &Path) -> Result<Walk> {
    let meta = fs::symlink_metadata(root).map_err(io(root))?;
    if !meta.is_dir() {
        return Err(Error::Refused(format!("{} isn't a directory", root.display())));
    }
    let mut w = Walk::default();
    w.tree.push(Entry { path: DATA.into(), kind: EntryKind::Dir, mode: mode(&meta), target: None });
    walk_dir(root, root, "", &mut w)?;
    Ok(w)
}

fn walk_dir(root: &Path, dir: &Path, rel: &str, w: &mut Walk) -> Result<()> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir).map_err(io(dir))? {
        names.push(e.map_err(io(dir))?.file_name());
    }
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let Some(n) = name.to_str() else {
            w.dropped.push(format!("{DATA}/{rel}/{} (a name that isn't UTF-8)", name.to_string_lossy()));
            continue;
        };
        let r = if rel.is_empty() { n.to_owned() } else { format!("{rel}/{n}") };
        let at = format!("{DATA}/{r}");
        if !safe_path(&at) {
            w.dropped.push(format!("{at} (a name a bundle can't hold)"));
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(io(&path))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(&path).map_err(io(&path))?;
            match portable_target(root, &r, &target) {
                Some(t) => w.tree.push(Entry { path: at, kind: EntryKind::Symlink, mode: None, target: Some(t) }),
                None => w.dropped.push(format!("{at} (a symlink to a host path)")),
            }
        } else if kind.is_dir() {
            w.tree.push(Entry { path: at.clone(), kind: EntryKind::Dir, mode: mode(&meta), target: None });
            if left_out(&r) {
                w.left_out.push(at);
            } else {
                walk_dir(root, &path, &r, w)?;
            }
        } else if kind.is_file() {
            w.tree.push(Entry { path: at.clone(), kind: EntryKind::File, mode: mode(&meta), target: None });
            w.disks.push((at, path));
        } else {
            w.dropped.push(format!("{at} (a socket, pipe or device node)"));
        }
    }
    Ok(())
}

/// Refuses a bundle path inside the device's own directory (`<set>/<udid>`): an export never
/// writes into the device it reads.
fn refuse_inside(d: &Simulator, to: &Path) -> Result<()> {
    let Some(device_dir) = d.data_dir.parent().and_then(|p| fs::canonicalize(p).ok()) else { return Ok(()) };
    let parent = match to.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if fs::canonicalize(&parent).is_ok_and(|p| p.starts_with(&device_dir)) {
        return Err(Error::Refused(format!(
            "{} is inside {}'s own directory; export never writes into the device it reads",
            to.display(),
            d.name
        )));
    }
    Ok(())
}

/// Exports simulator `udid`, shut down, to a bundle at `to` (a `.tar` file, else a new
/// directory): its data directory as `data/` ([`left_out`] directories empty), its device type
/// and runtime (whose build must be installed here, to be recorded). It only reads the device:
/// a booted one is refused, `to` may not be inside its directory, and a device that boots while
/// it is read fails the export and its bundle is removed.
pub fn export(svc: &dyn Service, udid: &str, to: &Path, opts: &Options) -> Result<Exported> {
    let d = svc.device(udid).map_err(Error::Refused)?;
    if !d.shut_down {
        return Err(Error::Refused(format!("{} must be shut down to export it (it is {})", d.name, d.state)));
    }
    if !d.data_dir.is_dir() {
        return Err(Error::Refused(format!("{} has no data directory yet (boot it once, then shut it down)", d.name)));
    }
    refuse_inside(&d, to)?;
    let runtimes = svc.runtimes().map_err(Error::Refused)?;
    let rt = runtimes.iter().find(|r| r.identifier.eq_ignore_ascii_case(&d.runtime)).ok_or_else(|| {
        Error::Refused(format!("{}'s runtime {} isn't installed here, so the bundle can't name its build", d.name, d.runtime))
    })?;
    let walked = walk(&d.data_dir)?;
    let secrets = vec![
        Secret {
            item: format!("{DATA}/Library/Keychains"),
            what: "the device's keychain: passwords, tokens and keys its apps stored".into(),
            excludable: false,
        },
        Secret {
            item: format!("{DATA}/Containers/Data"),
            what: "each app's data container: may hold accounts, tokens and app data".into(),
            excludable: false,
        },
    ];
    let header = Manifest {
        format: FORMAT,
        simee_version: env!("CARGO_PKG_VERSION").into(),
        platform: Platform::Ios,
        device: Device { name: d.name.clone(), form_factor: d.form_factor.clone() },
        identity: Identity::Ios(IosSimulator {
            device_type: d.device_type.clone(),
            runtime: rt.identifier.clone(),
            build: rt.build.clone(),
            version: rt.version.clone(),
            runtime_name: rt.name.clone(),
        }),
        captures: Captures { disks: true, ram: false },
        resume: COLD_BOOT.into(),
        secrets,
        guest_identity: GuestIdentity {
            on_import: "duplicated".into(),
            identifiers: GUEST_IDENTIFIERS.iter().map(|s| (*s).to_owned()).collect(),
        },
        dropped: walked.dropped,
        chunk_size: 0,
        compression: String::new(),
        disks: Vec::new(),
        files: Vec::new(),
        tree: walked.tree,
    };
    let contents = Contents { disks: walked.disks, files: Vec::new() };
    let (manifest, summary) = crate::export(header, &contents, to, opts)?;
    if !svc.device(udid).is_ok_and(|now| now.shut_down) {
        let _ = if to.is_dir() { fs::remove_dir_all(to) } else { fs::remove_file(to) };
        return Err(Error::Refused(format!("{} booted while it was exported; the bundle was removed", d.name)));
    }
    Ok(Exported { manifest, summary, left_out: walked.left_out })
}

/// Whether bundle path `p` is the data directory or under it.
fn in_data(p: &str) -> bool {
    p == DATA || p.strip_prefix(DATA).is_some_and(|rest| rest.starts_with('/'))
}

/// The installed runtime that is `want`'s identifier and build, or a refusal that says what to
/// install (and what this host has instead).
pub fn find_runtime(have: &[Runtime], want: &IosSimulator) -> Result<Runtime> {
    let same = |r: &&Runtime| r.identifier.eq_ignore_ascii_case(&want.runtime);
    if let Some(r) = have.iter().filter(same).find(|r| r.build == want.build && r.available) {
        return Ok(r.clone());
    }
    let near: Vec<String> = have
        .iter()
        .filter(same)
        .map(|r| {
            if r.build == want.build {
                format!("{} ({}) is unavailable", r.name, r.build)
            } else {
                format!("{} is build {}", r.name, r.build)
            }
        })
        .collect();
    let near = if near.is_empty() { String::new() } else { format!(" (this host: {})", near.join("; ")) };
    let platform = want.runtime_name.split_whitespace().next().filter(|p| !p.is_empty()).unwrap_or("iOS");
    Err(Error::Refused(format!(
        "the bundle needs the {} runtime, build {} ({}), which isn't installed{near}; install it (Xcode > Settings > Components, \
         or `xcodebuild -downloadPlatform {platform} -buildVersion {}`) and import again",
        want.runtime_name, want.build, want.runtime, want.build
    )))
}

/// Imports the bundle `reader` opened as a new simulator `name` in the service's set. Before
/// anything is made: the bundle must be an iOS simulator's with everything under `data/`, its
/// runtime installed ([`find_runtime`]) and `name` free in the set. Then the device is created
/// of the bundle's type on that runtime, the bundle extracted and checked beside it, its tree
/// (directories, symlinks, modes) applied, and the result put in place as its data directory.
/// A failure after the create deletes the device, so nothing is left behind.
pub fn import(svc: &dyn Service, reader: Reader, name: &str) -> Result<Imported> {
    if name.trim().is_empty() {
        return Err(Error::Refused("give the new device a `name`".into()));
    }
    let m = reader.manifest().clone();
    if m.platform != Platform::Ios {
        return Err(Error::Refused(format!("the bundle is an {} device's; this imports iOS simulators", m.platform.label())));
    }
    let Identity::Ios(id) = &m.identity else {
        return Err(Error::Refused("the bundle's manifest names no iOS runtime".into()));
    };
    let mut paths = m.disks.iter().map(|d| &d.path).chain(m.files.iter().map(|f| &f.path)).chain(m.tree.iter().map(|e| &e.path));
    if let Some(p) = paths.find(|p| !in_data(p)) {
        return Err(Error::Refused(format!("the bundle holds `{p}`, which isn't under {DATA}/ (a simulator's data directory)")));
    }
    if !m.tree.iter().any(|e| e.path == DATA && e.kind == EntryKind::Dir) {
        return Err(Error::Refused(format!("the bundle has no {DATA}/ directory")));
    }
    let runtime = find_runtime(&svc.runtimes().map_err(Error::Refused)?, id)?;
    if svc.names().map_err(Error::Refused)?.iter().any(|n| n == name) {
        return Err(Error::Refused(format!("a device named `{name}` already exists in this set")));
    }
    let made = svc.create(name, &id.device_type, &runtime.identifier).map_err(Error::Refused)?;
    let placed = place(reader, &m.tree, &made.data_dir).and_then(|x| Ok((x, svc.device(&made.udid).map_err(Error::Refused)?)));
    match placed {
        Ok((extracted, device)) => Ok(Imported { device, runtime, extracted, manifest: m }),
        Err(e) => {
            let _ = svc.delete(&made.udid);
            Err(e)
        }
    }
}

/// Extracts the bundle into a staging directory beside `data_dir` (in the new device's own
/// directory), applies `tree`, and swaps the result in for the data directory the service made.
fn place(reader: Reader, tree: &[Entry], data_dir: &Path) -> Result<Extracted> {
    let device_dir = data_dir.parent().ok_or_else(|| Error::Refused(format!("{} has no parent", data_dir.display())))?;
    let staging = device_dir.join(".simee-import.partial");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(io(&staging))?;
    }
    fs::create_dir(&staging).map_err(io(&staging))?;
    let done = (|| {
        let extracted = reader.extract(&staging)?;
        apply_tree(&staging, tree)?;
        if fs::symlink_metadata(data_dir).is_ok() {
            fs::remove_dir_all(data_dir).map_err(io(data_dir))?;
        }
        let data = staging.join(DATA);
        fs::rename(&data, data_dir).map_err(io(data_dir))?;
        Ok(extracted)
    })();
    let _ = fs::remove_dir_all(&staging);
    done
}

/// Makes `tree`'s directories and symlinks under `dest` (where the files were extracted) and
/// gives each file and directory its mode, directories last and deepest first, so a read-only
/// one is set after what is in it.
#[cfg(unix)]
fn apply_tree(dest: &Path, tree: &[Entry]) -> Result<()> {
    let set = |p: &Path, m: u32| fs::set_permissions(p, fs::Permissions::from_mode(m)).map_err(io(p));
    for e in tree.iter().filter(|e| e.kind == EntryKind::Dir) {
        let p = dest.join(&e.path);
        fs::create_dir_all(&p).map_err(io(&p))?;
    }
    for e in tree {
        let p = dest.join(&e.path);
        match (e.kind, &e.target, e.mode) {
            (EntryKind::Symlink, Some(t), _) => std::os::unix::fs::symlink(t, &p).map_err(io(&p))?,
            (EntryKind::File, _, Some(m)) => set(&p, m)?,
            _ => {}
        }
    }
    for e in tree.iter().rev().filter(|e| e.kind == EntryKind::Dir) {
        if let Some(m) = e.mode {
            set(&dest.join(&e.path), m)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_tree(_: &Path, _: &[Entry]) -> Result<()> {
    Err(Error::Unsupported("an iOS bundle's file tree (directories, symlinks, modes) is written on macOS only".into()))
}

// The fake device set uses Unix modes and symlinks, as a simulator's data directory has them.
#[cfg(all(test, unix))]
#[path = "ios_tests.rs"]
mod tests;
