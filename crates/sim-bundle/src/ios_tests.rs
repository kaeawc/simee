use super::*;
use std::cell::{Cell, RefCell};
use std::os::unix::fs::symlink;
use std::time::{Duration, Instant};

const RUNTIME: &str = "com.apple.CoreSimulator.SimRuntime.iOS-26-1";
const TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-17";
const SOURCE: &str = "0D5A6F1E-0000-4000-8000-000000000421";

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sim-bundle-ios-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("scratch");
    d
}

fn runtime(build: &str) -> Runtime {
    Runtime { identifier: RUNTIME.into(), name: "iOS 26.1".into(), version: "26.1".into(), build: build.into(), available: true }
}

/// A device set in a directory: devices are `<root>/<udid>/data`, as CoreSimulator lays them out.
struct Fake {
    root: PathBuf,
    devices: RefCell<Vec<Simulator>>,
    runtimes: Vec<Runtime>,
    /// After this many `device` calls, every device reads as booted (one that boots mid-export).
    boots_after: Cell<Option<usize>>,
    calls: Cell<usize>,
}

impl Fake {
    fn new(root: &Path, runtimes: Vec<Runtime>) -> Fake {
        Fake { root: root.into(), devices: RefCell::default(), runtimes, boots_after: Cell::new(None), calls: Cell::new(0) }
    }

    fn add(&self, udid: &str, name: &str, shut_down: bool) -> Simulator {
        let d = Simulator {
            udid: udid.into(),
            name: name.into(),
            shut_down,
            state: if shut_down { "Shutdown" } else { "Booted" }.into(),
            device_type: TYPE.into(),
            runtime: RUNTIME.into(),
            form_factor: Some("phone".into()),
            data_dir: self.root.join(udid).join("data"),
        };
        fs::create_dir_all(&d.data_dir).expect("data dir");
        fs::write(self.root.join(udid).join("device.plist"), b"plist").expect("device.plist");
        self.devices.borrow_mut().push(d.clone());
        d
    }
}

impl Service for Fake {
    fn device(&self, udid: &str) -> std::result::Result<Simulator, String> {
        self.calls.set(self.calls.get() + 1);
        let booted = self.boots_after.get().is_some_and(|n| self.calls.get() > n);
        let d = self.devices.borrow().iter().find(|d| d.udid == udid).cloned().ok_or_else(|| format!("no device matches `{udid}`"))?;
        Ok(if booted { Simulator { shut_down: false, state: "Booted".into(), ..d } } else { d })
    }

    fn names(&self) -> std::result::Result<Vec<String>, String> {
        Ok(self.devices.borrow().iter().map(|d| d.name.clone()).collect())
    }

    fn runtimes(&self) -> std::result::Result<Vec<Runtime>, String> {
        Ok(self.runtimes.clone())
    }

    fn create(&self, name: &str, device_type: &str, runtime: &Runtime) -> std::result::Result<Simulator, String> {
        assert_eq!((device_type, runtime.identifier.as_str()), (TYPE, RUNTIME));
        assert!(self.runtimes.contains(runtime), "{runtime:?} is one of the host's, its build included");
        let udid = format!("0D5A6F1E-0000-4000-8000-{:012}", self.devices.borrow().len());
        let d = self.add(&udid, name, true);
        // What a new device's data directory holds before its first boot: replaced by the import.
        fs::write(d.data_dir.join("fresh"), b"new").expect("fresh");
        Ok(d)
    }

    fn delete(&self, udid: &str) -> std::result::Result<(), String> {
        self.devices.borrow_mut().retain(|d| d.udid != udid);
        fs::remove_dir_all(self.root.join(udid)).map_err(|e| e.to_string())
    }
}

/// A source device's data: preferences, an app's bundle (an executable) and data container, the
/// keychain, caches and temporary files at each level, and three kinds of symlink.
fn populate(data: &Path) {
    let put = |rel: &str, bytes: &[u8]| {
        let p = data.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(&p, bytes).expect("write");
    };
    put("Library/Preferences/com.apple.springboard.plist", b"prefs");
    put("Library/Keychains/keychain-2.db", b"keychain");
    put("Library/Caches/com.apple.big/cache.bin", &[7u8; 4096]);
    put("tmp/scratch", b"tmp");
    put("Containers/Bundle/Application/B1/Demo.app/Demo", b"\xcf\xfa\xed\xfe binary");
    put("Containers/Bundle/Application/B1/Demo.app/Info.plist", b"info");
    put("Containers/Data/Application/A1/Documents/notes.txt", b"my notes");
    put("Containers/Data/Application/A1/Library/Caches/img.png", b"cached");
    put("Containers/Data/Application/A1/tmp/upload.part", b"part");
    put("Containers/Data/Application/A1/Library/Application Support/empty", b"");
    fs::create_dir_all(data.join("Media/DCIM")).expect("empty dir");
    let exe = data.join("Containers/Bundle/Application/B1/Demo.app/Demo");
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).expect("chmod");
    symlink("Preferences", data.join("Library/Prefs")).expect("relative link");
    symlink(data.join("Containers/Data/Application/A1/Documents"), data.join("Library/docs")).expect("absolute link into the data");
    symlink("/Users/someone/secret", data.join("Library/host")).expect("host link");
}

/// Every path under `root` with its kind, size, mode, mtime and contents (links not followed).
fn snapshot(root: &Path) -> Vec<(String, String, u64, u32, std::time::SystemTime, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut names: Vec<_> = fs::read_dir(&dir).expect("read_dir").map(|e| e.expect("entry").path()).collect();
        names.sort();
        for p in names {
            let m = fs::symlink_metadata(&p).expect("meta");
            let rel = p.strip_prefix(root).expect("under").display().to_string();
            let (kind, bytes) = if m.file_type().is_symlink() {
                ("link", fs::read_link(&p).expect("link").display().to_string().into_bytes())
            } else if m.is_dir() {
                stack.push(p.clone());
                ("dir", Vec::new())
            } else {
                ("file", fs::read(&p).expect("read"))
            };
            out.push((rel, kind.into(), m.len(), m.permissions().mode(), m.modified().expect("mtime"), bytes));
        }
    }
    out.sort();
    out
}

fn bundle_bytes(to: &Path) -> Vec<u8> {
    if to.is_dir() {
        let mut all = fs::read(to.join(crate::MANIFEST)).expect("manifest");
        for (rel, kind, .., bytes) in snapshot(to) {
            all.extend(rel.into_bytes());
            if kind == "file" {
                all.extend(bytes);
            }
        }
        all
    } else {
        fs::read(to).expect("archive")
    }
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn the_manifest_names_type_runtime_and_build_and_leaves_caches_out() {
    let root = scratch("manifest");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    let to = root.join("golden.tar");
    let ex = export(&fake, SOURCE, &to, &Options::default()).expect("export");
    let m = &ex.manifest;
    assert_eq!(m.platform, Platform::Ios);
    assert_eq!((m.device.name.as_str(), m.device.form_factor.as_deref()), ("golden", Some("phone")));
    let Identity::Ios(id) = &m.identity else { panic!("an iOS identity") };
    assert_eq!(
        (id.device_type.as_str(), id.runtime.as_str(), id.build.as_str(), id.version.as_str(), id.runtime_name.as_str()),
        (TYPE, RUNTIME, "23B80", "26.1", "iOS 26.1")
    );
    assert_eq!((m.captures.disks, m.captures.ram, m.resume.as_str()), (true, false, COLD_BOOT));
    assert!(m.secrets.iter().any(|s| s.item == "data/Library/Keychains"));
    let stored: Vec<&str> = m.disks.iter().map(|d| d.path.as_str()).collect();
    assert!(stored.contains(&"data/Containers/Data/Application/A1/Documents/notes.txt"), "{stored:?}");
    assert!(stored.contains(&"data/Library/Keychains/keychain-2.db"), "{stored:?}");
    assert!(stored.contains(&"data/Containers/Data/Application/A1/Library/Application Support/empty"), "{stored:?}");
    for gone in ["Caches/", "/tmp/", "data/tmp/"] {
        assert!(!stored.iter().any(|p| p.contains(gone)), "{gone} stored: {stored:?}");
    }
    assert_eq!(
        ex.left_out,
        [
            "data/Containers/Data/Application/A1/Library/Caches",
            "data/Containers/Data/Application/A1/tmp",
            "data/Library/Caches",
            "data/tmp"
        ]
    );
    // The left-out directories stay, empty; so do empty ones.
    let kind = |p: &str| m.tree.iter().find(|e| e.path == p).map(|e| e.kind);
    assert_eq!(kind("data/Library/Caches"), Some(EntryKind::Dir));
    assert_eq!(kind("data/Library/Caches/com.apple.big"), None);
    assert_eq!(kind("data/Media/DCIM"), Some(EntryKind::Dir));
    let exe = m.tree.iter().find(|e| e.path.ends_with("Demo.app/Demo")).expect("the executable");
    assert_eq!(exe.mode, Some(0o755));
    let target = |p: &str| m.tree.iter().find(|e| e.path == p).and_then(|e| e.target.clone());
    assert_eq!(target("data/Library/Prefs").as_deref(), Some("Preferences"));
    assert_eq!(target("data/Library/docs").as_deref(), Some("../Containers/Data/Application/A1/Documents"));
    assert_eq!(kind("data/Library/host"), None);
    assert_eq!(m.dropped, ["data/Library/host (a symlink to a host path)"]);
    // No host path anywhere in the bundle: not the set's, not the device's, not the link's.
    let bytes = bundle_bytes(&to);
    for host in [root.to_str().expect("utf-8"), "/Users/someone", SOURCE] {
        assert!(!contains(&bytes, host), "the bundle holds {host}");
    }
    assert_eq!(&Manifest::parse(&m.to_json()).expect("parses"), m);
    fs::remove_dir_all(root).ok();
}

#[test]
fn a_bundle_round_trips_into_a_new_device_with_its_files_modes_and_links() {
    let root = scratch("roundtrip");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    // Two files with the same contents are one chunk.
    fs::write(src.data_dir.join("Media/DCIM/a.jpg"), [3u8; 9000]).expect("a");
    fs::write(src.data_dir.join("Media/DCIM/b.jpg"), [3u8; 9000]).expect("b");
    for (to, name) in [("golden.tar", "copy"), ("golden-dir", "copy-dir")] {
        let to = root.join(to);
        let ex = export(&fake, SOURCE, &to, &Options::default()).expect("export");
        assert_eq!(ex.summary.unique_chunks + 1, ex.summary.chunks, "{:?}", ex.summary);
        let im = import(&fake, Reader::open(&to).expect("open"), name).expect("import");
        assert_eq!((im.device.name.as_str(), im.device.udid == SOURCE), (name, false));
        assert_eq!((im.runtime.build.as_str(), im.extracted.chunks), ("23B80", ex.summary.unique_chunks));
        let new = &im.device.data_dir;
        assert!(!new.join("fresh").exists(), "the new device's own data was replaced");
        assert!(!new.parent().expect("device dir").join(".simee-import.partial").exists());
        // The same tree, less the left-out contents and the host link.
        let want: Vec<_> = snapshot(&src.data_dir)
            .into_iter()
            .filter(|(rel, ..)| !ex.left_out.iter().any(|l| rel.starts_with(&format!("{}/", &l[DATA.len() + 1..]))))
            .filter(|(rel, ..)| rel != "Library/host")
            .map(|(rel, kind, len, mode, _, bytes)| match kind.as_str() {
                "link" if rel == "Library/docs" => (rel, kind, b"../Containers/Data/Application/A1/Documents".to_vec()),
                "link" => (rel, kind, bytes),
                "dir" => (rel, kind, mode.to_le_bytes().to_vec()),
                _ => (rel, kind, [bytes, len.to_le_bytes().to_vec(), mode.to_le_bytes().to_vec()].concat()),
            })
            .collect();
        let got: Vec<_> = snapshot(new)
            .into_iter()
            .map(|(rel, kind, len, mode, _, bytes)| match kind.as_str() {
                "link" => (rel, kind, bytes),
                "dir" => (rel, kind, mode.to_le_bytes().to_vec()),
                _ => (rel, kind, [bytes, len.to_le_bytes().to_vec(), mode.to_le_bytes().to_vec()].concat()),
            })
            .collect();
        assert_eq!(got, want);
        assert_eq!(fs::read(new.join("Library/docs/notes.txt")).expect("through the rewritten link"), b"my notes");
        // The name is taken now.
        let err = import(&fake, Reader::open(&to).expect("open"), name).expect_err("taken");
        assert!(err.to_string().contains("already exists"), "{err}");
    }
    fs::remove_dir_all(root).ok();
}

#[test]
fn a_booted_device_is_refused_and_nothing_is_written() {
    let root = scratch("booted");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", false);
    populate(&src.data_dir);
    let to = root.join("golden.tar");
    let err = export(&fake, SOURCE, &to, &Options::default()).expect_err("booted");
    assert_eq!(err.to_string(), "golden must be shut down to export it (it is Booted)");
    assert!(!to.exists());
    // One that boots while it is read: the bundle it wrote is removed.
    let fake = Fake::new(&root.join("set2"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    fake.boots_after.set(Some(1));
    let err = export(&fake, SOURCE, &to, &Options::default()).expect_err("booted meanwhile");
    assert!(err.to_string().contains("booted while it was exported"), "{err}");
    assert!(!to.exists());
    fs::remove_dir_all(root).ok();
}

#[test]
fn a_missing_runtime_is_refused_before_any_device_is_made() {
    let root = scratch("runtime");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    let to = root.join("golden.tar");
    export(&fake, SOURCE, &to, &Options::default()).expect("export");
    for (have, says) in [
        (vec![], "isn't installed;"),
        (vec![runtime("23B74")], "this host: iOS 26.1 is build 23B74"),
        (vec![Runtime { available: false, ..runtime("23B80") }], "iOS 26.1 (23B80) is unavailable"),
    ] {
        let other = Fake::new(&root.join("other"), have);
        let err = import(&other, Reader::open(&to).expect("open"), "copy").expect_err("no runtime").to_string();
        assert!(err.contains("needs the iOS 26.1 runtime, build 23B80") && err.contains(says), "{err}");
        assert!(err.contains("-downloadPlatform iOS -buildVersion 23B80"), "{err}");
        assert!(other.devices.borrow().is_empty(), "a device was made");
    }
    // The runtime of the device being exported must be installed to name its build.
    let lost = Fake::new(&root.join("lost"), vec![]);
    lost.add(SOURCE, "golden", true);
    let err = export(&lost, SOURCE, &root.join("lost.tar"), &Options::default()).expect_err("no runtime");
    assert!(err.to_string().contains("isn't installed here"), "{err}");
    fs::remove_dir_all(root).ok();
}

#[test]
fn export_never_writes_into_the_source() {
    let root = scratch("readonly");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    let device_dir = root.join("set").join(SOURCE);
    let before = snapshot(&device_dir);
    export(&fake, SOURCE, &root.join("golden.tar"), &Options::default()).expect("tar");
    export(&fake, SOURCE, &root.join("golden-dir"), &Options::default()).expect("dir");
    // Into the device's own directory, or its data: refused, nothing made.
    for inside in [device_dir.join("in.tar"), src.data_dir.join("in.tar"), src.data_dir.join("Library/in-dir")] {
        let err = export(&fake, SOURCE, &inside, &Options::default()).expect_err("inside");
        assert!(err.to_string().contains("never writes into the device"), "{err}");
    }
    assert_eq!(snapshot(&device_dir), before);
    fs::remove_dir_all(root).ok();
}

#[test]
fn imports_refuse_bundles_that_are_not_a_simulators_data() {
    let root = scratch("refuse");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    let to = root.join("golden-dir");
    let ex = export(&fake, SOURCE, &to, &Options::default()).expect("export");
    let rewrite = |f: &dyn Fn(&mut Manifest)| {
        let mut m = ex.manifest.clone();
        f(&mut m);
        fs::write(to.join(crate::MANIFEST), m.to_json()).expect("manifest");
        Reader::open(&to).map(|r| import(&fake, r, "copy"))
    };
    let err = rewrite(&|m| m.platform = Platform::Android).expect("opens").expect_err("android");
    assert_eq!(err.to_string(), "the bundle is an Android device's; this imports iOS simulators");
    let err = rewrite(&|m| m.tree.push(Entry { path: "elsewhere".into(), kind: EntryKind::Dir, mode: None, target: None }))
        .expect("opens")
        .expect_err("outside data");
    assert!(err.to_string().contains("isn't under data/"), "{err}");
    // A file under a symlink would be written through it: the manifest is refused as it opens.
    let err = rewrite(&|m| {
        let at = m.tree.iter().position(|e| e.path == "data/Library/Keychains").expect("dir");
        m.tree[at] = Entry { path: "data/Library/Keychains".into(), kind: EntryKind::Symlink, mode: None, target: Some("/etc".into()) };
    })
    .expect_err("refused");
    assert!(err.to_string().contains("lies under the symlink `data/Library/Keychains`"), "{err}");
    let err = rewrite(&|m| m.tree.retain(|e| e.kind != EntryKind::File || !e.path.ends_with("notes.txt"))).expect_err("refused");
    assert!(err.to_string().contains("notes.txt` is stored but not in the tree"), "{err}");
    assert_eq!(fake.devices.borrow().len(), 1, "nothing was made");
    fs::remove_dir_all(root).ok();
}

#[test]
fn caches_and_temporary_directories_are_left_out_where_ios_purges_them() {
    for rel in ["Library/Caches", "Containers/Data/Application/A1/Library/Caches", "tmp", "Containers/Data/Application/A1/tmp"] {
        assert!(left_out(rel), "{rel}");
    }
    for rel in ["Library", "Library/Preferences", "Caches", "Media/tmp", "Containers/Data/Application/A1/Documents/tmp"] {
        assert!(!left_out(rel), "{rel}");
    }
}

fn dir(path: &str) -> Entry {
    Entry { path: path.into(), kind: EntryKind::Dir, mode: Some(0o755), target: None }
}

fn link(path: &str, target: &str) -> Entry {
    Entry { path: path.into(), kind: EntryKind::Symlink, mode: None, target: Some(target.into()) }
}

/// The NFC and NFD spellings of `été`, which APFS treats as one name.
const NFC: &str = "\u{e9}t\u{e9}";
const NFD: &str = "e\u{301}te\u{301}";

#[test]
fn paths_fold_as_a_case_and_normalization_insensitive_volume_compares_them() {
    use crate::manifest::fold;
    assert_eq!(fold("data/Library/Prefs"), fold("DATA/library/PREFS"));
    assert_eq!(fold(&format!("data/{NFC}")), fold(&format!("data/{NFD}")));
    assert_eq!(fold(&format!("data/{}", NFC.to_uppercase())), fold(&format!("data/{NFD}")));
    assert_eq!(fold("data/a\u{200d}b"), fold("data/ab"), "a code point HFS+ ignores");
    // APFS takes the capital sharp s, the sharp s and `ss` for one name.
    assert_eq!(fold("data/\u{1e9e}"), fold("data/ss"));
    assert_eq!(fold("data/\u{df}"), fold("data/SS"));
    assert_eq!(fold("data/stra\u{1e9e}e"), fold("data/STRASSE"));
    assert_ne!(fold("data/a"), fold("data/b"));
    assert_eq!(fold("data/a/b").matches('/').count(), 2, "folding keeps the components");
}

/// A tree whose entry reaches a symlink by another case or Unicode normalization of its path
/// (which a case-insensitive APFS volume follows), or that holds two entries that fold alike, is
/// refused as the manifest is read, whichever comes first in the tree.
#[test]
fn a_tree_that_reaches_a_link_by_another_spelling_is_refused() {
    let root = scratch("fold");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    let ex = export(&fake, SOURCE, &root.join("golden.tar"), &Options::default()).expect("export");
    let refused = |extra: Vec<Entry>| {
        let mut m = ex.manifest.clone();
        m.tree.extend(extra);
        Manifest::parse(&m.to_json()).expect_err("refused").to_string()
    };
    // The review's case: `data/l/evil` goes through the link `data/L` on APFS.
    let err = refused(vec![link("data/L", "../../../../../../LaunchAgents"), link("data/l/evil", "/etc/hosts")]);
    assert!(err.contains("`data/l/evil` lies under the symlink `data/L`"), "{err}");
    let err = refused(vec![link(&format!("data/{NFC}"), "/etc"), dir(&format!("data/{NFD}")), dir(&format!("data/{NFD}/x"))]);
    assert!(err.contains("are one path on a case-insensitive volume"), "{err}");
    let err = refused(vec![link(&format!("data/{NFC}"), "/etc"), dir(&format!("data/{NFD}/x"))]);
    assert!(err.contains(&format!("`data/{NFD}/x` lies under the symlink `data/{NFC}`")), "{err}");
    // The link after its sibling, and after an entry under it.
    let err = refused(vec![dir("data/docs"), link("data/Docs", "/etc")]);
    assert!(err.contains("`data/docs` and `data/Docs` are one path"), "{err}");
    let err = refused(vec![dir("data/m/x"), link("data/M", "/etc")]);
    assert!(err.contains("`data/m/x` lies under the symlink `data/M`"), "{err}");
    // Two directories alike when folded, no link at all.
    let err = refused(vec![dir("data/Docs"), dir("data/DOCS")]);
    assert!(err.contains("are one path"), "{err}");
    // The capital sharp s: `data/ẞ/x` is `data/ss/x` on APFS, refused here rather than when the
    // tree is applied to a device already made.
    let err = refused(vec![link("data/ss", "/etc"), dir("data/\u{1e9e}/x")]);
    assert!(err.contains("`data/\u{1e9e}/x` lies under the symlink `data/ss`"), "{err}");
    let err = refused(vec![link("data/\u{df}", "/etc"), dir("data/\u{1e9e}")]);
    assert!(err.contains("are one path"), "{err}");
    fs::remove_dir_all(root).ok();
}

/// Defence in depth: even a tree that got past validation writes, links and chmods nothing
/// outside its destination. Every directory is opened without following a link, by the bundle's
/// spelling or (on a case-insensitive volume) another.
#[test]
fn applying_a_tree_writes_nothing_through_a_link() {
    let root = scratch("nofollow");
    let (dest, outside) = (root.join("dest"), root.join("outside"));
    fs::create_dir_all(&dest).expect("dest");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("victim"), b"v").expect("victim");
    fs::set_permissions(outside.join("victim"), fs::Permissions::from_mode(0o600)).expect("chmod");
    let out = outside.to_str().expect("utf-8");
    apply_tree(&dest, &[dir("data"), link("data/L", out), link(&format!("data/{NFC}"), out)]).expect("links");
    let through = |tree: &[Entry]| match apply_tree(&dest, tree) {
        Err(Error::ThroughLink(at)) => Some(at),
        Err(e) => panic!("{e}"),
        Ok(()) => None,
    };
    for tree in [
        vec![link("data/L/evil", "/etc/hosts")],
        vec![dir("data/L/sub")],
        vec![Entry { path: "data/L/victim".into(), kind: EntryKind::File, mode: Some(0o777), target: None }],
        vec![Entry { path: "data/L".into(), kind: EntryKind::Dir, mode: Some(0o777), target: None }],
    ] {
        assert_eq!(through(&tree).as_deref(), Some("data/L"), "{tree:?}");
    }
    // Another case or normalization: refused on a case-insensitive volume, a directory of its
    // own on a case-sensitive one. Never a write outside.
    for tree in [vec![link("data/l/evil", "/etc/hosts")], vec![dir(&format!("data/{NFD}/x"))]] {
        through(&tree);
    }
    let b = crate::beneath::Beneath::new(&dest).expect("dest");
    assert!(matches!(b.create("data/L/new"), Err(Error::ThroughLink(at)) if at == "data/L"));
    let _ = b.create(&format!("data/{NFD}/new"));
    let _ = b.create("data/l/new");
    let names: Vec<_> = fs::read_dir(&outside).expect("outside").map(|e| e.expect("entry").file_name()).collect();
    assert_eq!(names, ["victim"], "nothing was written outside");
    assert_eq!(fs::metadata(outside.join("victim")).expect("victim").permissions().mode() & 0o7777, 0o600);
    fs::remove_dir_all(root).ok();
}

/// Links an export leaves out (to a host path, or climbing out of `data/`) are refused on
/// import: the device's daemons, running as the host user, would read and write through them.
#[test]
fn imports_refuse_links_out_of_the_data_directory() {
    let root = scratch("links");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    symlink("../../device.plist", src.data_dir.join("Library/up")).expect("climbing link");
    symlink("../Library/..", src.data_dir.join("Library/root")).expect("the data root");
    // Climbs out only through a link to `.`, which a lexical check doesn't see.
    symlink(".", src.data_dir.join("Library/self")).expect("a link to its own directory");
    symlink("self/self/../../../tmp", src.data_dir.join("Library/esc")).expect("climbing through a link");
    let to = root.join("golden-dir");
    let ex = export(&fake, SOURCE, &to, &Options::default()).expect("export");
    for gone in ["data/Library/up", "data/Library/esc"] {
        assert!(ex.manifest.dropped.contains(&format!("{gone} (a symlink to a host path)")), "{:?}", ex.manifest.dropped);
    }
    for stays in ["data/Library/root", "data/Library/self"] {
        assert!(ex.manifest.tree.iter().any(|e| e.path == stays), "{stays} stays");
    }
    let deep = format!("{}{}tmp", "a/".repeat(20), "../".repeat(20));
    for (extra, path, target) in [
        (None, "data/Library/agents", "/Users/someone/Library/LaunchAgents"),
        (None, "data/Library/tmp", "/tmp"),
        (None, "data/up", ".."),
        (None, "data/Library/up", "../Preferences/../../../device.plist"),
        // The review's bypasses: through the tree's own link to `.`, and host paths spelled
        // with `//`, `.`, the data volume's firmlink, another case, or `..`.
        (Some(link("data/a", ".")), "data/b", deep.as_str()),
        (Some(link("data/A", ".")), "data/b", "a/../tmp"),
        (None, "data/Library/agents", "//Users/someone/Library/LaunchAgents"),
        (None, "data/Library/agents", "/./Users/someone/Library/LaunchAgents"),
        (None, "data/Library/agents", "/System/Volumes/Data/Users/someone/Library/LaunchAgents"),
        (None, "data/Library/agents", "/system/volumes/data/private/tmp"),
        (None, "data/Library/agents", "/users/someone"),
        (None, "data/Library/agents", "/Library/../Users/someone"),
        (None, "data/Library/agents", "/.vol/16777234/2"),
        // Directories the host's users can write, or anyone can.
        (None, "data/Library/tmp", "/var/tmp/x"),
        (None, "data/Library/tmp", "/private/var/tmp"),
        (None, "data/Library/tmp", "/Library/Caches/com.apple.x"),
        (None, "data/Library/tmp", "/opt/homebrew/bin/brew"),
        (None, "data/Library/tmp", "/Applications/Demo.app"),
        (None, "data/root", "/opt"),
        // A path that holds a host root: `data/root/Users/...` reaches it.
        (None, "data/root", "/"),
        (None, "data/root", "/private"),
        (None, "data/root", "/System/Volumes"),
        (None, "data/root", "/System/Volumes/Data"),
        // Through an absolute link the tree keeps, and through a loop.
        (Some(link("data/sys", "/System/Library")), "data/b", "sys/../../Users/someone"),
        (None, "data/x", "x"),
        (None, "data/x", "Library/../x/y"),
    ] {
        let mut m = ex.manifest.clone();
        m.tree.extend(extra);
        m.tree.push(link(path, target));
        fs::write(to.join(crate::MANIFEST), m.to_json()).expect("manifest");
        let err = import(&fake, Reader::open(&to).expect("opens"), "copy").expect_err("refused").to_string();
        // The link named, not the one it goes through (which stays inside).
        assert!(err.contains(&format!("symlink `{path}` points at `{target}`")), "{err}");
        assert_eq!(fake.devices.borrow().len(), 1, "nothing was made for {target}");
    }
    // Links that stay inside the data directory, through the tree's own links, import.
    let mut m = ex.manifest.clone();
    m.tree.extend([
        link("data/a", "."),
        link("data/b", &format!("{}Library/../Library/Preferences", "a/".repeat(20))),
        link("data/c", "Library/Prefs/com.apple.springboard.plist"),
        link("data/d", "a/Library/self/self/../Library/Preferences"),
        link("data/sys", "/System/Library"),
        link("data/e", "sys/Frameworks"),
    ]);
    fs::write(to.join(crate::MANIFEST), m.to_json()).expect("manifest");
    let im = import(&fake, Reader::open(&to).expect("opens"), "copy").expect("in-tree links import");
    assert_eq!(fs::read(im.device.data_dir.join("c")).expect("through two links"), b"prefs");
    assert!(im.device.data_dir.join("d/com.apple.springboard.plist").is_file());
    // A bundle whose disks add up to more than an import would read back is refused as it opens.
    let mut m = ex.manifest.clone();
    m.disks[0].size = crate::MAX_SIZE + 1;
    m.disks[0].extents = vec![crate::Extent::zero(0, crate::MAX_SIZE + 1)];
    let err = Manifest::parse(&m.to_json()).expect_err("too big").to_string();
    assert!(err.contains("add up to more than"), "{err}");
    fs::remove_dir_all(root).ok();
}

/// The review's escape (#572), reproduced on APFS: a link named `Cookies` and a zero-width space
/// (a name the volume keeps apart from `Cookies`, which the bundle leaves out) to ten levels of
/// directories, and `evil` through `Cookies` and nine `..`s. A walk that took the link for
/// `Cookies` stayed in `data/`; on the disk `evil` dangles until something makes
/// `Library/Cookies`, and then is `~/Library/LaunchAgents`. Such a name is refused as the
/// manifest is read, and a walk follows a link only as the tree spells it: another spelling
/// that folds alike (`ı` for `i`, another case, a directory spelt two ways) is refused, as one
/// volume follows it and another doesn't.
#[test]
fn links_are_followed_only_as_the_tree_spells_them() {
    let root = scratch("spelling");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    populate(&src.data_dir);
    let to = root.join("golden-dir");
    let ex = export(&fake, SOURCE, &to, &Options::default()).expect("export");
    let deep = "a/".repeat(10);
    let evil = format!("Cookies/{}Library/LaunchAgents", "../".repeat(9));
    let zwsp = "data/Library/Cookies\u{200b}";
    // The walk itself, without the manifest's checks, doesn't take the link for `Cookies`.
    let tree = [dir("data"), dir("data/Library"), link(zwsp, &deep), link("data/Library/evil", &evil)];
    let refused: Vec<&str> = refused_links(&tree).iter().map(|e| e.path.as_str()).collect();
    assert_eq!(refused, ["data/Library/evil"]);
    // And the bundle is refused as it opens, for the name alone.
    let mut m = ex.manifest.clone();
    m.tree.extend([link(zwsp, &deep), link("data/Library/evil", &evil)]);
    let err = Manifest::parse(&m.to_json()).expect_err("refused").to_string();
    assert!(err.contains(&format!("unsafe path `{zwsp}`")), "{err}");
    for target in ["Preferences\u{feff}/../..".to_owned(), "a/".repeat(crate::MAX_TARGET)] {
        let mut m = ex.manifest.clone();
        m.tree.push(link("data/Library/x", &target));
        let err = Manifest::parse(&m.to_json()).expect_err("refused").to_string();
        assert!(err.contains("symlink `data/Library/x` needs a target"), "{err}");
    }
    // Spellings that fold alike but that a volume may keep apart.
    for extra in [
        vec![link("data/Library/Cook\u{131}es", &deep)],
        vec![link("data/Library/COOKIES", &deep)],
        vec![dir("data/Library/cookies"), dir("data/Library/Cookies/a")],
    ] {
        let mut m = ex.manifest.clone();
        m.tree.extend(extra);
        m.tree.push(link("data/Library/evil", &evil));
        fs::write(to.join(crate::MANIFEST), m.to_json()).expect("manifest");
        let err = import(&fake, Reader::open(&to).expect("opens"), "copy").expect_err("refused").to_string();
        assert!(err.contains(&format!("symlink `data/Library/evil` points at `{evil}`")), "{err}");
        assert_eq!(fake.devices.borrow().len(), 1, "nothing was made");
    }
    // As the tree spells it, the link is followed, and the target stays in data/.
    let mut m = ex.manifest.clone();
    m.tree.extend([link("data/Library/Cookies", &deep), link("data/Library/evil", &evil)]);
    fs::write(to.join(crate::MANIFEST), m.to_json()).expect("manifest");
    import(&fake, Reader::open(&to).expect("opens"), "copy").expect("stays inside");
    fs::remove_dir_all(root).ok();
}

/// A walk's work is linear in what it charges to the budget, and all of `refused_links`'s
/// walks together stop at [`WALK_BUDGET`]: the review's case, a target of a million components
/// (2 MB), took hours when each component re-folded the path walked so far.
#[test]
fn link_walks_take_linear_work_and_stop_at_a_budget() {
    let tree = [dir("data"), dir("data/d")];
    let names = Names::new(&tree);
    let n = 1_000_000;
    let target = format!("{}{}x", "d/".repeat(n), "../".repeat(n));
    let mut budget = usize::MAX;
    let start = Instant::now();
    assert!(!leaves_data(&names, "l", &target, &mut budget), "it comes back to data/");
    assert_eq!(usize::MAX - budget, "l".len() + target.len() + 1, "charged its own bytes");
    assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
    let mut budget = target.len();
    assert!(leaves_data(&names, "l", &target, &mut budget), "refused past its budget");
    // A chain of 30 links of 800-odd bytes each, and a thousand links into it: each takes about
    // 25 KB to walk, so the budget runs out after some 650 and the rest are refused.
    let long = format!("{}/", "d".repeat(200)).repeat(4);
    let mut tree = vec![dir("data")];
    for k in 0..30 {
        let next = if k == 29 { ".".to_owned() } else { format!("L{}", k + 1) };
        tree.push(link(&format!("data/L{k}"), &format!("{long}{}{next}", "../".repeat(4))));
    }
    tree.extend((0..1000).map(|i| link(&format!("data/h{i}"), "L0")));
    let start = Instant::now();
    let refused: Vec<&str> = refused_links(&tree).iter().map(|e| e.path.as_str()).collect();
    assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
    assert!(!refused.contains(&"data/h0") && refused.contains(&"data/h999"), "{} refused", refused.len());
    assert!((200..500).contains(&refused.len()), "{} refused", refused.len());
}

const LOW_FD_CHILD: &str = "SIM_BUNDLE_LOW_FD_CHILD";

/// With `RLIMIT_NOFILE` lowered below the number of files (a simee launchd starts has 256), an
/// export and an import of both forms still work: each file is opened, copied or hashed, and
/// closed in turn. The limit is lowered in a child process (this test binary, running only this
/// test), so other tests keep theirs.
#[test]
fn export_and_import_stream_more_files_than_the_open_file_limit() {
    if std::env::var_os(LOW_FD_CHILD).is_none() {
        let module = module_path!().split_once("::").map_or("", |(_, m)| m);
        let name = format!("{module}::export_and_import_stream_more_files_than_the_open_file_limit");
        let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args(["--exact", &name, "--test-threads=1"])
            .env(LOW_FD_CHILD, "1")
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && stdout.contains("1 passed"), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        return;
    }
    const LIMIT: u64 = 64;
    const FILES: usize = 300;
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    setrlimit(Resource::Nofile, Rlimit { current: Some(LIMIT), maximum: getrlimit(Resource::Nofile).maximum }).expect("setrlimit");
    let root = scratch("fds");
    let fake = Fake::new(&root.join("set"), vec![runtime("23B80")]);
    let src = fake.add(SOURCE, "golden", true);
    let file = |i: usize| src.data_dir.join(format!("Library/Preferences/d{}/f{i}.plist", i % 10));
    for i in 0..FILES {
        fs::create_dir_all(file(i).parent().expect("parent")).expect("dirs");
        fs::write(file(i), format!("preference {i}")).expect("write");
    }
    // The limit holds: these files can't all be open at once.
    let all: std::io::Result<Vec<fs::File>> = (0..FILES).map(|i| fs::File::open(file(i))).collect();
    assert!(all.is_err(), "{FILES} files opened under a limit of {LIMIT}");
    for (to, name) in [("golden.tar", "copy"), ("golden-dir", "copy-dir")] {
        let to = root.join(to);
        let ex = export(&fake, SOURCE, &to, &Options::default()).expect("export");
        assert_eq!(ex.manifest.disks.len(), FILES);
        let im = import(&fake, Reader::open(&to).expect("open"), name).expect("import");
        assert_eq!(im.extracted.chunks, FILES as u64);
        let rel = file(FILES - 1).strip_prefix(&src.data_dir).expect("under").to_path_buf();
        assert_eq!(fs::read(im.device.data_dir.join(rel)).expect("read"), format!("preference {}", FILES - 1).as_bytes());
    }
    fs::remove_dir_all(root).ok();
}
