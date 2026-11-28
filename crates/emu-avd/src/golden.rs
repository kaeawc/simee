//! A settled "golden" userdata per system image (#226).
//!
//! A fresh AVD's first boot formats and encrypts user data, decompresses the image's compressed
//! APEX files one at a time and runs the first-boot work of `system_server`: 190 to 200 s more
//! than a warm boot in software mode (docs/research/software-mode-profile.md). The first AVD
//! made from an image boots once to the launcher and is stopped; its simee disks
//! ([`CLONED_DISKS`], the set `-clone-avd` copies) are kept here, and every later AVD of that
//! image starts from a copy-on-write clone of them.
//!
//! Layout, in `<avd home>/.simee-golden/` (the same file system as the AVDs, which reflinks need;
//! `list_avds` only lists `*.ini`, so the directory stays out of the way):
//!
//! ```text
//! <api>_<tag>_<abi>_<id>/        one golden: userdata.img, cache.img, encryptionkey.img, golden.ini
//! <api>_<tag>_<abi>_<id>.lock    held while that golden is built
//! .hashes/                       the hash of each image file, so an unchanged image isn't read again
//! .<name>.partial-<pid>/         a build in progress; renamed to <name> when complete
//! ```
//!
//! `<id>` is the BLAKE3 of the image's identity ([`Env::image_identity`], what #170's bundles
//! name an image by: API level, tag, ABI, revision, the hash of each image file and the build
//! fingerprint) and of the AVD's *shape*, the configuration a settled user data bakes in (data
//! partition size, screen, hardware keys, `boot.prop`). A changed image (a new revision, or the
//! same one with other contents) has another id, so the old golden isn't used and, once the new
//! one is built, is removed. AVDs of one image with another shape get their own golden.
//!
//! Concurrency: a build holds `<name>.lock` (an `flock`) from its check for an existing golden to
//! the rename that publishes it, so a second builder waits, then finds the golden and clones it.
//! A golden is a directory that only appears by rename, after `golden.ini` (written last) says
//! its disks are complete; a reader never sees a half-written one, and a killed build leaves only
//! a `.partial-*` directory, which the next build removes.
//!
//! Guest identity: like `-clone-avd` and a bundle import ([`crate::bundle::DUPLICATED_IDENTITY`]),
//! every AVD made from a golden is the same device to its guest: `android_id` and the per-app
//! SSAIDs live in the encrypted user data, which emu can't edit offline. `ro.serialno` is the
//! constant boot property `EMULATOR0EMURS0`, and the adb serial `emulator-<port>` is per launch.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Avd, CLONED_DISKS, Env, Error, Result, parse_ini, read, state_dir};

/// Where goldens live, under the AVD home.
pub const GOLDEN_DIR: &str = ".simee-golden";

/// Under [`GOLDEN_DIR`]: hashes of image files already read ([`Env::golden_key`]).
const HASH_CACHE: &str = ".hashes";

/// The `config.ini` keys whose values a settled user data depends on: the data partition's size
/// (the image is formatted at it), the screen (the launcher lays its grid out for it) and the
/// hardware the guest is told about. The AVD's `boot.prop` is part of the shape too. Memory, vCPU
/// count, GPU mode and names are not: they change nothing the guest writes.
pub const SHAPE_KEYS: [&str; 10] = [
    "abi.type",
    "tag.id",
    "disk.dataPartition.size",
    "hw.lcd.width",
    "hw.lcd.height",
    "hw.lcd.density",
    "hw.mainKeys",
    "hw.dPad",
    "hw.keyboard",
    "hw.sdCard",
];

/// What a golden says about guest identity, in `golden.ini` and in docs.
pub const DUPLICATED_IDENTITY: &str = "every AVD made from a golden is the same device to its guest: android_id and per-app SSAIDs \
     live in the copied, encrypted user data";

/// How a disk was copied, and the one function that copies them (`clone.rs`).
pub use crate::clone::{CloneKind, clone_file, clone_file_atomic};

/// What a golden is keyed by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenKey {
    /// The golden's directory name under [`Env::golden_root`].
    pub name: String,
    /// The image's package (its path under the SDK root, `system-images/android-36/default/x86_64`).
    pub package: String,
    /// BLAKE3 of the image's identity.
    pub image: String,
    /// BLAKE3 of the AVD's shape.
    pub shape: String,
}

/// A complete golden.
#[derive(Clone, Debug)]
pub struct Golden {
    pub dir: PathBuf,
    pub key: GoldenKey,
    /// The disks it holds, with their lengths.
    pub disks: Vec<(String, u64)>,
    /// Seconds since the Unix epoch when it was published.
    pub built_unix: u64,
}

/// Where a new AVD's user data came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Cloned from a golden that already existed (another AVD, or a concurrent build, made it).
    Cloned,
    /// This AVD booted, settled, and became the image's golden.
    Built,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    pub key: GoldenKey,
    pub source: Source,
    pub took: Duration,
    /// How the disks were copied: the golden's into this AVD (`Cloned`) or this AVD's into the
    /// golden (`Built`); the slowest kind of the three disks.
    pub copied: CloneKind,
    /// Seconds spent waiting for another process's build of the same golden.
    pub waited: Duration,
}

/// How long a second `-create-avd` of an image waits for the first one's build before it gives up
/// on the golden and keeps its fresh AVD (a software-mode build takes about 7 minutes).
pub const WAIT_LIMIT: Duration = Duration::from_secs(45 * 60);

fn fs_safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect()
}

fn canonical_image(i: &sim_bundle::AndroidImage) -> String {
    let files: String = i.files.iter().map(|(f, h)| format!("{f}={h}\n")).collect();
    format!("api={}\ntag={}\nabi={}\nrevision={}\nfingerprint={}\n{files}", i.api, i.tag, i.abi, i.revision, i.fingerprint)
}

fn shape_of(avd: &Avd) -> String {
    let mut s: String = SHAPE_KEYS.iter().map(|k| format!("{k}={}\n", avd.get(k).unwrap_or(""))).collect();
    s.push_str(&std::fs::read_to_string(avd.dir.join("boot.prop")).unwrap_or_default());
    s
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Env {
    /// `<avd home>/.simee-golden`.
    pub fn golden_root(&self) -> PathBuf {
        self.avd_home.join(GOLDEN_DIR)
    }

    /// The key of the golden for `avd`: its image's identity ([`Env::image_identity`]; this
    /// hashes the image's files) and its shape ([`SHAPE_KEYS`] and `boot.prop`).
    pub fn golden_key(&self, avd: &Avd) -> Result<GoldenKey> {
        let id = self.image_identity_with(&avd.sysdir, &|p| self.image_file_hash(p))?;
        let image = sim_bundle::hash(canonical_image(&id).as_bytes());
        let shape = sim_bundle::hash(shape_of(avd).as_bytes());
        let package = self
            .sdk_root
            .as_deref()
            .and_then(|r| avd.sysdir.strip_prefix(r).ok())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| avd.sysdir.display().to_string());
        let name = format!("{}_{}_{}_{}{}", fs_safe(&id.api), fs_safe(&id.tag), fs_safe(&id.abi), &image[..12], &shape[..8]);
        Ok(GoldenKey { name, package, image, shape })
    }

    /// The BLAKE3 of image file `path`, remembered in `<golden root>/.hashes/` under the file's
    /// path, size, inode and modification and change times: an image is multi-gigabyte, and
    /// hashing it takes as long as reading it from disk (20 s of a 25 s `-create-avd` that finds a
    /// golden). Any write to the file changes its change time, which a program can't set back, so
    /// an edited file is hashed again; a remembered hash that doesn't look like one is ignored.
    fn image_file_hash(&self, path: &Path) -> Result<String> {
        use std::os::unix::fs::MetadataExt;
        let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
        let meta = std::fs::metadata(path).map_err(|e| io(path, e))?;
        let real = std::fs::canonicalize(path).map_err(|e| io(path, e))?;
        let stamp = format!(
            "{}|{}|{}|{}.{}|{}.{}",
            real.display(),
            meta.len(),
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec()
        );
        let dir = self.golden_root().join(HASH_CACHE);
        let entry = dir.join(&sim_bundle::hash(stamp.as_bytes())[..32]);
        if let Ok(h) = std::fs::read_to_string(&entry)
            && h.len() == 64
            && h.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Ok(h);
        }
        let h = sim_bundle::hash_file(path)?;
        // Best effort: a read-only AVD home just hashes every time.
        if std::fs::create_dir_all(&dir).is_ok() {
            // Unique per write: threads of one process (and processes) hash the same file at once.
            static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp =
                dir.join(format!(".{}.{}.{n}", entry.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(), std::process::id()));
            if std::fs::write(&tmp, &h).is_ok() && std::fs::rename(&tmp, &entry).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        Ok(h)
    }

    /// The complete golden for `key`, if there is one. A directory whose `golden.ini` is missing
    /// or whose disks differ from it is not one (and is never produced by a build here).
    pub fn find_golden(&self, key: &GoldenKey) -> Option<Golden> {
        read_golden(&self.golden_root().join(&key.name))
    }

    /// Every complete golden.
    pub fn goldens(&self) -> Vec<Golden> {
        let mut v: Vec<Golden> = std::fs::read_dir(self.golden_root())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| read_golden(&e.path()))
            .collect();
        v.sort_by(|a, b| a.dir.cmp(&b.dir));
        v
    }

    /// Gives AVD `name` (made, never booted) user data that is already settled: its image's
    /// golden, cloned into `<avd>/simee/`, or, when the image has none yet, the result of
    /// `build`, which must boot the AVD to the launcher and stop it (cleanly), leaving its disks
    /// in `<avd>/simee/`; those are then kept as the image's golden. `say` reports progress
    /// (waiting for another build, starting this one).
    ///
    /// Two processes asking for the same golden at once build it once: the second waits for the
    /// first's lock ([`WAIT_LIMIT`]), then clones what it built. When `build` fails the AVD's
    /// disks are removed (it first-boots fresh, as with no golden) and the error returned.
    pub fn ensure_golden(&self, name: &str, build: impl FnOnce(&Avd) -> Result<()>, say: &mut dyn FnMut(&str)) -> Result<Outcome> {
        let started = Instant::now();
        let avd = self.load(name)?;
        let key = self.golden_key(&avd)?;
        let root = self.golden_root();
        let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
        std::fs::create_dir_all(&root).map_err(|e| io(&root, e))?;
        // A golden that exists needs no lock: it only ever appears whole, by rename.
        if let Some(g) = self.find_golden(&key) {
            let copied = apply(&g, &avd)?;
            return Ok(Outcome { key, source: Source::Cloned, took: started.elapsed(), copied, waited: Duration::ZERO });
        }
        let lock_path = root.join(format!("{}.lock", key.name));
        let lock = File::create(&lock_path).map_err(|e| io(&lock_path, e))?;
        let waited_from = Instant::now();
        let mut told = false;
        while lock.try_lock().is_err() {
            if !told {
                say(&format!("another emu is building the golden for this image ({}); waiting for it", key.name));
                told = true;
            }
            if waited_from.elapsed() > WAIT_LIMIT {
                return Err(Error::Config(format!("gave up waiting for the golden build holding {}", lock_path.display())));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let waited = if told { waited_from.elapsed() } else { Duration::ZERO };
        // The build we waited for, or another's, may have finished.
        if let Some(g) = self.find_golden(&key) {
            let copied = apply(&g, &avd)?;
            drop(lock);
            return Ok(Outcome { key, source: Source::Cloned, took: started.elapsed(), copied, waited });
        }
        // A killed build's leftovers; this lock says nobody is building this golden now.
        for e in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(&format!(".{}.partial-", key.name)) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        let stale = root.join(&key.name);
        if stale.exists() {
            let _ = std::fs::remove_dir_all(&stale);
        }
        say(&format!("no golden for this image yet: booting {name} once to settle it ({})", key.name));
        let disks = state_dir(&avd.dir);
        let built = build(&avd).and_then(|()| {
            if disks.join("userdata.img").is_file() {
                Ok(())
            } else {
                Err(Error::Config(format!("{name} stopped without user data in {}", disks.display())))
            }
        });
        if let Err(e) = built {
            for f in CLONED_DISKS {
                let _ = std::fs::remove_file(disks.join(f));
            }
            return Err(e);
        }
        let copied = publish(self, &key, &avd)?;
        // Images this one replaced (same package, other contents) can't be used again.
        for g in self.goldens() {
            if g.key.package == key.package && g.key.image != key.image {
                let _ = std::fs::remove_dir_all(&g.dir);
            }
        }
        drop(lock);
        Ok(Outcome { key, source: Source::Built, took: started.elapsed(), copied, waited })
    }
}

/// `<avd>/simee/`'s disks, kept under `key` (copied into a partial directory, then renamed).
/// The AVD is stopped: its instance lock is held for the copy.
fn publish(env: &Env, key: &GoldenKey, avd: &Avd) -> Result<CloneKind> {
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let src = state_dir(&avd.dir);
    let lock_path = src.join("instance.lock");
    let lock = File::create(&lock_path).map_err(|e| io(&lock_path, e))?;
    if lock.try_lock().is_err() {
        return Err(Error::Config(format!("{} is still running; its disks can't be kept as the golden", avd.name)));
    }
    let root = env.golden_root();
    let partial = root.join(format!(".{}.partial-{}", key.name, std::process::id()));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir(&partial).map_err(|e| io(&partial, e))?;
    let made = (|| {
        let mut disks = Vec::new();
        let mut kind = CloneKind::Reflink;
        for f in CLONED_DISKS {
            let from = src.join(f);
            if from.is_file() {
                kind = worst(kind, clone_file(&from, &partial.join(f))?);
                disks.push((f.to_owned(), std::fs::metadata(partial.join(f)).map_err(|e| io(&partial, e))?.len()));
            }
        }
        let list: Vec<String> = disks.iter().map(|(f, n)| format!("{f}:{n}")).collect();
        let body = format!(
            "disks={}\nimage={}\nshape={}\npackage={}\nsimee_version={}\nbuilt_unix={}\nguest_identity={DUPLICATED_IDENTITY}\ncomplete=yes\n",
            list.join(","),
            key.image,
            key.shape,
            key.package,
            env!("CARGO_PKG_VERSION"),
            now_unix()
        );
        let ini = partial.join("golden.ini");
        std::fs::write(&ini, body).map_err(|e| io(&ini, e))?;
        let dest = root.join(&key.name);
        std::fs::rename(&partial, &dest).map_err(|e| io(&dest, e))?;
        Ok::<_, Error>(kind)
    })();
    drop(lock);
    match made {
        Ok(kind) => match read_golden(&root.join(&key.name)) {
            Some(_) => Ok(kind),
            None => Err(Error::Config("the golden just published is incomplete".into())),
        },
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            Err(e)
        }
    }
}

/// The slower of two ways a disk was copied: a reflink beats a sparse copy, which beats a full
/// copy (it writes the holes out).
fn worst(a: CloneKind, b: CloneKind) -> CloneKind {
    let rank = |k| match k {
        CloneKind::Reflink => 0,
        CloneKind::Sparse => 1,
        CloneKind::Copy => 2,
    };
    if rank(b) > rank(a) { b } else { a }
}

/// Clones `golden`'s disks into AVD `avd`'s `simee/` directory. The AVD's instance lock is held,
/// so one that is running (or has user data already) is refused. Each disk is cloned under a
/// temporary name and renamed, so an interrupted clone leaves no half-written disk to boot.
fn apply(golden: &Golden, avd: &Avd) -> Result<CloneKind> {
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let dst = state_dir(&avd.dir);
    std::fs::create_dir_all(&dst).map_err(|e| io(&dst, e))?;
    let lock_path = dst.join("instance.lock");
    let lock = File::create(&lock_path).map_err(|e| io(&lock_path, e))?;
    if lock.try_lock().is_err() {
        return Err(Error::Config(format!("{} is running; it can't be given a golden's user data", avd.name)));
    }
    if dst.join("userdata.img").exists() {
        return Err(Error::Config(format!("{} already has user data; a golden only starts a new AVD", avd.name)));
    }
    let mut kind = CloneKind::Reflink;
    let result = (|| {
        for (f, _) in &golden.disks {
            let tmp = dst.join(format!(".{f}.partial"));
            let _ = std::fs::remove_file(&tmp);
            kind = worst(kind, clone_file(&golden.dir.join(f), &tmp)?);
            let to = dst.join(f);
            std::fs::rename(&tmp, &to).map_err(|e| io(&to, e))?;
        }
        Ok(kind)
    })();
    drop(lock);
    if result.is_err() {
        for f in CLONED_DISKS {
            let _ = std::fs::remove_file(dst.join(f));
            let _ = std::fs::remove_file(dst.join(format!(".{f}.partial")));
        }
    }
    result
}

/// The golden in `dir`, if complete: `golden.ini` says so and every disk it lists has its length.
fn read_golden(dir: &Path) -> Option<Golden> {
    let ini: BTreeMap<String, String> = parse_ini(&read(&dir.join("golden.ini")).ok()?);
    if ini.get("complete").map(String::as_str) != Some("yes") {
        return None;
    }
    let disks: Vec<(String, u64)> = ini
        .get("disks")?
        .split(',')
        .map(|d| d.split_once(':').and_then(|(f, n)| Some((f.to_owned(), n.parse().ok()?))))
        .collect::<Option<_>>()?;
    if !disks.iter().any(|(f, _)| f == "userdata.img") {
        return None;
    }
    for (f, n) in &disks {
        if !CLONED_DISKS.contains(&f.as_str()) || std::fs::metadata(dir.join(f)).ok()?.len() != *n {
            return None;
        }
    }
    Some(Golden {
        dir: dir.to_path_buf(),
        key: GoldenKey {
            name: dir.file_name()?.to_string_lossy().into_owned(),
            package: ini.get("package")?.clone(),
            image: ini.get("image")?.clone(),
            shape: ini.get("shape")?.clone(),
        },
        disks,
        built_unix: ini.get("built_unix").and_then(|v| v.parse().ok()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::NewAvd;
    use crate::bundle::IMAGE_FILES;

    const SYSDIR: &str = "system-images/android-36/default/x86_64";

    /// An SDK with one fake image (revision `rev`) and an environment on it.
    fn sdk(tag: &str, rev: &str) -> (PathBuf, Env) {
        let tmp = std::env::temp_dir().join(format!("emu-avd-golden-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let env = Env { sdk_root: Some(tmp.join("sdk")), avd_home: tmp.join("avd") };
        install_image(&env, rev, "system");
        (tmp, env)
    }

    fn install_image(env: &Env, rev: &str, system: &str) {
        let sys = env.sdk_root.as_deref().expect("sdk").join(SYSDIR);
        std::fs::create_dir_all(&sys).expect("mkdir");
        for f in IMAGE_FILES {
            std::fs::write(sys.join(f), if f == "system.img" { system.as_bytes() } else { f.as_bytes() }).expect("image file");
        }
        std::fs::write(
            sys.join("source.properties"),
            format!("Pkg.Revision={rev}\nAndroidVersion.ApiLevel=36\nSystemImage.Abi=x86_64\nSystemImage.TagId=default\n"),
        )
        .expect("props");
    }

    fn avd(env: &Env, name: &str) {
        env.create_avd(&NewAvd::for_image(name, SYSDIR)).expect("create");
    }

    /// What a booted guest leaves: a user data image that is mostly a hole.
    fn settle(avd: &Avd, marker: &str) -> Result<()> {
        let d = state_dir(&avd.dir);
        std::fs::create_dir_all(&d).expect("simee");
        let f = File::create(d.join("userdata.img")).expect("userdata");
        f.set_len(64 << 20).expect("len");
        std::os::unix::fs::FileExt::write_all_at(&f, marker.as_bytes(), 1 << 20).expect("write");
        std::fs::write(d.join("cache.img"), b"cache").expect("cache");
        std::fs::write(d.join("encryptionkey.img"), b"key").expect("key");
        std::fs::write(d.join("console.log"), b"log").expect("log");
        Ok(())
    }

    fn marker(avd_dir: &Path) -> String {
        let mut buf = vec![0u8; 8];
        let f = File::open(state_dir(avd_dir).join("userdata.img")).expect("open");
        std::os::unix::fs::FileExt::read_exact_at(&f, &mut buf, 1 << 20).expect("read");
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn the_first_avd_of_an_image_builds_the_golden_and_the_next_one_starts_from_it() {
        let (tmp, env) = sdk("flow", "2");
        avd(&env, "one");
        avd(&env, "two");
        let builds = AtomicUsize::new(0);
        let mut said = Vec::new();
        let one = env
            .ensure_golden(
                "one",
                |a| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    settle(a, "settled1")
                },
                &mut |m| said.push(m.to_owned()),
            )
            .expect("first");
        assert_eq!((one.source, builds.load(Ordering::SeqCst)), (Source::Built, 1));
        assert!(said.iter().any(|m| m.contains("booting one once")), "{said:?}");
        let key = one.key.clone();
        let g = env.find_golden(&key).expect("golden kept");
        assert_eq!(g.disks.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(), CLONED_DISKS);
        assert!(!g.dir.join("console.log").exists(), "only the cloned disks");
        // The golden is the settled disks, and the builder keeps its own.
        assert_eq!(marker(&env.load("one").expect("one").dir), "settled1");

        let two = env.ensure_golden("two", |_| panic!("the second AVD must not boot"), &mut |_| {}).expect("second");
        assert_eq!((two.source, two.key.clone()), (Source::Cloned, key));
        let two_dir = env.load("two").expect("two").dir;
        assert_eq!(marker(&two_dir), "settled1");
        assert_eq!(std::fs::read(state_dir(&two_dir).join("encryptionkey.img")).expect("key"), b"key");
        // The clone is its own: writing it leaves the golden alone, and it stays sparse.
        std::os::unix::fs::FileExt::write_all_at(
            &std::fs::OpenOptions::new().write(true).open(state_dir(&two_dir).join("userdata.img")).expect("open"),
            b"changed!",
            1 << 20,
        )
        .expect("write");
        assert_eq!(std::fs::read(g.dir.join("userdata.img")).expect("golden")[1 << 20..(1 << 20) + 8], *b"settled1");
        let blocks = std::fs::metadata(state_dir(&two_dir).join("userdata.img")).expect("meta").blocks() * 512;
        assert!(blocks < 16 << 20, "a 64 MiB image with 8 bytes of data takes {blocks} bytes on disk");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_changed_image_or_shape_gets_another_golden_and_the_replaced_image_loses_its_own() {
        let (tmp, env) = sdk("identity", "2");
        avd(&env, "a");
        let a = env.ensure_golden("a", |x| settle(x, "imageone"), &mut |_| {}).expect("a");
        // Another screen: the same image, another shape, another golden; both are kept.
        let mut spec = NewAvd::for_image("wide", SYSDIR);
        spec.lcd = (1080, 1920, 420);
        env.create_avd(&spec).expect("wide");
        let wide = env.ensure_golden("wide", |x| settle(x, "wideshap"), &mut |_| {}).expect("wide");
        assert_ne!(a.key.name, wide.key.name);
        assert_eq!(a.key.image, wide.key.image);
        assert_eq!(env.goldens().len(), 2);
        // Memory and cores aren't part of the shape.
        let mut spec = NewAvd::for_image("big", SYSDIR);
        spec.ram_mb = 4096;
        spec.cores = 8;
        env.create_avd(&spec).expect("big");
        let big = env.ensure_golden("big", |_| panic!("same shape"), &mut |_| {}).expect("big");
        assert_eq!((big.key.name, big.source), (a.key.name.clone(), Source::Cloned));

        // The image's contents change (the same package, a new revision): a new golden, and the
        // goldens of the replaced image are removed once it is built.
        install_image(&env, "3", "system, rebuilt");
        avd(&env, "b");
        let b = env.ensure_golden("b", |x| settle(x, "imagetwo"), &mut |_| {}).expect("b");
        assert_eq!(b.source, Source::Built);
        assert_ne!(b.key.image, a.key.image);
        assert!(env.find_golden(&b.key).is_some());
        assert!(env.find_golden(&a.key).is_none(), "the old image's golden is gone");
        assert!(env.find_golden(&wide.key).is_none());
        assert_eq!(env.goldens().len(), 1);
        // Same revision, other contents: also another identity.
        install_image(&env, "3", "system, patched");
        avd(&env, "c");
        let c = env.ensure_golden("c", |x| settle(x, "imagethr"), &mut |_| {}).expect("c");
        assert_eq!(c.source, Source::Built);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn two_creates_at_once_build_the_golden_once_and_nobody_reads_a_half_written_one() {
        let (tmp, env) = sdk("race", "2");
        let names: Vec<String> = (0..4).map(|i| format!("racer{i}")).collect();
        for n in &names {
            avd(&env, n);
        }
        let builds = AtomicUsize::new(0);
        let results: Vec<Outcome> = std::thread::scope(|s| {
            let handles: Vec<_> = names
                .iter()
                .map(|n| {
                    let (env, builds) = (&env, &builds);
                    s.spawn(move || {
                        env.ensure_golden(
                            n,
                            |a| {
                                builds.fetch_add(1, Ordering::SeqCst);
                                // A slow boot, and a slow write of the disks.
                                std::thread::sleep(Duration::from_millis(300));
                                settle(a, "racedata")
                            },
                            &mut |_| {},
                        )
                        .expect("ensure")
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        let diag = || {
            let ls: Vec<String> = std::fs::read_dir(env.golden_root())
                .expect("root")
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let ini: Vec<String> = env.goldens().iter().map(|g| g.dir.display().to_string()).collect();
            format!("outcomes {results:?}; root {ls:?}; goldens {ini:?}")
        };
        assert_eq!(builds.load(Ordering::SeqCst), 1, "one build: {}", diag());
        assert_eq!(results.iter().filter(|o| o.source == Source::Built).count(), 1);
        assert!(results.iter().any(|o| o.source == Source::Cloned && o.waited > Duration::ZERO), "others waited for the build");
        for n in &names {
            assert_eq!(marker(&env.load(n).expect("load").dir), "racedata", "{n}");
        }
        assert_eq!(env.goldens().len(), 1);
        let leftovers: Vec<_> = std::fs::read_dir(env.golden_root())
            .expect("root")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_failed_build_leaves_a_fresh_avd_and_no_golden_and_the_next_create_builds_again() {
        let (tmp, env) = sdk("fail", "2");
        avd(&env, "x");
        let err = env
            .ensure_golden(
                "x",
                |a| {
                    settle(a, "halfboot")?;
                    Err(Error::Config("boot timed out".into()))
                },
                &mut |_| {},
            )
            .expect_err("fails");
        assert!(err.to_string().contains("boot timed out"));
        let dir = env.load("x").expect("x").dir;
        assert!(!state_dir(&dir).join("userdata.img").exists(), "its half-booted disks are removed: it first-boots fresh");
        assert!(env.goldens().is_empty());
        // Stopped without leaving user data behind: also a failure.
        avd(&env, "y");
        assert!(env.ensure_golden("y", |_| Ok(()), &mut |_| {}).is_err());
        // A killed build's partial directory and an unfinished golden are never used, and are cleared.
        let root = env.golden_root();
        let key = env.golden_key(&env.load("x").expect("x")).expect("key");
        std::fs::create_dir_all(root.join(&key.name)).expect("unfinished");
        std::fs::write(root.join(&key.name).join("userdata.img"), b"torn").expect("torn");
        std::fs::create_dir_all(root.join(format!(".{}.partial-1", key.name))).expect("partial");
        assert!(env.find_golden(&key).is_none(), "no golden.ini, no golden");
        let built = env.ensure_golden("x", |a| settle(a, "second!!"), &mut |_| {}).expect("rebuild");
        assert_eq!(built.source, Source::Built);
        assert_eq!(marker(&env.load("x").expect("x").dir), "second!!");
        assert!(!root.join(format!(".{}.partial-1", key.name)).exists());
        assert!(env.find_golden(&key).is_some());
        // A golden whose disk no longer has the recorded length isn't one.
        std::fs::write(root.join(&key.name).join("cache.img"), b"longer than five").expect("truncate");
        assert!(env.find_golden(&key).is_none());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn image_hashes_are_remembered_until_the_file_changes() {
        let (tmp, env) = sdk("hashes", "2");
        avd(&env, "a");
        let a = env.load("a").expect("a");
        let first = env.golden_key(&a).expect("key");
        let cache = env.golden_root().join(".hashes");
        let entries = || -> Vec<PathBuf> { std::fs::read_dir(&cache).expect("cache").flatten().map(|e| e.path()).collect() };
        assert_eq!(entries().len(), IMAGE_FILES.len(), "one remembered hash per image file");
        assert_eq!(env.golden_key(&a).expect("again"), first);
        // The key is made from what is remembered: planted hashes change it.
        for e in entries() {
            std::fs::write(e, "a".repeat(64)).expect("plant");
        }
        assert_ne!(env.golden_key(&a).expect("planted").image, first.image);
        // Something that isn't a hash is ignored and replaced by the real one.
        for e in entries() {
            std::fs::write(e, "not a hash").expect("garbage");
        }
        assert_eq!(env.golden_key(&a).expect("healed"), first);
        // An edit in place that keeps the length and puts the modification time back is still
        // seen (its change time moved), so the golden of the old contents isn't used.
        let system = env.sdk_root.as_deref().expect("sdk").join(SYSDIR).join("system.img");
        let mtime = std::fs::metadata(&system).expect("meta").modified().expect("mtime");
        let len = std::fs::metadata(&system).expect("meta").len() as usize;
        std::fs::write(&system, vec![b'x'; len]).expect("edit");
        File::options().write(true).open(&system).expect("open").set_modified(mtime).expect("mtime back");
        assert_ne!(env.golden_key(&a).expect("edited").image, first.image);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn the_worst_copy_is_the_slowest_one() {
        use CloneKind::{Copy, Reflink, Sparse};
        for (a, b, want) in [
            (Reflink, Reflink, Reflink),
            (Reflink, Sparse, Sparse),
            (Sparse, Reflink, Sparse),
            (Sparse, Copy, Copy),
            (Copy, Sparse, Copy),
            (Reflink, Copy, Copy),
            (Copy, Reflink, Copy),
            (Copy, Copy, Copy),
        ] {
            assert_eq!(worst(a, b), want, "{a:?} then {b:?}");
        }
    }

    #[test]
    fn clone_file_keeps_holes_and_content() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-clonefile-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let src = tmp.join("src.img");
        let f = File::create(&src).expect("src");
        f.set_len(256 << 20).expect("len");
        let at = |pos: u64, b: &[u8]| std::os::unix::fs::FileExt::write_all_at(&f, b, pos).expect("write");
        at(0, b"head");
        at(100 << 20, b"middle");
        at((256 << 20) - 4, b"tail");
        let dst = tmp.join("dst.img");
        let kind = clone_file(&src, &dst).expect("clone");
        assert_eq!(std::fs::metadata(&dst).expect("meta").len(), 256 << 20);
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        let used = std::fs::metadata(&dst).expect("meta").blocks() * 512;
        assert!(kind != CloneKind::Sparse || used < 8 << 20, "{kind:?} took {used} bytes of a 256 MiB hole");
        assert!(clone_file(&src, &dst).is_err(), "never overwrites");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn threads_hashing_the_same_image_at_once_agree_on_the_key() {
        let (tmp, env) = sdk("hashrace", "2");
        avd(&env, "a");
        let a = env.load("a").expect("a");
        let keys: Vec<GoldenKey> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..8).map(|_| s.spawn(|| env.golden_key(&a).expect("key"))).collect();
            hs.into_iter().map(|h| h.join().expect("thread")).collect()
        });
        assert!(keys.windows(2).all(|w| w[0] == w[1]), "{keys:?}");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_failed_clone_leaves_no_partial_file() {
        let (tmp, env) = sdk("partial", "2");
        avd(&env, "a");
        let a = env.load("a").expect("a");
        // clone_file: a missing source leaves no destination.
        let to = tmp.join("out.img");
        assert!(clone_file(&tmp.join("missing.img"), &to).is_err());
        assert!(!to.exists());
        // apply: a golden that lists a disk it doesn't have fails with nothing half-written.
        let dir = tmp.join("g");
        std::fs::create_dir_all(&dir).expect("g");
        std::fs::write(dir.join("userdata.img"), b"data").expect("userdata");
        let golden = Golden {
            dir,
            key: GoldenKey { name: "g".into(), package: String::new(), image: String::new(), shape: String::new() },
            disks: vec![("userdata.img".into(), 4), ("cache.img".into(), 5)],
            built_unix: 0,
        };
        assert!(apply(&golden, &a).is_err());
        let left: Vec<String> = std::fs::read_dir(state_dir(&a.dir))
            .expect("simee")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "instance.lock")
            .collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn a_golden_only_starts_a_new_avd() {
        let (tmp, env) = sdk("apply", "2");
        avd(&env, "one");
        env.ensure_golden("one", |a| settle(a, "settled1"), &mut |_| {}).expect("one");
        // `one` has user data now: asking again is an error, not an overwrite.
        assert!(env.ensure_golden("one", |_| Ok(()), &mut |_| {}).is_err());
        assert_eq!(marker(&env.load("one").expect("one").dir), "settled1");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
