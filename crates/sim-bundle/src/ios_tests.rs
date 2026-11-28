use super::*;
use std::cell::{Cell, RefCell};
use std::os::unix::fs::symlink;

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

    fn create(&self, name: &str, device_type: &str, runtime: &str) -> std::result::Result<Simulator, String> {
        assert_eq!((device_type, runtime), (TYPE, RUNTIME));
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
