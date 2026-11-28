//! AVDs as portable bundles (#170): a stopped AVD's simee disks, `config.ini` and `boot.prop`,
//! with no host path, and its system image named by content so another host (or this one, under
//! a new name) finds its own copy.

use std::path::{Path, PathBuf};

use sim_bundle::{AndroidImage, Captures, Contents, Device, GuestIdentity, Identity, Reader, Secret};
pub use sim_bundle::{Extracted, Manifest, Options, Summary};
use sim_core::platform::Platform;

use crate::{CLONED_DISKS, Env, Error, Result, held_lock, parse_ini, read, state_dir, valid_avd_name};

/// The image files whose BLAKE3 identifies a system image (those an image has).
pub const IMAGE_FILES: [&str; 4] = ["system.img", "vendor.img", "kernel-ranchu", "ramdisk.img"];

/// What the guest keeps in user data that makes it this device to its apps and services; an
/// import keeps them (`clone_avd`'s clones do too).
pub const GUEST_IDENTIFIERS: [&str; 3] = ["ro.serialno", "android_id", "per-app SSAIDs"];

/// What an import says about guest identity, in its result and in the manifest.
pub const DUPLICATED_IDENTITY: &str = "the import is the same device to the guest as the AVD it came from: ro.serialno, \
     android_id and per-app SSAIDs live in the copied user data";

/// An export's manifest and what it wrote.
#[derive(Clone, Debug)]
pub struct Exported {
    pub manifest: Manifest,
    pub summary: Summary,
}

/// An import's new AVD and what it wrote.
#[derive(Clone, Debug)]
pub struct Imported {
    pub name: String,
    pub dir: PathBuf,
    /// `image.sysdir.1`, relative to the SDK root.
    pub sysdir: String,
    pub extracted: Extracted,
    pub manifest: Manifest,
    /// The bundle's `config.ini` keys the import left out because they hold host paths
    /// (`sdcard.path`, which the stock emulator would attach read-write).
    pub dropped: Vec<String>,
    /// With `allow_mismatch`: the installed revision used instead of the bundle's.
    pub mismatch: Option<String>,
}

/// The system image an import uses ([`Env::find_image`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundImage {
    pub dir: PathBuf,
    /// Relative to the SDK root, with a trailing `/` (`image.sysdir.1`).
    pub rel: String,
    /// Set when only the revision matched (allowed by `allow_mismatch`): what differs.
    pub mismatch: Option<String>,
}

/// The paths an AVD bundle may hold: its config, its boot properties and the simee disks.
fn allowed_path(path: &str) -> bool {
    matches!(path, "config.ini" | "boot.prop") || path.strip_prefix("simee/").is_some_and(|d| CLONED_DISKS.contains(&d))
}

/// Whether a `config.ini` value is a path on this host: absolute, or from the home directory.
fn host_path(value: &str) -> bool {
    let v = value.trim();
    v.starts_with('/') || v.starts_with('~') || v.contains(":\\")
}

/// A relative path with `/` between its parts, as `image.sysdir.N` and the bundle spell it on
/// every host (`Path::display` would put a `\` there on Windows).
fn slashed(p: &Path) -> String {
    p.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// `config.ini` for a bundle: without `image.sysdir.N` (an import writes its own) and without
/// any key whose value is a host path (`skin.path`, `sdcard.path`, ...), which come back as the
/// dropped keys.
pub fn portable_config(text: &str) -> (String, Vec<String>) {
    let mut dropped = Vec::new();
    let kept = text
        .lines()
        .filter(|l| match l.split_once('=') {
            Some((k, _)) if k.trim().starts_with("image.sysdir.") => false,
            Some((k, v)) if host_path(v) => {
                dropped.push(format!("config.ini {} (a host path)", k.trim()));
                false
            }
            _ => true,
        })
        .map(|l| format!("{l}\n"))
        .collect();
    (kept, dropped)
}

/// `system-images/android-36/google_apis/arm64-v8a/` as its SDK package path.
fn package_of(rel: &str) -> String {
    rel.trim_matches('/').replace('/', ";")
}

impl Env {
    /// The identity of the system image in `sysdir`: `source.properties`'s API level, tag, ABI
    /// and revision, the BLAKE3 of each of [`IMAGE_FILES`] it has, and `build.prop`'s fingerprint.
    pub fn image_identity(&self, sysdir: &Path) -> Result<AndroidImage> {
        self.image_identity_with(sysdir, &|p| Ok(sim_bundle::hash_file(p)?))
    }

    /// [`Env::image_identity`] with `hash_file` as the hash of each image file (a cache of hashes
    /// that is sure the file is unchanged, for one).
    pub(crate) fn image_identity_with(&self, sysdir: &Path, hash_file: &dyn Fn(&Path) -> Result<String>) -> Result<AndroidImage> {
        let props = parse_ini(&read(&sysdir.join("source.properties"))?);
        let prop =
            |k: &str| props.get(k).cloned().ok_or_else(|| Error::Config(format!("{}: source.properties has no {k}", sysdir.display())));
        let mut files = std::collections::BTreeMap::new();
        for f in IMAGE_FILES {
            let p = sysdir.join(f);
            if p.is_file() {
                files.insert(f.to_owned(), hash_file(&p)?);
            }
        }
        let build = std::fs::read_to_string(sysdir.join("build.prop")).map(|t| parse_ini(&t)).unwrap_or_default();
        let fingerprint =
            build.get("ro.system.build.fingerprint").or_else(|| build.get("ro.build.fingerprint")).cloned().unwrap_or_default();
        let rel = self.sdk_root.as_deref().and_then(|r| sysdir.strip_prefix(r).ok()).map(slashed);
        Ok(AndroidImage {
            api: prop("AndroidVersion.ApiLevel")?,
            tag: prop("SystemImage.TagId")?,
            abi: prop("SystemImage.Abi")?,
            revision: prop("Pkg.Revision")?,
            package: rel.map(|r| package_of(&r)).unwrap_or_default(),
            files,
            fingerprint,
        })
    }

    /// The installed system image that is `want` by content, as its directory and its path
    /// relative to the SDK root. Images of another API level, tag, ABI or revision aren't hashed.
    /// With `allow_mismatch`, an image that differs only in revision (same API level, tag and
    /// ABI; the highest such revision) is used when none matches, and the result says so. None
    /// installed: an error naming the package and revision to install.
    pub fn find_image(&self, want: &AndroidImage, allow_mismatch: bool) -> Result<FoundImage> {
        let root = self.sdk_root.as_deref().ok_or_else(|| Error::Config("no Android SDK (set ANDROID_HOME)".into()))?;
        let dirs = |p: &Path| -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> =
                std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            v.sort();
            v
        };
        let mut near = Vec::new();
        // Same API level, tag and ABI, another revision: (revision, dir, rel).
        let mut other_revisions: Vec<(String, PathBuf, String)> = Vec::new();
        for sysdir in dirs(&root.join("system-images")).iter().flat_map(|a| dirs(a)).flat_map(|t| dirs(&t)) {
            let Ok(text) = std::fs::read_to_string(sysdir.join("source.properties")) else { continue };
            let props = parse_ini(&text);
            let is = |k: &str, v: &str| props.get(k).is_some_and(|x| x == v);
            if !(is("AndroidVersion.ApiLevel", &want.api) && is("SystemImage.TagId", &want.tag) && is("SystemImage.Abi", &want.abi)) {
                continue;
            }
            let rel = sysdir.strip_prefix(root).map(slashed).unwrap_or_default();
            if !is("Pkg.Revision", &want.revision) {
                let revision = props.get("Pkg.Revision").cloned().unwrap_or_else(|| "?".into());
                near.push(format!("{} is revision {revision}", package_of(&rel)));
                other_revisions.push((revision, sysdir.clone(), rel));
                continue;
            }
            let have = self.image_identity(&sysdir)?;
            if have.files == want.files && have.fingerprint == want.fingerprint {
                return Ok(FoundImage { dir: sysdir, rel: format!("{rel}/"), mismatch: None });
            }
            near.push(format!("{} is revision {} but its files differ", package_of(&rel), want.revision));
        }
        let numeric = |r: &str| r.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
        let another_revision = !other_revisions.is_empty();
        if allow_mismatch && let Some((revision, dir, rel)) = other_revisions.into_iter().max_by_key(|(r, ..)| numeric(r)) {
            let mismatch = format!(
                "the bundle was made on {} revision {}; this host has revision {revision}, used instead",
                package_of(&rel),
                want.revision
            );
            return Ok(FoundImage { dir, rel: format!("{rel}/"), mismatch: Some(mismatch) });
        }
        let package = if want.package.is_empty() {
            format!("system-images;android-{};{};{}", want.api, want.tag, want.abi)
        } else {
            want.package.clone()
        };
        let near = if near.is_empty() { String::new() } else { format!(" ({})", near.join("; ")) };
        let allow = if another_revision && !allow_mismatch { "; or allow_mismatch to use the installed revision" } else { "" };
        Err(Error::Config(format!(
            "no installed system image matches the bundle's (API {} {} {} revision {}, {}); install {package} revision {}{near}{allow}",
            want.api, want.tag, want.abi, want.revision, want.fingerprint, want.revision
        )))
    }

    /// Exports AVD `name`, stopped, to a bundle at `to` (a `.tar` file, else a new directory):
    /// its simee disks ([`CLONED_DISKS`]), `config.ini` without host paths ([`portable_config`])
    /// and `boot.prop`. Its instance lock is held throughout, so it can't start meanwhile; a
    /// running AVD (simee's or the stock emulator's lock held) is refused. Stock's own files
    /// (qcow2 overlays, snapshots) never go in.
    pub fn export_avd(&self, name: &str, to: &Path, opts: &Options) -> Result<Exported> {
        let avd = self.load(name)?;
        if let Some(lock) = held_lock(&avd.dir) {
            return Err(Error::Config(format!("{name} is running ({} is held); stop it before exporting it", lock.display())));
        }
        let disks_dir = state_dir(&avd.dir);
        if !disks_dir.join("userdata.img").is_file() {
            return Err(Error::Config(format!("{name} has no simee user data yet (boot it once with emu, then stop it)")));
        }
        let lock_path = disks_dir.join("instance.lock");
        let lock = std::fs::File::create(&lock_path).map_err(|e| Error::Io(lock_path.clone(), e))?;
        if lock.try_lock().is_err() {
            return Err(Error::Config(format!("{name} is running; stop it before exporting it")));
        }
        let identity = self.image_identity(&avd.sysdir)?;
        let (config, dropped) = portable_config(&read(&avd.dir.join("config.ini"))?);
        let mut contents = Contents { disks: Vec::new(), files: vec![("config.ini".into(), config.into_bytes())] };
        let boot = avd.dir.join("boot.prop");
        if boot.is_file() {
            contents.files.push(("boot.prop".into(), std::fs::read(&boot).map_err(|e| Error::Io(boot.clone(), e))?));
        }
        for f in CLONED_DISKS {
            let p = disks_dir.join(f);
            if p.is_file() {
                contents.disks.push((format!("simee/{f}"), p));
            }
        }
        let secrets = vec![
            Secret {
                item: "simee/userdata.img".into(),
                what: "the guest's user data, encrypted and opaque: may hold accounts, tokens and app data".into(),
                excludable: false,
            },
            Secret { item: "simee/encryptionkey.img".into(), what: "the key to the user data's encryption".into(), excludable: false },
        ];
        let header = Manifest {
            format: sim_bundle::FORMAT,
            simee_version: env!("CARGO_PKG_VERSION").into(),
            platform: Platform::Android,
            device: Device { name: name.to_owned(), form_factor: Some(avd.profile().kind.form_factor.as_str().to_owned()) },
            identity: Identity::Android(identity),
            captures: Captures { disks: true, ram: false },
            resume: sim_bundle::COLD_BOOT.into(),
            secrets,
            guest_identity: GuestIdentity {
                on_import: "duplicated".into(),
                identifiers: GUEST_IDENTIFIERS.iter().map(|s| (*s).to_owned()).collect(),
            },
            dropped,
            chunk_size: 0,
            compression: String::new(),
            disks: Vec::new(),
            files: Vec::new(),
        };
        let (manifest, summary) = sim_bundle::export(header, &contents, to, opts)?;
        drop(lock);
        Ok(Exported { manifest, summary })
    }

    /// Imports the bundle at `from` as a new AVD `name`. The system image is found by identity
    /// ([`Env::find_image`], with `allow_mismatch` for a revision-only difference) before
    /// anything is written; a bundle that isn't an AVD's (another platform, a path outside
    /// `config.ini`, `boot.prop` and the simee disks) is refused. The bundle's `config.ini` is
    /// untrusted: keys holding host paths are dropped ([`portable_config`]) and reported, it is
    /// renamed to `name`, and `image.sysdir.1` points at the image relative to the SDK root.
    ///
    /// Everything is written into `<avd home>/.<name>.avd.partial` (a stale one from an
    /// interrupted import is removed first), verified, then renamed to `<name>.avd`; the pointer
    /// `<name>.ini` comes last. A failure at any step leaves nothing behind.
    ///
    /// The import keeps the guest identity of the AVD it came from ([`DUPLICATED_IDENTITY`]).
    pub fn import_avd(&self, from: &Path, name: &str, allow_mismatch: bool) -> Result<Imported> {
        if !valid_avd_name(name) {
            return Err(Error::Config(format!("invalid AVD name '{name}' (letters, digits, . _ -)")));
        }
        let dir = self.avd_home.join(format!("{name}.avd"));
        let pointer = self.avd_home.join(format!("{name}.ini"));
        if dir.exists() || pointer.exists() {
            return Err(Error::Config(format!("AVD '{name}' already exists")));
        }
        let reader = Reader::open(from)?;
        let m = reader.manifest().clone();
        if m.platform != Platform::Android {
            return Err(Error::Config(format!("the bundle is an {} device's; this imports Android AVDs", m.platform.label())));
        }
        let Identity::Android(image) = &m.identity;
        if let Some(p) = m.disks.iter().map(|d| &d.path).chain(m.files.iter().map(|f| &f.path)).find(|p| !allowed_path(p)) {
            return Err(Error::Config(format!("the bundle holds `{p}`, which isn't part of an AVD (config.ini, boot.prop, simee/<disk>)")));
        }
        if !m.files.iter().any(|f| f.path == "config.ini") {
            return Err(Error::Config("the bundle has no config.ini".into()));
        }
        let found = self.find_image(image, allow_mismatch)?;
        let sysdir = found.rel.clone();
        let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
        std::fs::create_dir_all(&self.avd_home).map_err(|e| io(&self.avd_home, e))?;
        let partial = self.avd_home.join(format!(".{name}.avd.partial"));
        if partial.exists() {
            std::fs::remove_dir_all(&partial).map_err(|e| io(&partial, e))?;
        }
        std::fs::create_dir(&partial).map_err(|e| io(&partial, e))?;
        let mut placed = false;
        let made = (|| -> Result<(Extracted, Vec<String>)> {
            let extracted = reader.extract(&partial)?;
            let cfg = partial.join("config.ini");
            let (config, dropped) = portable_config(&read(&cfg)?);
            let config: String = config
                .lines()
                .map(|l| match l.split_once('=').map(|(k, _)| k.trim()) {
                    Some("AvdId") => format!("AvdId={name}\n"),
                    Some("avd.ini.displayname") => format!("avd.ini.displayname={name}\n"),
                    _ => format!("{l}\n"),
                })
                .collect();
            std::fs::write(&cfg, format!("{config}image.sysdir.1={sysdir}\n")).map_err(|e| io(&cfg, e))?;
            // rename(2) replaces an empty directory: check again right before it.
            if dir.exists() || pointer.exists() {
                return Err(Error::Config(format!("AVD '{name}' already exists")));
            }
            std::fs::rename(&partial, &dir).map_err(|e| io(&dir, e))?;
            placed = true;
            let target = format!("android-{}", image.api);
            let body = format!("avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/{name}.avd\ntarget={target}\n", dir.display());
            std::fs::write(&pointer, body).map_err(|e| io(&pointer, e))?;
            self.load(name)?;
            Ok((extracted, dropped))
        })();
        match made {
            Ok((extracted, dropped)) => {
                Ok(Imported { name: name.to_owned(), dir, sysdir, extracted, manifest: m, dropped, mismatch: found.mismatch })
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&partial);
                if placed {
                    let _ = std::fs::remove_file(&pointer);
                    let _ = std::fs::remove_dir_all(&dir);
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewAvd;

    /// An SDK with one fake API 36 image and an AVD on it that has booted under simee once, its
    /// `config.ini` holding host paths as Android Studio writes them.
    fn setup(tag: &str) -> (PathBuf, Env) {
        let tmp = std::env::temp_dir().join(format!("emu-avd-bundle-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let sdk = tmp.join("sdk");
        let sys = sdk.join("system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&sys).expect("mkdir");
        for f in IMAGE_FILES {
            std::fs::write(sys.join(f), f.as_bytes()).expect("image file");
        }
        std::fs::write(
            sys.join("source.properties"),
            "Pkg.Revision=7\nAndroidVersion.ApiLevel=36\nSystemImage.Abi=arm64-v8a\nSystemImage.TagId=google_apis\n",
        )
        .expect("props");
        std::fs::write(sys.join("build.prop"), "ro.system.build.fingerprint=google/sdk/emu64a:16/X/1:userdebug/dev-keys\n").expect("build");
        let env = Env { sdk_root: Some(sdk.clone()), avd_home: tmp.join("avd") };
        let mut spec = NewAvd::new("golden", "system-images/android-36/google_apis/arm64-v8a");
        spec.boot_props = vec![("qemu.hw.mainkeys".into(), "1".into())];
        spec.extra = vec![
            ("skin.path".into(), tmp.join("sdk/skins/pixel").display().to_string()),
            ("sdcard.path".into(), tmp.join("avd/golden.avd/sdcard.img").display().to_string()),
            ("skin.name".into(), "pixel".into()),
        ];
        let dir = env.create_avd(&spec).expect("create");
        // config.ini as Android Studio leaves it: an absolute sysdir too.
        let cfg = dir.join("config.ini");
        let text = std::fs::read_to_string(&cfg).expect("config");
        std::fs::write(&cfg, format!("{text}image.sysdir.2={}/\n", sys.display())).expect("config");
        let disks = state_dir(&dir);
        std::fs::create_dir_all(&disks).expect("simee");
        let mut userdata = vec![0u8; 3 << 20];
        userdata[..5].copy_from_slice(b"hello");
        userdata[(2 << 20) + 7] = 1;
        std::fs::write(disks.join("userdata.img"), &userdata).expect("userdata");
        std::fs::write(disks.join("cache.img"), vec![0u8; 1 << 20]).expect("cache");
        std::fs::write(disks.join("encryptionkey.img"), b"key").expect("key");
        std::fs::write(disks.join("console.log"), b"log").expect("log");
        std::fs::write(dir.join("userdata-qemu.img.qcow2"), b"stock").expect("stock overlay");
        (tmp, env)
    }

    /// Every byte of every bundle file and of the manifest that could hold a host path: none
    /// names the temp root (the AVD home, the SDK, the AVD's directory are all under it).
    fn assert_no_host_path(tmp: &Path, bundle: &Path) {
        let reader = Reader::open(bundle).expect("open");
        let m = reader.manifest().clone();
        let out = tmp.join("check");
        std::fs::create_dir_all(&out).expect("dir");
        reader.extract(&out).expect("extract");
        let root = tmp.display().to_string();
        let manifest = String::from_utf8(m.to_json()).expect("utf8");
        assert!(!manifest.contains(&root) && !manifest.contains("/Users/") && !manifest.contains("/home/"), "{manifest}");
        for f in &m.files {
            let text = std::fs::read_to_string(out.join(&f.path)).expect("file");
            assert!(!text.contains(&root), "{}: {text}", f.path);
            for (k, v) in parse_ini(&text) {
                assert!(!host_path(&v), "{}: {k}={v}", f.path);
            }
        }
        let _ = std::fs::remove_dir_all(out);
    }

    #[test]
    fn exports_carry_no_host_path_and_import_under_a_new_name() {
        let (tmp, env) = setup("roundtrip");
        for to in ["golden.tar", "golden-dir"] {
            let bundle = tmp.join(to);
            let ex = env.export_avd("golden", &bundle, &Options::default()).expect("export");
            assert_no_host_path(&tmp, &bundle);
            let m = &ex.manifest;
            let Identity::Android(img) = &m.identity;
            assert_eq!(
                (img.api.as_str(), img.tag.as_str(), img.abi.as_str(), img.revision.as_str()),
                ("36", "google_apis", "arm64-v8a", "7")
            );
            assert_eq!(img.package, "system-images;android-36;google_apis;arm64-v8a");
            assert_eq!(img.files.len(), 4);
            assert_eq!(img.fingerprint, "google/sdk/emu64a:16/X/1:userdebug/dev-keys");
            assert_eq!((m.guest_identity.on_import.as_str(), m.captures.ram, m.resume.as_str()), ("duplicated", false, "cold-boot"));
            assert_eq!(m.dropped, ["config.ini skin.path (a host path)", "config.ini sdcard.path (a host path)"]);
            let paths: Vec<&str> = m.disks.iter().map(|d| d.path.as_str()).chain(m.files.iter().map(|f| f.path.as_str())).collect();
            assert_eq!(paths, ["simee/userdata.img", "simee/cache.img", "simee/encryptionkey.img", "config.ini", "boot.prop"]);
            assert!(m.secrets.iter().any(|s| s.item == "simee/encryptionkey.img"));
            assert_eq!(ex.summary.zero_bytes, (1 << 20) + (1 << 20), "userdata's middle chunk and all of cache.img");

            let name = format!("copy-{}", to.replace('.', "-"));
            let im = env.import_avd(&bundle, &name, false).expect("import");
            assert_eq!(im.sysdir, "system-images/android-36/google_apis/arm64-v8a/");
            let avd = env.load(&name).expect("load import");
            assert_eq!((avd.get("AvdId"), avd.get("avd.ini.displayname")), (Some(name.as_str()), Some(name.as_str())));
            assert_eq!(avd.get("image.sysdir.1"), Some("system-images/android-36/google_apis/arm64-v8a/"));
            assert_eq!((avd.get("image.sysdir.2"), avd.get("skin.path"), avd.get("skin.name")), (None, None, Some("pixel")));
            assert_eq!(avd.sysdir, env.sdk_root.as_ref().expect("sdk").join("system-images/android-36/google_apis/arm64-v8a"));
            assert_eq!(avd.boot_properties(), vec![("qemu.hw.mainkeys".to_string(), "1".to_string())]);
            for f in CLONED_DISKS {
                assert_eq!(
                    std::fs::read(state_dir(&avd.dir).join(f)).ok(),
                    std::fs::read(state_dir(&tmp.join("avd/golden.avd")).join(f)).ok(),
                    "{f}"
                );
            }
            assert!(!state_dir(&avd.dir).join("console.log").exists() && !avd.dir.join("userdata-qemu.img.qcow2").exists());
            let pointer = std::fs::read_to_string(env.avd_home.join(format!("{name}.ini"))).expect("pointer");
            assert!(pointer.contains(&format!("path.rel=avd/{name}.avd")) && pointer.contains("target=android-36"), "{pointer}");
            assert!(env.import_avd(&bundle, &name, false).expect_err("twice").to_string().contains("already exists"));
        }
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn running_avds_and_missing_images_are_refused() {
        let (tmp, env) = setup("refuse");
        let golden = tmp.join("avd/golden.avd");
        // Running under simee: its instance lock is held.
        let held = std::fs::File::create(state_dir(&golden).join("instance.lock")).expect("lock");
        held.lock().expect("lock");
        let err = env.export_avd("golden", &tmp.join("b.tar"), &Options::default()).expect_err("running");
        assert!(err.to_string().contains("is running"), "{err}");
        assert!(!tmp.join("b.tar").exists());
        drop(held);
        // Never booted under simee.
        env.create_avd(&NewAvd::new("fresh", "system-images/android-36/google_apis/arm64-v8a")).expect("fresh");
        assert!(
            env.export_avd("fresh", &tmp.join("f.tar"), &Options::default()).expect_err("fresh").to_string().contains("no simee user data")
        );

        env.export_avd("golden", &tmp.join("b.tar"), &Options::default()).expect("export");
        // The image changes: same API, tag, ABI and revision, other contents.
        let sys = tmp.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::write(sys.join("system.img"), b"rebuilt").expect("write");
        let err = env.import_avd(&tmp.join("b.tar"), "copy", false).expect_err("mismatch").to_string();
        assert!(err.contains("install system-images;android-36;google_apis;arm64-v8a revision 7") && err.contains("files differ"), "{err}");
        // Another revision installed instead.
        std::fs::write(sys.join("system.img"), b"system.img").expect("restore");
        let props = std::fs::read_to_string(sys.join("source.properties")).expect("props");
        std::fs::write(sys.join("source.properties"), props.replace("Pkg.Revision=7", "Pkg.Revision=8")).expect("props");
        let err = env.import_avd(&tmp.join("b.tar"), "copy", false).expect_err("revision").to_string();
        assert!(err.contains("revision 7") && err.contains("is revision 8") && err.contains("allow_mismatch"), "{err}");
        assert!(!env.avd_home.join("copy.avd").exists() && !env.avd_home.join("copy.ini").exists(), "nothing written");
        assert!(env.import_avd(&tmp.join("b.tar"), "../evil", false).is_err());
        // allow_mismatch takes the other revision of the same API, tag and ABI, and says so.
        let im = env.import_avd(&tmp.join("b.tar"), "copy", true).expect("revision allowed");
        let mismatch = im.mismatch.expect("recorded");
        assert!(mismatch.contains("revision 7") && mismatch.contains("revision 8"), "{mismatch}");
        assert_eq!(env.load("copy").expect("load").get("image.sysdir.1"), Some("system-images/android-36/google_apis/arm64-v8a/"));
        // ...but never another ABI (or API level, or tag).
        std::fs::write(sys.join("source.properties"), props.replace("SystemImage.Abi=arm64-v8a", "SystemImage.Abi=x86_64")).expect("props");
        assert!(env.import_avd(&tmp.join("b.tar"), "copy2", true).is_err());
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// The AVD home's entries other than `keep`, sorted.
    fn home_entries(env: &Env, keep: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&env.avd_home)
            .expect("home")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !keep.contains(&n.as_str()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_failed_import_leaves_nothing_and_untrusted_config_loses_host_paths() {
        let (tmp, env) = setup("atomic");
        let keep = ["golden.avd", "golden.ini"];
        let bundle = tmp.join("b");
        env.export_avd("golden", &bundle, &Options::default()).expect("export");
        // Corrupt one stored chunk of userdata: the import fails on its hash and leaves nothing,
        // neither the AVD, its pointer nor the partial directory it extracted into.
        let m = Reader::open(&bundle).expect("open").manifest().clone();
        let h = m.disks[0].extents[0].hash.clone().expect("a stored chunk");
        std::fs::write(bundle.join(sim_bundle::chunk_name(&h)), sim_bundle::compress(b"tampered")).expect("tamper");
        let err = env.import_avd(&bundle, "copy", false).expect_err("corrupt");
        assert!(err.to_string().contains("doesn't match its hash"), "{err}");
        assert_eq!(home_entries(&env, &keep), Vec::<String>::new());
        // A stale partial from an interrupted import doesn't block the next one.
        let _ = std::fs::remove_dir_all(&bundle);
        std::fs::create_dir_all(env.avd_home.join(".copy.avd.partial/simee")).expect("stale");
        std::fs::write(env.avd_home.join(".copy.avd.partial/simee/userdata.img"), b"half").expect("stale");

        // A bundle whose config.ini attaches a host file: dropped on import, and reported.
        let crafted = tmp.join("crafted");
        let cfg = tmp.join("avd/golden.avd/config.ini");
        let text = std::fs::read_to_string(&cfg).expect("config");
        let mut contents = Contents { disks: Vec::new(), files: Vec::new() };
        contents.files.push(("config.ini".into(), format!("{text}sdcard.path=/etc/hosts\n").into_bytes()));
        contents.disks.push(("simee/userdata.img".into(), state_dir(&tmp.join("avd/golden.avd")).join("userdata.img")));
        let header = env.export_avd("golden", &tmp.join("plain.tar"), &Options::default()).expect("export").manifest;
        sim_bundle::export(header, &contents, &crafted, &Options::default()).expect("crafted bundle");
        let im = env.import_avd(&crafted, "copy", false).expect("import");
        assert!(im.dropped.iter().any(|d| d.contains("sdcard.path")), "{:?}", im.dropped);
        let avd = env.load("copy").expect("load");
        assert!(avd.config.values().all(|v| !host_path(v)), "{:?}", avd.config);
        assert_eq!(home_entries(&env, &keep), ["copy.avd", "copy.ini"], "the stale partial is gone");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn host_paths_leave_config_ini() {
        let (text, dropped) = portable_config(
            "AvdId=a\nimage.sysdir.1=system-images/android-36/google_apis/arm64-v8a/\nskin.path=/Users/x/sdk/skins/p\nsdcard.path = ~/s.img\nskin.name=p\n",
        );
        assert_eq!(text, "AvdId=a\nskin.name=p\n");
        assert_eq!(dropped, ["config.ini skin.path (a host path)", "config.ini sdcard.path (a host path)"]);
        assert!(allowed_path("simee/userdata.img") && allowed_path("config.ini"));
        assert!(!allowed_path("simee/console.log") && !allowed_path("hardware-qemu.ini"));
    }
}
