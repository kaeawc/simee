//! Parsed `.ini` files remembered by (path, mtime, ctime, size, inode), so resolving a device doesn't
//! read and parse every AVD's pointer and `config.ini` on every call (#256).
//!
//! A hit costs the one `stat` that validates it; an edit, a replacement or a different file at
//! that path changes the stamp and is read again. The change time is part of the stamp because
//! the modification time can be set back (`cp -p`, `touch -r`, `tar` restoring a file of the same
//! size) but the change time can't. A file changed within [`SETTLE`] of the read is not
//! remembered: a second edit in the same clock tick, of the same size, would keep the stamp
//! (git's "racily clean" rule), so a file is trusted once it is older than the timestamp's grain.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

use crate::{Error, Result, parse_ini};

/// How old a file's mtime and ctime must be before its parse is remembered (coarser than any
/// filesystem's timestamp grain: ext4 and HFS+ 1-2 s at worst).
const SETTLE: Duration = Duration::from_secs(2);

/// Remembered files before the table starts over (a home has tens of AVDs, two files each).
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime: SystemTime,
    /// Seconds and nanoseconds of the change time.
    ctime: (i64, i64),
    len: u64,
    ino: u64,
}

struct Entry {
    stamp: Stamp,
    map: Arc<BTreeMap<String, String>>,
}

fn table() -> MutexGuard<'static, HashMap<PathBuf, Entry>> {
    static TABLE: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    TABLE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

fn stamp(meta: &std::fs::Metadata) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    Some(Stamp { mtime: meta.modified().ok()?, ctime: (meta.ctime(), meta.ctime_nsec()), len: meta.len(), ino: meta.ino() })
}

impl Stamp {
    /// Whether neither time is within [`SETTLE`] of `now` (or after it).
    fn settled(&self, now: SystemTime) -> bool {
        let ctime = u64::try_from(self.ctime.0)
            .ok()
            .zip(u32::try_from(self.ctime.1).ok())
            .and_then(|(s, n)| SystemTime::UNIX_EPOCH.checked_add(Duration::new(s, n)));
        ctime.is_some_and(|c| c.max(self.mtime) + SETTLE <= now)
    }
}

/// The parsed `.ini` at `path`; `None` when it isn't a regular file (missing, or a directory).
/// What [`parse_ini`] returns for the file's current contents, from memory when its stamp is
/// unchanged.
pub(crate) fn ini(path: &Path) -> Result<Option<Arc<BTreeMap<String, String>>>> {
    ini_at(path, SystemTime::now)
}

/// [`ini`] with the clock it settles files by, so a test can say how old a file is.
fn ini_at(path: &Path, clock: impl FnOnce() -> SystemTime) -> Result<Option<Arc<BTreeMap<String, String>>>> {
    // `metadata` follows links, as `is_file` and `read` do.
    let Ok(meta) = std::fs::metadata(path) else { return Ok(None) };
    if !meta.is_file() {
        return Ok(None);
    }
    let now = stamp(&meta);
    if let Some(now) = now
        && let Some(e) = table().get(path)
        && e.stamp == now
    {
        return Ok(Some(e.map.clone()));
    }
    let read_at = clock();
    #[cfg(test)]
    reads::count(path);
    let text = read_text(path, meta.len()).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let map = Arc::new(parse_ini(&text));
    let mut table = table();
    match now {
        // Settled before the read began: the contents read are the contents the stamp names.
        Some(s) if s.settled(read_at) => {
            if table.len() >= MAX_ENTRIES {
                table.clear();
            }
            table.insert(path.to_path_buf(), Entry { stamp: s, map: map.clone() });
        }
        _ => {
            table.remove(path);
        }
    }
    Ok(Some(map))
}

/// The file's text, read with the size `stat` just gave (std's `read_to_string` asks the file
/// for its size again: two more system calls per file, which add up over a home of AVDs).
fn read_text(path: &Path, len: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0).saturating_add(1));
    // `take` hides the file from `read_to_end`, which would ask it for its size and position.
    std::fs::File::open(path)?.take(len.saturating_add(1)).read_to_end(&mut bytes)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The names `make` lists in the directory `dir`, from memory while the directory's stamp is
/// unchanged. Creating, deleting or renaming an entry changes its mtime and ctime, which is all a
/// listing of names depends on; a directory changed within [`SETTLE`] of the read isn't remembered.
/// `make` says with its names whether they can be remembered: a read that failed in any way, or a
/// listing of something the directory's stamp doesn't cover, is used once and forgotten, as an
/// unreadable file is.
pub(crate) fn listing(dir: &Path, make: impl FnOnce() -> (Vec<String>, bool)) -> Vec<String> {
    listing_at(dir, SystemTime::now, make)
}

fn listing_at(dir: &Path, clock: impl FnOnce() -> SystemTime, make: impl FnOnce() -> (Vec<String>, bool)) -> Vec<String> {
    let Some(now) = std::fs::metadata(dir).ok().filter(std::fs::Metadata::is_dir).as_ref().and_then(stamp) else { return make().0 };
    if let Some(known) = listings().get(dir).filter(|(s, _)| *s == now) {
        return known.1.to_vec();
    }
    let read_at = clock();
    #[cfg(test)]
    reads::count(dir);
    let (names, cacheable) = make();
    let mut table = listings();
    if cacheable && now.settled(read_at) {
        if table.len() >= MAX_ENTRIES {
            table.clear();
        }
        table.insert(dir.to_path_buf(), (now, names.as_slice().into()));
    } else {
        table.remove(dir);
    }
    names
}

type Listings = HashMap<PathBuf, (Stamp, Arc<[String]>)>;

fn listings() -> MutexGuard<'static, Listings> {
    static TABLE: OnceLock<Mutex<Listings>> = OnceLock::new();
    TABLE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// How often each file was read from disk, for tests.
#[cfg(test)]
mod reads {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    fn table() -> std::sync::MutexGuard<'static, HashMap<PathBuf, usize>> {
        static T: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
        T.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn count(path: &Path) {
        *table().entry(path.to_path_buf()).or_default() += 1;
    }

    pub fn of(path: &Path) -> usize {
        table().get(path).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Much later than anything a test writes: every file is old enough to remember.
    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(3600)
    }

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emu-avd-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    fn set_mtime(path: &Path, to: SystemTime) {
        std::fs::File::options().write(true).open(path).expect("open").set_modified(to).expect("mtime");
    }

    fn mtime(path: &Path) -> SystemTime {
        std::fs::metadata(path).expect("meta").modified().expect("mtime")
    }

    /// `path` as `ini_at` reads it with the clock at `now`: the value of key `a`.
    fn get_at(path: &Path, now: SystemTime) -> Option<String> {
        ini_at(path, || now).expect("read").and_then(|m| m.get("a").cloned())
    }

    fn get(path: &Path) -> Option<String> {
        get_at(path, later())
    }

    #[test]
    fn an_unchanged_file_is_read_once() {
        let d = dir("hit");
        let f = d.join("config.ini");
        std::fs::write(&f, "a=1\n").expect("write");
        for _ in 0..3 {
            assert_eq!(get(&f).as_deref(), Some("1"));
        }
        assert_eq!(reads::of(&f), 1, "the later calls were answered from memory");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_changed_mtime_size_inode_or_ctime_is_read_again() {
        let d = dir("miss");
        let f = d.join("config.ini");
        std::fs::write(&f, "a=1\n").expect("write");
        assert_eq!(get(&f).as_deref(), Some("1"));
        // A newer mtime.
        std::fs::write(&f, "a=2\n").expect("rewrite");
        set_mtime(&f, SystemTime::now() + Duration::from_secs(30));
        assert_eq!(get(&f).as_deref(), Some("2"), "mtime");
        // The same mtime, another size.
        let stamp = mtime(&f);
        std::fs::write(&f, "a=33\n").expect("rewrite");
        set_mtime(&f, stamp);
        assert_eq!(get(&f).as_deref(), Some("33"), "size");
        // Replaced by another file of that size and mtime (a rename over it): another inode.
        let other = d.join("other.ini");
        std::fs::write(&other, "a=44\n").expect("write");
        set_mtime(&other, stamp);
        std::fs::rename(&other, &f).expect("rename");
        assert_eq!(get(&f).as_deref(), Some("44"), "inode");
        // Gone, and a directory: not a file.
        std::fs::remove_file(&f).expect("remove");
        assert!(ini_at(&f, later).expect("gone").is_none());
        std::fs::create_dir(&f).expect("dir");
        assert!(ini_at(&f, later).expect("dir").is_none());
        std::fs::remove_dir_all(d).ok();
    }

    /// `cp -p`, `touch -r` and `tar x` put back a file of the same size with its old mtime, in
    /// place (same inode): only the change time says it was rewritten.
    #[test]
    fn a_same_size_file_restored_with_its_old_mtime_is_read_again() {
        let d = dir("ctime");
        let f = d.join("config.ini");
        std::fs::write(&f, "a=1\n").expect("write");
        let (before, ino) = (mtime(&f), std::fs::metadata(&f).expect("meta").ino());
        assert_eq!(get(&f).as_deref(), Some("1"));
        // The ctime has to move: a coarse clock may give the rewrite the first write's tick.
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&f, "a=2\n").expect("rewrite");
        set_mtime(&f, before);
        let meta = std::fs::metadata(&f).expect("meta");
        assert_eq!((meta.len(), meta.ino(), meta.modified().expect("mtime")), (4, ino, before), "everything but the ctime is as it was");
        assert_eq!(get(&f).as_deref(), Some("2"));
        assert_eq!(reads::of(&f), 2);
        std::fs::remove_dir_all(d).ok();
    }

    /// Which clock a file is remembered at is the test's, not the runner's: no stall can change it.
    #[test]
    fn a_file_changed_within_two_seconds_of_the_read_is_not_remembered() {
        let d = dir("racy");
        let f = d.join("config.ini");
        std::fs::write(&f, "a=1\n").expect("write");
        let meta = std::fs::metadata(&f).expect("meta");
        let changed = SystemTime::UNIX_EPOCH + Duration::new(meta.ctime() as u64, meta.ctime_nsec() as u32);
        let newest = changed.max(mtime(&f));
        // Read a second after the write: the next edit could share the stamp, so nothing is kept.
        for _ in 0..3 {
            assert_eq!(get_at(&f, newest + Duration::from_secs(1)).as_deref(), Some("1"));
        }
        assert_eq!(reads::of(&f), 3, "read every time");
        // Read when it is older than the grain: the first of these (the 4th read) is remembered, the
        // other two are answered from memory.
        for _ in 0..3 {
            assert_eq!(get_at(&f, newest + SETTLE).as_deref(), Some("1"));
        }
        assert_eq!(reads::of(&f), 4, "one more read, then remembered");
        // A same-size rewrite with its mtime set 10 s earlier than the rewrite's own changes the
        // mtime and the ctime: the 5th read sees it and, the ctime being within the grain of the
        // read (the real clock), doesn't remember it.
        std::fs::write(&f, "a=2\n").expect("rewrite");
        set_mtime(&f, mtime(&f) - Duration::from_secs(10));
        assert_eq!(get_at(&f, SystemTime::now()).as_deref(), Some("2"));
        // An mtime in the future (a skewed clock) never settles.
        set_mtime(&f, SystemTime::now() + Duration::from_secs(3600 * 24 * 365));
        for _ in 0..2 {
            assert_eq!(get_at(&f, later()).as_deref(), Some("2"));
        }
        assert_eq!(reads::of(&f), 7);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_listing_is_remembered_until_an_entry_comes_or_goes() {
        let d = dir("listing");
        let list = |now: SystemTime| {
            listing_at(
                &d,
                || now,
                || {
                    let mut names: Vec<String> = std::fs::read_dir(&d)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    (names, true)
                },
            )
        };
        std::fs::write(d.join("a.ini"), "").expect("a");
        for _ in 0..3 {
            assert_eq!(list(later()), ["a.ini"]);
        }
        assert_eq!(reads::of(&d), 1, "read once");
        // An entry coming, going or being renamed over moves the directory's mtime and ctime.
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(d.join("b.ini"), "").expect("b");
        assert_eq!(list(later()), ["a.ini", "b.ini"]);
        std::thread::sleep(Duration::from_millis(30));
        std::fs::remove_file(d.join("a.ini")).expect("remove");
        assert_eq!(list(later()), ["b.ini"]);
        std::thread::sleep(Duration::from_millis(30));
        std::fs::rename(d.join("b.ini"), d.join("c.ini")).expect("rename");
        assert_eq!(list(later()), ["c.ini"]);
        assert_eq!(reads::of(&d), 4);
        // Gone: nothing to remember it by.
        std::fs::remove_dir_all(&d).ok();
        assert_eq!(list(later()), Vec::<String>::new());
    }

    /// A read that failed (out of file handles, say) lists nothing this time: the directory's stamp
    /// didn't change, so remembering it would make it nothing until the directory does.
    #[test]
    fn a_listing_whose_read_failed_is_not_remembered() {
        let d = dir("listing-failed");
        std::fs::write(d.join("a.ini"), "").expect("a");
        let failed = std::cell::Cell::new(true);
        let list = || {
            listing_at(&d, later, || {
                // The injected failure: the same stamp, no names, `cacheable` false.
                if failed.get() { (Vec::new(), false) } else { (vec!["a".to_owned()], true) }
            })
        };
        for _ in 0..3 {
            assert!(list().is_empty());
        }
        assert_eq!(reads::of(&d), 3, "asked again each time");
        failed.set(false);
        assert_eq!(list(), ["a"], "the next read isn't answered with the failure");
        assert_eq!(list(), ["a"]);
        assert_eq!(reads::of(&d), 4, "then remembered");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Changed within two seconds of the read: not remembered, whatever the runner's speed.
    #[test]
    fn a_directory_changed_within_two_seconds_of_the_read_is_listed_every_time() {
        let d = dir("listing-racy");
        std::fs::write(d.join("a.ini"), "").expect("a");
        let meta = std::fs::metadata(&d).expect("meta");
        let changed = SystemTime::UNIX_EPOCH + Duration::new(meta.ctime() as u64, meta.ctime_nsec() as u32);
        let list = |now: SystemTime| listing_at(&d, || now, || (vec!["a".to_owned()], true));
        for _ in 0..3 {
            list(changed.max(mtime(&d)) + Duration::from_secs(1));
        }
        assert_eq!(reads::of(&d), 3, "read every time");
        for _ in 0..3 {
            list(changed.max(mtime(&d)) + SETTLE);
        }
        assert_eq!(reads::of(&d), 4, "one more read, then remembered");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_a_miss() {
        let d = dir("denied");
        let f = d.join("config.ini");
        std::fs::write(&f, "a=1\n").expect("write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        // Root reads it anyway; everyone else gets the error the plain read gave.
        if std::fs::read_to_string(&f).is_err() {
            assert!(matches!(ini_at(&f, later), Err(Error::Io(..))));
        }
        std::fs::remove_dir_all(d).ok();
    }
}
