use super::*;
use crate::testing::stored_zip;

const SYS_IMG: &str = include_str!("../testdata/sys-img2-3.xml");
const REPO: &str = include_str!("../testdata/repository2-3.xml");
const SYS_IMG_BASE: &str = "https://dl.google.com/android/repository/sys-img/google_apis/";
const HOST: Host = Host::MACOS_ARM64;

fn sys_img() -> Manifest {
    Manifest::parse(SYS_IMG, SYS_IMG_BASE).expect("parse sys-img fixture")
}

fn repo() -> Manifest {
    Manifest::parse(REPO, REPO_URL).expect("parse repository fixture")
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("emu-sdk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

#[test]
fn resolves_newest_stable_system_image() {
    let d = sys_img().resolve(&system_image_path("36", "google_apis", "arm64-v8a"), HOST).expect("resolve");
    // The beta r8 is newer but not stable.
    assert_eq!(d.revision, Revision { major: 7, ..Revision::default() });
    assert_eq!(d.url, format!("{SYS_IMG_BASE}arm64-v8a-36_r07.zip"));
    assert_eq!(d.size, 1_872_691_175);
    assert_eq!(d.sha1, "5a99183b6d924da606260e45fd41a3eb8eca6eb7");
    assert_eq!(d.api_level.as_deref(), Some("36"));
    assert_eq!(d.install_dir(), PathBuf::from("system-images/android-36/google_apis/arm64-v8a"));
    assert_eq!(d.file_name(), "arm64-v8a-36_r07.zip");

    // Duplicate stable entries: the higher revision wins regardless of order.
    let old = sys_img().resolve("system-images;android-24;google_apis;arm64-v8a", HOST).expect("resolve 24");
    assert_eq!(old.revision.major, 29);
}

#[test]
fn resolution_errors() {
    let m = sys_img();
    // Only a canary build exists.
    let e = m.resolve(&system_image_path("CANARY", "google_apis", "arm64-v8a"), HOST).expect_err("canary only");
    assert!(matches!(e, Error::NoArchive(..)), "{e}");
    assert!(e.to_string().contains("no stable archive") && e.to_string().contains("macosx/aarch64"), "{e}");
    let e = m.resolve(&system_image_path("99", "google_apis", "arm64-v8a"), HOST).expect_err("unknown");
    assert!(matches!(e, Error::UnknownPackage(..)), "{e}");
    // An x86-only archive is not for this host.
    assert!(matches!(repo().resolve("x86-only", HOST), Err(Error::NoArchive(..))));
}

#[test]
fn resolves_platform_tools_for_host() {
    let d = repo().resolve("platform-tools", HOST).expect("resolve");
    // The dev-channel 38.0.0-rc1 is skipped; the darwin archive is picked over linux/windows.
    assert_eq!(d.revision.to_string(), "37.0.3");
    assert_eq!(d.url, "https://dl.google.com/android/repository/platform-tools_r37.0.3-darwin.zip");
    assert_eq!(d.size, 16_597_626);
    assert_eq!(d.sha1, "d48920e2650321710b4d15d3acd9e277d59cd55b", "checksums are normalised to lowercase");
    assert_eq!(d.install_dir(), PathBuf::from("platform-tools"));

    // host-arch matters when present (and absolute archive URLs are kept as-is).
    let ndk = repo().resolve("ndk;23.0.7421159", HOST).expect("ndk");
    assert_eq!(ndk.url, "https://example.invalid/android-ndk-r23-darwin-aarch64.zip");
    let intel = Host { os: "macosx", arch: "x64" };
    assert!(repo().resolve("ndk;23.0.7421159", intel).expect("ndk x64").url.ends_with("darwin-x86_64.zip"));
    assert!(repo().resolve("platform-tools", Host { os: "linux", arch: "x64" }).expect("linux").url.ends_with("-linux.zip"));
}

#[test]
fn lists_system_images_for_host() {
    let m = sys_img();
    let all: Vec<String> = m.system_images(HOST, None, "arm64-v8a").into_iter().map(|d| format!("{} r{}", d.path, d.revision)).collect();
    assert_eq!(
        all,
        [
            "system-images;android-24;google_apis;arm64-v8a r29",
            "system-images;android-36;google_apis;arm64-v8a r7",
            "system-images;android-36;google_apis_ps16k;arm64-v8a r7",
        ]
    );
    let tagged = m.system_images(HOST, Some("google_apis"), "arm64-v8a");
    assert_eq!(tagged.len(), 2);
    let x86 = m.system_images(HOST, None, "x86_64");
    assert_eq!(x86.len(), 1);
    assert_eq!(x86[0].file_name(), "x86_64-36_r07.zip");

    // Platforms order numerically, extension and minor releases after their base.
    let mut platforms = ["android-CANARY", "android-37.0", "android-36.1", "android-36-ext19", "android-9", "android-36"];
    let key = |p: &&str| (SysImagePath { platform: p, tag: "t", abi: "a" }).api_key();
    platforms.sort_by_key(key);
    assert_eq!(platforms, ["android-9", "android-36", "android-36-ext19", "android-36.1", "android-37.0", "android-CANARY"]);
}

#[test]
fn lists_android_xr_images_by_path_or_package_tag() {
    let m =
        Manifest::parse(include_str!("../testdata/sys-img-google_xr.xml"), "https://dl.google.com/android/repository/sys-img/google_xr/")
            .expect("parse XR fixture");
    let paths =
        |tag| m.system_images(HOST, tag, "arm64-v8a").into_iter().map(|d| format!("{} r{}", d.path, d.revision)).collect::<Vec<_>>();
    // The dev-channel r8 of google-xr isn't stable.
    let all = [
        "system-images;android-34;android-xr-v3-playstore;arm64-v8a r3",
        "system-images;android-34;google-xr;arm64-v8a r7",
        "system-images;android-36;ai-glasses;arm64-v8a r6",
    ];
    assert_eq!(paths(None), all);
    // No path says `android-xr`: the headsets carry it as a package tag.
    assert_eq!(paths(Some("android-xr")), all[..2]);
    assert_eq!(paths(Some("google-xr")), all[1..2]);
    assert_eq!(paths(Some("ai-glasses")), all[2..]);
    let d = m.resolve("system-images;android-34;google-xr;arm64-v8a", HOST).expect("google-xr");
    assert_eq!(
        (d.tags.as_slice(), d.file_name()),
        (["android-xr".to_owned(), "google_apis_playstore".to_owned()].as_slice(), "xr-arm64-v8a-playstore-34_r07.zip")
    );
}

#[test]
fn revisions_order_and_print() {
    let r = |major, minor, micro, preview| Revision { major, minor, micro, preview };
    assert!(r(38, 0, 0, Some(1)) < r(38, 0, 0, None));
    assert!(r(37, 0, 3, None) < r(38, 0, 0, Some(1)));
    assert_eq!(r(7, 0, 0, None).to_string(), "7");
    assert_eq!(r(38, 0, 0, Some(2)).to_string(), "38.0.0-rc2");
}

#[test]
fn site_list_and_urls() {
    let xml = r#"<common:site-list xmlns:sdk="http://schemas.android.com/sdk/android/addons-list/5" xmlns:common="http://schemas.android.com/repository/android/sites-common/1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <site xsi:type="sdk:sysImgSiteType"><displayName>Android System Images</displayName><url>sys-img/android/sys-img2-3.xml</url></site>
  <site xsi:type="sdk:addonSiteType"><displayName>Some add-on</displayName><url>addon2-3.xml</url></site>
  <site xsi:type="sdk:sysImgSiteType"><displayName>Google APIs</displayName><url>sys-img/google_apis/sys-img2-3.xml</url></site>
</common:site-list>"#;
    assert_eq!(
        parse_site_list(xml, REPO_URL).expect("sites"),
        [format!("{REPO_URL}sys-img/android/sys-img2-3.xml"), format!("{REPO_URL}sys-img/google_apis/sys-img2-3.xml")]
    );
    assert_eq!(sys_img_url("default"), format!("{REPO_URL}sys-img/android/sys-img2-3.xml"));
    assert_eq!(sys_img_url("google_apis_ps16k"), format!("{REPO_URL}sys-img/google_apis/sys-img2-3.xml"));
    assert_eq!(sys_img_url("android-tv"), format!("{REPO_URL}sys-img/android-tv/sys-img2-3.xml"));
    for xr in ["google-xr", "android-xr", "android-xr-v3-playstore", "android-xr-preview-playstore", "ai-glasses"] {
        assert_eq!(sys_img_url(xr), format!("{REPO_URL}sys-img/google_xr/sys-img2-3.xml"), "{xr}");
    }
    assert_eq!(system_image_path("android-36.1", "google_apis", "arm64-v8a"), "system-images;android-36.1;google_apis;arm64-v8a");
    assert!(Manifest::parse("<not xml", REPO_URL).is_err());
}

#[test]
fn sha1_known_values() {
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    let dir = scratch("sha1");
    let f = dir.join("big");
    // Spans several read buffers.
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&f, &data).expect("write");
    assert_eq!(sha1_file(&f).expect("sha1"), sha1_hex(&data));
    let _ = std::fs::remove_dir_all(dir);
}

/// The whole install path, offline: curl fetches a `file://` URL, the archive is verified,
/// unpacked and moved into place.
#[test]
fn installs_from_a_local_archive() {
    let work = scratch("install");
    let zip = work.join("platform-tools_r37.0.3-darwin.zip");
    let archive = stored_zip(&[
        ("platform-tools/source.properties", b"Pkg.Revision=37.0.3\n"),
        ("platform-tools/adb", b"#!/bin/sh\necho fake adb\n"),
    ]);
    std::fs::write(&zip, &archive).expect("write zip");
    let bytes = std::fs::read(&zip).expect("read zip");

    let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
    sdk.quiet = true;
    let good = Download {
        path: "platform-tools".into(),
        revision: Revision { major: 37, minor: 0, micro: 3, preview: None },
        api_level: None,
        tags: Vec::new(),
        url: format!("file://{}", zip.display()),
        size: bytes.len() as u64,
        sha1: sha1_hex(&bytes),
    };

    // A bad checksum is refused and the partial download is discarded.
    let bad = Download { sha1: "0".repeat(40), ..good.clone() };
    let e = sdk.install(&bad).expect_err("sha1 mismatch");
    assert!(matches!(e, Error::Verify { what: "sha1", .. }), "{e}");
    assert!(!sdk.downloads().join("platform-tools_r37.0.3-darwin.zip.part").exists());
    assert!(!sdk.is_installed(Path::new("platform-tools")));

    let dest = sdk.install(&good).expect("install");
    assert_eq!(dest, sdk.root().join("platform-tools"));
    assert!(sdk.is_installed(Path::new("platform-tools")));
    assert!(dest.join("adb").is_file());
    assert!(!sdk.downloads().join("platform-tools_r37.0.3-darwin.zip.part").exists(), "archive removed after install");
    let leftovers: Vec<_> = std::fs::read_dir(sdk.downloads()).expect("ls").flatten().collect();
    assert!(leftovers.is_empty(), "no temp dirs left: {leftovers:?}");

    // Manifests are fetched once, then served from the cache.
    let mf = work.join("repository2-3.xml");
    std::fs::write(&mf, REPO).expect("write manifest");
    // `file:///C:/dir/x.xml` on Windows: a URL has slashes, and an absolute path starts with one.
    let slashed = |p: &Path| p.display().to_string().replace('\\', "/");
    let url = format!("file://{}{}", if cfg!(windows) { "/" } else { "" }, slashed(&mf));
    let m = sdk.manifest(&url).expect("manifest");
    assert_eq!(m.base_url, format!("file://{}{}/", if cfg!(windows) { "/" } else { "" }, slashed(&work)));
    std::fs::remove_file(&mf).expect("rm");
    assert_eq!(sdk.manifest(&url).expect("cached").packages.len(), m.packages.len());
    let _ = std::fs::remove_dir_all(work);
}

#[test]
fn resolves_emulator_for_host() {
    let d = repo().resolve(renderer::PACKAGE, HOST).expect("resolve emulator");
    // The dev-channel 37.4.1 is skipped; of the stable ones the newest wins, darwin/aarch64 archive.
    assert_eq!(d.revision.to_string(), "37.3.10");
    assert_eq!(d.url, "https://dl.google.com/android/repository/emulator-darwin_aarch64-16970787.zip");
    assert_eq!(d.size, 420_629_851);
    assert_eq!(d.sha1, "3ec739595d44092e835f006d806dd380bc8d6dd7");
    assert_eq!(renderer::dir(Path::new("/sdk"), d.revision), PathBuf::from("/sdk/simee/renderer/37.3.10"));
    let intel = Host { os: "macosx", arch: "x64" };
    assert!(repo().resolve(renderer::PACKAGE, intel).expect("x64").url.ends_with("darwin_x64-16970787.zip"));
    // Linux x64 gets its own archive (the .so renderer); there is none for arm64 Linux.
    let linux = Host { os: "linux", arch: "x64" };
    assert!(repo().resolve(renderer::PACKAGE, linux).expect("linux").url.ends_with("emulator-linux_x64-16970787.zip"));
    assert!(repo().resolve(renderer::PACKAGE, Host { os: "linux", arch: "aarch64" }).is_err());
}

#[test]
fn the_renderer_comes_from_the_package_this_host_can_load() {
    if cfg!(target_os = "macos") {
        assert_eq!((renderer::host(), renderer::BACKEND), (Host::MACOS_ARM64, "libgfxstream_backend.dylib"));
    } else if cfg!(target_os = "linux") {
        // Before: every host fetched Apple silicon's archive, whose .dylib Linux can't load.
        assert_eq!((renderer::host(), renderer::BACKEND), (Host::current(), "libgfxstream_backend.so"));
        assert_eq!(renderer::host().os, "linux");
    }
}

#[test]
fn revisions_parse() {
    for s in ["37.3.10", "7", "38.0.0-rc2", "36.1.0"] {
        let r: Revision = s.parse().expect(s);
        assert_eq!(r.to_string().parse::<Revision>().expect("round trip"), r, "{s}");
    }
    assert_eq!("37.3".parse::<Revision>().expect("two parts"), Revision { major: 37, minor: 3, micro: 0, preview: None });
    assert_eq!("38.0.0-rc2".parse::<Revision>().expect("rc").preview, Some(2));
    for bad in ["", "current", "37.x", "1.2.3.4", "37-rc", ".1"] {
        assert!(bad.parse::<Revision>().is_err(), "{bad}");
    }
    // Numeric, not lexicographic.
    assert!("37.3.9".parse::<Revision>().expect("a") < "37.3.10".parse::<Revision>().expect("b"));
}

#[test]
fn renderer_unzip_patterns() {
    let args: Vec<String> =
        renderer::unzip_args(Path::new("/dl/e.zip.part"), Path::new("/tmp/u")).iter().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(
        args,
        [
            "-q",
            "/dl/e.zip.part",
            "emulator/lib64/*",
            "emulator/LICENSE",
            "emulator/NOTICE.txt",
            "-x",
            "emulator/lib64/qt/*",
            "-d",
            "/tmp/u"
        ]
    );
    let m = renderer::Marker { revision: "37.3.10".parse().expect("rev"), sha1: "ab".repeat(20), url: "https://x/e.zip".into() };
    assert_eq!(m.to_string().parse::<renderer::Marker>().expect("marker round trip"), m);
    assert!("Pkg.Revision=37\n".parse::<renderer::Marker>().is_err());
}

/// A fake renderer install: `<sdk>/simee/renderer/<name>` with a marker for `rev` (if any).
fn fake_renderer(sdk: &Path, name: &str, rev: Option<&str>, backend: bool) -> PathBuf {
    let dir = renderer::root(sdk).join(name);
    std::fs::create_dir_all(dir.join("lib64")).expect("mkdir");
    if backend {
        std::fs::write(dir.join("lib64").join(renderer::BACKEND), b"").expect("write");
    }
    if let Some(rev) = rev {
        let m = renderer::Marker { revision: rev.parse().expect("rev"), sha1: "0".repeat(40), url: "file:///e.zip".into() };
        std::fs::write(dir.join(renderer::MARKER), m.to_string()).expect("marker");
    }
    dir
}

#[test]
fn renderer_lookup_preference() {
    let sdk = scratch("lookup");
    assert_eq!(renderer::find_lib64(&sdk), None);
    fake_renderer(&sdk, "37.3.9", Some("37.3.9"), true);
    let newest = fake_renderer(&sdk, "37.3.10", Some("37.3.10"), true);
    // Not complete installs: no marker, a marker for another revision, no backend, a stray name.
    fake_renderer(&sdk, "37.4.0", None, true);
    fake_renderer(&sdk, "37.5.0", Some("37.3.9"), true);
    fake_renderer(&sdk, "38.0.0", Some("38.0.0"), false);
    fake_renderer(&sdk, "junk", Some("39"), true);
    symlink_dir(Path::new("37.3.9"), &renderer::root(&sdk).join(renderer::CURRENT)).expect("symlink");
    let revs: Vec<String> = renderer::installed(&sdk).iter().map(|i| i.marker.revision.to_string()).collect();
    assert_eq!(revs, ["37.3.10", "37.3.9"]);
    assert_eq!(renderer::find_lib64(&sdk), Some(newest.join("lib64")));

    // An emulator package without the renderer doesn't count...
    let emu_lib64 = sdk.join("emulator/lib64");
    std::fs::create_dir_all(&emu_lib64).expect("mkdir");
    assert_eq!(renderer::find_lib64(&sdk), Some(newest.join("lib64")));
    // ...one with it wins over simee's own copies.
    std::fs::write(emu_lib64.join(renderer::BACKEND), b"").expect("write");
    assert_eq!(renderer::sdk_emulator_lib64(&sdk), Some(emu_lib64.clone()));
    assert_eq!(renderer::find_lib64(&sdk), Some(emu_lib64));
    let _ = std::fs::remove_dir_all(sdk);
}

/// Whether every host tool a test shells out to is present (an absolute path, or a name looked up
/// on `PATH`). A missing tool in CI panics, since CI images have them and a broken image must not
/// read as a pass; elsewhere it prints a `SKIP` line to stderr and returns false.
#[cfg(unix)]
fn have_tools(test: &str, tools: &[&str]) -> bool {
    let found = |tool: &str| {
        let path = Path::new(tool);
        if path.is_absolute() {
            return path.is_file();
        }
        std::env::var_os("PATH").is_some_and(|dirs| std::env::split_paths(&dirs).any(|dir| dir.join(tool).is_file()))
    };
    let Some(missing) = tools.iter().find(|tool| !found(tool)) else { return true };
    let name = Path::new(missing).file_name().and_then(|n| n.to_str()).unwrap_or(missing);
    let shown = if Path::new(missing).is_absolute() { missing.to_string() } else { name.to_string() };
    assert!(std::env::var_os("CI").is_none(), "{test}: {shown} missing on a CI host (install {name} in the CI image)");
    eprintln!("SKIP {test}: {shown} missing (apt-get install {name})");
    false
}

/// Zips `<work>/<src>/emulator` with `/usr/bin/zip -y` (symlinks stored as links) and returns its
/// `Download` for revision `rev`.
#[cfg(unix)]
fn zip_emulator(work: &Path, src: &str, rev: &str) -> Download {
    let zip = work.join(format!("emulator-darwin_aarch64-{src}.zip"));
    let _ = std::fs::remove_file(&zip);
    let ok = Command::new("/usr/bin/zip").current_dir(work.join(src)).arg("-qry").arg(&zip).arg("emulator").status().expect("zip");
    assert!(ok.success());
    let bytes = std::fs::read(&zip).expect("read zip");
    Download {
        path: renderer::PACKAGE.into(),
        revision: rev.parse().expect("rev"),
        api_level: None,
        tags: Vec::new(),
        url: format!("file://{}", zip.display()),
        size: bytes.len() as u64,
        sha1: sha1_hex(&bytes),
    }
}

/// A tree shaped like the emulator package under `<work>/<src>/emulator`.
#[cfg(unix)]
fn fake_emulator_tree(work: &Path, src: &str, backend: bool) {
    let e = work.join(src).join("emulator");
    for d in ["lib64/qt/lib", "lib64/vulkan/layers", "lib64/gles_angle", "lib", "bin64"] {
        std::fs::create_dir_all(e.join(d)).expect("mkdir");
    }
    for (f, body) in [
        ("emulator", "#!/bin/sh\n"),
        ("source.properties", "Pkg.Path=emulator\n"),
        ("LICENSE", "license\n"),
        ("NOTICE.txt", "notice\n"),
        ("lib/advancedFeatures.ini", "x\n"),
        ("bin64/qemu-img", "x\n"),
        ("lib64/libemugl_common.dylib", "x"),
        ("lib64/libprotobuf.28.4.0.dylib", "protobuf"),
        ("lib64/qt/lib/QtCore.dylib", "qt"),
        ("lib64/vulkan/libvulkan.dylib", "x"),
        ("lib64/vulkan/MoltenVK_icd.json", "{}"),
        ("lib64/vulkan/layers/VkLayer_khronos_validation.json", "{}"),
        ("lib64/gles_angle/libEGL.dylib", "x"),
    ] {
        std::fs::write(e.join(f), body).expect("write");
    }
    if backend {
        std::fs::write(e.join("lib64").join(renderer::BACKEND), src).expect("write");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("libprotobuf.28.4.0.dylib", e.join("lib64/libprotobuf.dylib")).expect("symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_file("libprotobuf.28.4.0.dylib", e.join("lib64/libprotobuf.dylib")).expect("symlink");
}

/// Files under `dir`, relative, sorted; symlinks as `name -> target`.
#[cfg(unix)]
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).expect("ls").flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).expect("prefix").to_string_lossy().into_owned();
            let t = e.file_type().expect("type");
            if t.is_symlink() {
                out.push(format!("{rel} -> {}", std::fs::read_link(&p).expect("readlink").display()));
            } else if t.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The renderer install path, offline: only lib64 (minus Qt) and the license files are
/// unpacked, symlinks survive, a marker and `current` are written, the archive is deleted, and a
/// newer revision replaces the older one. Unix only: it zips with `/usr/bin/zip` and unpacks with
/// `unzip`, neither on a Windows host.
#[cfg(unix)]
#[test]
fn installs_renderer_from_a_local_archive() {
    if !have_tools("installs_renderer_from_a_local_archive", &["/usr/bin/zip", "curl", "unzip"]) {
        return;
    }
    let work = scratch("renderer");
    let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
    sdk.quiet = true;
    let current = renderer::root(sdk.root()).join(renderer::CURRENT);

    fake_emulator_tree(&work, "r1", true);
    let d = zip_emulator(&work, "r1", "37.3.10");
    let inst = sdk.install_renderer(&d).expect("install renderer");
    assert_eq!(inst.dir, sdk.root().join("simee/renderer/37.3.10"));
    assert_eq!((inst.marker.revision, inst.marker.sha1.as_str(), inst.marker.url.as_str()), (d.revision, d.sha1.as_str(), d.url.as_str()));
    // The backend is this host's (`.dylib` on macOS, `.so` elsewhere); the other files are the
    // fixture's own.
    let backend = format!("lib64/{}", renderer::BACKEND);
    let mut want = vec![
        "LICENSE",
        "NOTICE.txt",
        "lib64/gles_angle/libEGL.dylib",
        "lib64/libemugl_common.dylib",
        backend.as_str(),
        "lib64/libprotobuf.28.4.0.dylib",
        "lib64/libprotobuf.dylib -> libprotobuf.28.4.0.dylib",
        "lib64/vulkan/MoltenVK_icd.json",
        "lib64/vulkan/layers/VkLayer_khronos_validation.json",
        "lib64/vulkan/libvulkan.dylib",
        "renderer.properties",
    ];
    want.sort_unstable();
    assert_eq!(tree(&inst.dir), want);
    // Nothing that would make sdkmanager think an `emulator` package lives here.
    assert!(!inst.dir.join("source.properties").exists() && !sdk.root().join("emulator").exists());
    assert_eq!(std::fs::read_link(&current).expect("current"), PathBuf::from("37.3.10"));
    assert_eq!(renderer::find_lib64(sdk.root()), Some(inst.lib64()));
    assert!(renderer::disk_usage(&inst.dir) > 0);
    let leftovers: Vec<_> = std::fs::read_dir(sdk.downloads()).expect("ls").flatten().collect();
    assert!(leftovers.is_empty(), "archive and temp dirs removed: {leftovers:?}");

    // A newer revision takes over `current` and the old one is removed.
    fake_emulator_tree(&work, "r2", true);
    let inst2 = sdk.install_renderer(&zip_emulator(&work, "r2", "37.3.11")).expect("install newer");
    assert_eq!(renderer::installed(sdk.root()), std::slice::from_ref(&inst2));
    assert!(!inst.dir.exists());
    assert_eq!(std::fs::read_link(&current).expect("current"), PathBuf::from("37.3.11"));
    assert_eq!(std::fs::read_to_string(current.join("lib64").join(renderer::BACKEND)).expect("backend via current"), "r2");

    // An emulator archive without the renderer is refused and nothing changes.
    fake_emulator_tree(&work, "r3", false);
    assert!(matches!(sdk.install_renderer(&zip_emulator(&work, "r3", "40")), Err(Error::NoRenderer(..))));
    assert_eq!(renderer::installed(sdk.root()), [inst2]);
    assert!(!renderer::dir(sdk.root(), "40".parse().expect("rev")).exists());
    assert_eq!(std::fs::read_dir(sdk.downloads()).expect("ls").count(), 0, "useless archive deleted");
    let _ = std::fs::remove_dir_all(work);
}

#[test]
fn bare_api_levels_fall_back_to_the_newest_extension_level() {
    let m = Manifest::parse(
        include_str!("../testdata/sys-img-android-automotive.xml"),
        "https://dl.google.com/android/repository/sys-img/android-automotive/",
    )
    .expect("parse Automotive fixture");
    let path = |api: &str| m.resolve_system_image(api, "android-automotive", "arm64-v8a", HOST).map(|d| d.path);
    // Automotive has no plain API 34 or 35 image, only extension levels.
    assert_eq!(path("34").expect("34"), "system-images;android-34-ext9;android-automotive;arm64-v8a");
    assert_eq!(path("android-35").expect("35"), "system-images;android-35-ext15;android-automotive;arm64-v8a");
    assert_eq!(path("33").expect("33"), "system-images;android-33;android-automotive;arm64-v8a", "exact match first");
    assert_eq!(path("34-ext9").expect("explicit"), "system-images;android-34-ext9;android-automotive;arm64-v8a");
    assert!(matches!(path("34-ext8"), Err(Error::UnknownPackage(..))), "an explicit extension level is never swapped");
    assert!(matches!(path("3"), Err(Error::UnknownPackage(..))), "34-ext9 isn't an extension of 3");
    assert!(matches!(path("36"), Err(Error::UnknownPackage(..))), "36.1 is a dev-channel preview, not 36");
    let tv = m.resolve_system_image("34", "android-tv", "arm64-v8a", HOST);
    assert!(matches!(tv, Err(Error::UnknownPackage(..))), "only the asked tag");
}

#[test]
fn the_current_host_matches_the_build_target() {
    let host = Host::current();
    #[cfg(target_os = "linux")]
    assert_eq!(host.os, "linux");
    #[cfg(target_os = "macos")]
    assert_eq!(host.os, "macosx");
    #[cfg(target_os = "windows")]
    assert_eq!(host.os, "windows");
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    assert_eq!(host, Host::MACOS_ARM64);
    assert!(repo().resolve("platform-tools", Host { os: "linux", arch: "x64" }).is_ok());
}

#[test]
fn a_missing_host_tool_says_what_to_install() {
    let dir = std::env::temp_dir().join(format!("emu-sdk-missing-tool-{}", std::process::id()));
    let mut sdk = Sdk::new(&dir).expect("sdk");
    sdk.quiet = true;
    sdk.curl = "/nonexistent/curl".into();
    let err = sdk.manifest("https://example.invalid/repository2-3.xml").expect_err("no curl").to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(err.contains("`/nonexistent/curl` not found: install curl"), "{err}");
}

/// Bytes the filesystem allocated to a file (`st_blocks`); `None` where std can't tell.
fn allocated(meta: &std::fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    return Some(std::os::unix::fs::MetadataExt::blocks(meta) * 512);
    #[cfg(not(unix))]
    return {
        let _ = meta;
        None
    };
}

#[test]
fn write_sparse_skips_zero_blocks_and_keeps_contents() {
    let path = std::env::temp_dir().join(format!("emu-sdk-sparse-{}.img", std::process::id()));
    // 3 MiB: crosses the copy buffer, with data at a buffer edge and a short non-zero tail.
    let mut data = vec![0u8; 3 * (1 << 20) + 100];
    data[5 * 4096 + 7] = 1;
    data[(1 << 20) - 1] = 2; // the last byte of the first buffer
    data[1 << 20] = 3; // the first of the second
    data[3 << 20..].fill(0xff);
    let file = std::fs::File::create(&path).expect("create");
    let (len, written) = write_sparse(&data[..], &file).expect("write");
    drop(file);
    let back = std::fs::read(&path).expect("read");
    let allocated = allocated(&std::fs::metadata(&path).expect("meta"));
    let _ = std::fs::remove_file(&path);
    assert_eq!(back, data, "contents unchanged");
    assert_eq!((len, written), (data.len() as u64, 3 * 4096 + 100), "only the four non-zero blocks");
    if let Some(bytes) = allocated {
        assert!(bytes < 64 * 1024, "{bytes} bytes allocated for 3 MiB");
    }

    let empty = std::env::temp_dir().join(format!("emu-sdk-sparse-empty-{}.img", std::process::id()));
    let (len, written) = write_sparse(&[][..], &std::fs::File::create(&empty).expect("create")).expect("empty");
    let _ = std::fs::remove_file(&empty);
    assert_eq!((len, written), (0, 0));
}

/// A system image installs with its disk images sparse and everything else as unzip leaves it.
/// Unix only: it unpacks with `unzip`, which a Windows host doesn't have.
#[cfg(unix)]
#[test]
fn installs_system_images_with_sparse_disks() {
    if !have_tools("installs_system_images_with_sparse_disks", &["unzip"]) {
        return;
    }
    let work = scratch("sparse-install");
    let mut system = vec![0u8; 2 << 20];
    system[4096..8192].fill(7);
    let zip = stored_zip(&[
        ("arm64-v8a/source.properties", b"Pkg.Revision=3\n"),
        ("arm64-v8a/data/misc/x.xml", b"<x/>"),
        ("arm64-v8a/system.img", &system),
        ("arm64-v8a/vendor.img", b"tiny"),
    ]);
    let path = work.join("img.zip");
    std::fs::write(&path, &zip).expect("zip");
    let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
    sdk.quiet = true;
    let d = Download {
        path: "system-images;android-34;android-tv;arm64-v8a".into(),
        revision: Revision { major: 3, minor: 0, micro: 0, preview: None },
        api_level: Some("34".into()),
        tags: Vec::new(),
        url: format!("file://{}", path.display()),
        size: zip.len() as u64,
        sha1: sha1_hex(&zip),
    };
    let dest = sdk.install(&d).expect("install");
    assert_eq!(dest, sdk.root().join("system-images/android-34/android-tv/arm64-v8a"));
    assert_eq!(std::fs::read(dest.join("system.img")).expect("system"), system);
    assert_eq!(std::fs::read(dest.join("vendor.img")).expect("vendor"), b"tiny");
    assert_eq!(std::fs::read(dest.join("data/misc/x.xml")).expect("data"), b"<x/>");
    let allocated = allocated(&std::fs::metadata(dest.join("system.img")).expect("meta"));
    let leftovers: Vec<_> = std::fs::read_dir(sdk.downloads()).expect("ls").flatten().collect();
    let _ = std::fs::remove_dir_all(&work);
    if let Some(bytes) = allocated {
        assert!(bytes < 1 << 20, "a 2 MiB image with one data block takes {bytes} bytes");
    }
    assert!(leftovers.is_empty(), "no archive or temp dirs left: {leftovers:?}");
}

#[test]
fn tag_variants_are_found_on_whichever_site_lists_them() {
    // `android-wear-signed` images live on the `android-wear` site; there's no site of their own.
    let dir = std::env::temp_dir().join(format!("emu-sdk-sites-{}", std::process::id()));
    let mut sdk = Sdk::new(&dir).expect("sdk");
    sdk.quiet = true;
    sdk.curl = "/nonexistent/curl".into(); // offline: only cached manifests load
    let cache = |url: &str, text: &str| {
        let name: String = url.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        std::fs::create_dir_all(sdk.downloads()).expect("downloads");
        std::fs::write(sdk.downloads().join(name), text).expect("cache");
    };
    let wear_site = format!("{REPO_URL}sys-img/android-wear/sys-img2-3.xml");
    cache(
        &sites_url(),
        r#"<common:site-list xmlns:sdk="http://schemas.android.com/sdk/android/addons-list/5" xmlns:common="http://schemas.android.com/repository/android/sites-common/1" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <site xsi:type="sdk:sysImgSiteType"><displayName>Android TV</displayName><url>sys-img/android-tv/sys-img2-3.xml</url></site>
  <site xsi:type="sdk:sysImgSiteType"><displayName>Wear OS</displayName><url>sys-img/android-wear/sys-img2-3.xml</url></site>
</common:site-list>"#,
    );
    cache(&wear_site, &include_str!("../testdata/sys-img-android-automotive.xml").replace("android-automotive", "android-wear-signed"));
    let found = sdk.find_system_image("34", "android-wear-signed", "arm64-v8a", HOST).map(|d| d.path);
    let listed = sdk.system_image_manifests(Some("android-wear-signed")).map(|m| m.len());
    let missing = sdk.find_system_image("34", "android-tv", "arm64-v8a", HOST).map(|d| d.path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(found.expect("found"), "system-images;android-34-ext9;android-wear-signed;arm64-v8a");
    assert_eq!(listed.expect("listed"), 1, "the TV site isn't cached, so it's skipped");
    assert!(missing.is_err());
}

mod dhu_tests {
    use super::*;
    use crate::dhu::{self, Channel};

    const AUTO: &str = include_str!("../testdata/extras-google-auto.xml");
    const MAC_ARM: Host = Host::MACOS_ARM64;
    const MAC_X64: Host = Host { os: "macosx", arch: "x64" };
    const LINUX: Host = Host { os: "linux", arch: "x64" };
    const WINDOWS: Host = Host { os: "windows", arch: "x64" };
    const LINUX_ARM: Host = Host { os: "linux", arch: "aarch64" };

    fn auto() -> Manifest {
        Manifest::parse(AUTO, REPO_URL).expect("parse fixture")
    }

    #[test]
    fn picks_the_archive_for_each_host() {
        for (host, file) in [
            (MAC_ARM, "desktop-head-unit-mac-arm_r2.0.zip"),
            (MAC_X64, "desktop-head-unit-mac_r2.0.zip"),
            (LINUX, "desktop-head-unit-linux_r2.0.zip"),
            (WINDOWS, "desktop-head-unit-win_r2.0.zip"),
        ] {
            let plan = dhu::resolve(&auto(), host, Channel::Stable).expect("resolve");
            assert_eq!(plan.download.url, format!("{REPO_URL}{file}"), "{host}");
            assert_eq!(plan.download.install_dir(), PathBuf::from("extras/google/auto"));
        }
        let plan = dhu::resolve(&auto(), LINUX, Channel::Stable).expect("resolve");
        assert_eq!((plan.download.size, plan.download.sha1.len()), (1_000_002, 40));
        assert_eq!(plan.license.expect("license").id, "android-sdk-license");
    }

    #[test]
    fn stable_is_the_default_and_beta_is_opt_in() {
        assert_eq!(Channel::default(), Channel::Stable);
        let stable = dhu::resolve(&auto(), LINUX, Channel::Stable).expect("stable");
        assert_eq!(stable.download.revision.to_string(), "2");
        let beta = dhu::resolve(&auto(), LINUX, Channel::Beta).expect("beta");
        assert_eq!(beta.download.revision.to_string(), "2.1.0");
        assert!(beta.download.url.ends_with("desktop-head-unit-linux_r2.1.zip"));

        // With only a beta in the manifest, stable finds nothing and says so.
        let only_beta = AUTO.replacen("channel-0\"/>", "channel-1\"/>", 1);
        let m = Manifest::parse(&only_beta, REPO_URL).expect("parse");
        assert!(dhu::resolve(&m, LINUX, Channel::Stable).is_err());
        assert_eq!(dhu::resolve(&m, LINUX, Channel::Beta).expect("beta").download.revision.minor, 1);
    }

    #[test]
    fn a_host_without_an_archive_names_the_hosts_that_have_one() {
        let e = dhu::resolve(&auto(), LINUX_ARM, Channel::Stable).expect_err("no linux arm64 archive").to_string();
        assert!(e.contains("linux/aarch64"), "{e}");
        for have in ["macosx/aarch64", "macosx/x64", "linux/x64", "windows/x64"] {
            assert!(e.contains(have), "{e}");
        }
        let e = dhu::resolve(&Manifest::parse(REPO, REPO_URL).expect("repo"), LINUX, Channel::Stable).expect_err("not listed");
        assert!(matches!(e, Error::UnknownPackage(..)), "{e}");
    }

    /// A DHU download of `zip` bytes served from a file; `rev` names its file.
    fn served(work: &Path, zip: &[u8]) -> Download {
        let file = work.join("desktop-head-unit-linux_r2.0.zip");
        std::fs::write(&file, zip).expect("write zip");
        Download {
            path: dhu::PACKAGE.into(),
            revision: Revision { major: 2, ..Revision::default() },
            api_level: None,
            tags: Vec::new(),
            url: format!("file://{}", file.display()),
            size: zip.len() as u64,
            sha1: sha1_hex(zip),
        }
    }

    fn plan(download: Download) -> dhu::Plan {
        let license = Some(License { id: "android-sdk-license".into(), text: String::new() });
        dhu::Plan { download, license }
    }

    fn sdk_in(work: &Path) -> Sdk {
        let mut sdk = Sdk::new(work.join("sdk")).expect("sdk");
        sdk.quiet = true;
        sdk
    }

    fn dhu_zip() -> Vec<u8> {
        // Stored entries carry mode 0644: the install must add the executable bit.
        stored_zip(&[
            ("auto/package.xml", b"<package/>"),
            ("auto/desktop-head-unit", b"#!/bin/sh\necho fake dhu\n"),
            ("auto/config/default.ini", b"[dhu]\n"),
        ])
    }

    #[test]
    fn installs_atomically_with_an_executable_program_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let work = scratch("dhu-install");
        let bytes = dhu_zip();
        let p = plan(served(&work, &bytes));
        let sdk = sdk_in(&work);
        let mut seen = Vec::new();

        let dest = sdk.install_dhu(&p, &["android-sdk-license"]).expect("install");
        assert_eq!(dest, dhu::dir(sdk.root()));
        let exe = dhu::executable(sdk.root()).expect("installed program");
        assert_eq!(std::fs::metadata(&exe).expect("meta").permissions().mode() & 0o111, 0o111);
        assert!(sdk.is_installed(Path::new("extras/google/auto")));
        assert!(dest.join("config/default.ini").is_file());
        for e in std::fs::read_dir(sdk.downloads()).expect("ls").flatten() {
            seen.push(e.file_name());
        }
        assert!(seen.is_empty(), "archive and temp dirs removed: {seen:?}");

        // Installing again replaces the directory whole and leaves no set-aside copy.
        std::fs::write(dest.join("stale"), b"x").expect("stale");
        sdk.install_dhu(&p, &["android-sdk-license"]).expect("reinstall");
        assert!(!dest.join("stale").exists());
        let siblings: Vec<_> = std::fs::read_dir(dest.parent().expect("parent")).expect("ls").flatten().collect();
        assert_eq!(siblings.len(), 1, "{siblings:?}");
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn an_archive_with_its_files_at_the_top_installs_too() {
        let work = scratch("dhu-flat");
        let zip = stored_zip(&[("package.xml", b"<package/>"), ("desktop-head-unit", b"#!/bin/sh\n")]);
        let sdk = sdk_in(&work);
        sdk.install_dhu(&plan(served(&work, &zip)), &["android-sdk-license"]).expect("install");
        assert!(dhu::executable(sdk.root()).is_some());
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn nothing_installs_without_the_license() {
        let work = scratch("dhu-license");
        let p = plan(served(&work, &dhu_zip()));
        let sdk = sdk_in(&work);
        for accepted in [&[][..], &["some-other-license"][..]] {
            let e = sdk.install_dhu(&p, accepted).expect_err("license refused");
            assert!(matches!(e, Error::License(ref id) if id == "android-sdk-license"), "{e}");
        }
        assert!(!sdk.root().join("extras").exists() && !sdk.downloads().exists(), "refused before any download");
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn a_checksum_mismatch_installs_nothing_and_keeps_the_old_install() {
        let work = scratch("dhu-sha1");
        let bytes = dhu_zip();
        let good = plan(served(&work, &bytes));
        let sdk = sdk_in(&work);
        sdk.install_dhu(&good, &["android-sdk-license"]).expect("install");

        let mut bad = good.clone();
        bad.download.sha1 = "0".repeat(40);
        let e = sdk.install_dhu(&bad, &["android-sdk-license"]).expect_err("sha1");
        assert!(matches!(e, Error::Verify { what: "sha1", .. }), "{e}");
        let mut short = good.clone();
        short.download.size += 1;
        let e = sdk.install_dhu(&short, &["android-sdk-license"]).expect_err("size");
        assert!(matches!(e, Error::Verify { what: "size", .. }), "{e}");
        assert!(dhu::executable(sdk.root()).is_some(), "the existing install is untouched");
        assert!(std::fs::read_dir(sdk.downloads()).expect("ls").next().is_none(), "bad archives are discarded");
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn a_failed_prepare_leaves_the_existing_install_alone() {
        let work = scratch("dhu-prepare");
        let sdk = sdk_in(&work);
        let old = dhu::dir(sdk.root());
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::write(old.join("package.xml"), b"old").expect("write");
        // An archive without the program: making it executable fails before the move.
        let zip = stored_zip(&[("auto/package.xml", b"<package/>")]);
        let e = sdk.install_dhu(&plan(served(&work, &zip)), &["android-sdk-license"]).expect_err("no program");
        assert!(matches!(e, Error::Io(..)), "{e}");
        assert_eq!(std::fs::read(old.join("package.xml")).expect("read"), b"old");
        let _ = std::fs::remove_dir_all(work);
    }

    #[test]
    fn a_partial_download_resumes_and_an_oversized_one_restarts() {
        let work = scratch("dhu-resume");
        let bytes = dhu_zip();
        let p = plan(served(&work, &bytes));
        let sdk = sdk_in(&work);
        let part = sdk.downloads().join(format!("{}.part", p.download.file_name()));
        std::fs::create_dir_all(sdk.downloads()).expect("mkdir");

        std::fs::write(&part, &bytes[..bytes.len() / 2]).expect("partial");
        sdk.install_dhu(&p, &["android-sdk-license"]).expect("resumed");
        assert!(dhu::executable(sdk.root()).is_some());

        let _ = std::fs::remove_dir_all(dhu::dir(sdk.root()));
        let mut junk = bytes.clone();
        junk.extend_from_slice(b"trailing junk from another file");
        std::fs::write(&part, &junk).expect("oversized");
        sdk.install_dhu(&p, &["android-sdk-license"]).expect("restarted");
        assert!(dhu::executable(sdk.root()).is_some());

        // A same-size partial with the wrong bytes fails verification and is discarded.
        let _ = std::fs::remove_dir_all(dhu::dir(sdk.root()));
        std::fs::write(&part, vec![7u8; bytes.len()]).expect("corrupt");
        let e = sdk.install_dhu(&p, &["android-sdk-license"]).expect_err("corrupt");
        assert!(matches!(e, Error::Verify { what: "sha1", .. }), "{e}");
        assert!(!part.exists());
        sdk.install_dhu(&p, &["android-sdk-license"]).expect("clean retry");
        let _ = std::fs::remove_dir_all(work);
    }
}
