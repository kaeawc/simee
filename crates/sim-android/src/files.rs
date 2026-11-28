//! App files on a running device, over adb: an app's containers and `ls`, `read`, `write`, `rm`,
//! `du` and clearing inside them, with iOS's parameters and results (sim-api's
//! `android_files.rs` answers the methods).
//!
//! - **Paths** are relative to the device's `/`, as iOS's are to its data directory (a leading
//!   `/` is allowed), and each must be inside one app's container: its data dir
//!   (`data/data/<package>`, `data/user/<n>/<package>`), its external dir
//!   (`sdcard/Android/data/<package>`, `storage/emulated/<n>/Android/data/<package>`) or its APK's
//!   dir (`data/app/…/<package>-…`, read only). `..` and paths outside every container are
//!   refused before anything runs ([`locate`]); on the device every command first resolves the
//!   path's deepest existing ancestor with `realpath` and refuses one a symlink takes out of the
//!   container.
//! - **Access** ([`Adb::access`]): the app's own uid through `run-as <package>` when the app is
//!   debuggable; otherwise root: adbd already root, `su 0` (userdebug and eng images; it leaves
//!   adbd, and the forwards through it, alone), or `adb root` (which restarts adbd). Neither: an
//!   error naming why each failed. An APK's dir is read as the shell, which may read it.
//! - **Reads** come back through `adb exec-out` (no pty, so binary-safe), checked against the
//!   size read first: exec-out keeps neither the exit status nor stderr apart.
//! - **Writes** go through `adb exec-in` into a temp file beside the target, are checked for
//!   size, then `mv`ed over it. As root, a data-dir file then gets the owner (and mode) the file
//!   had, or the container's owner when it is new, with `chown` (`chmod`) and its SELinux
//!   context with `restorecon`; through `run-as` the app's uid wrote it, so both are the app's
//!   already, and on external storage they are its FUSE daemon's.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::apps::{Adb, failure, io_within, package_name, shell_quote};

#[cfg(any(test, feature = "testing"))]
pub mod fake;

/// What a device-side guard prints (and exits with) when a path resolves outside its container.
const ESCAPE: &str = "simee: escapes the container";
const ESCAPE_EXIT: u8 = 3;
/// What it prints when the container itself isn't there.
const NO_CONTAINER: &str = "no such container";

/// Which of an app's containers a path is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Its data dir: `/data/user/<n>/<package>` (`/data/data/<package>` for user 0).
    Data,
    /// Its external dir: `/sdcard/Android/data/<package>`.
    External,
    /// Its APK's dir: `/data/app/…/<package>-…`. Read only.
    Bundle,
}

/// A path inside one app's container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub package: String,
    pub kind: Kind,
    /// The container, absolute (`/data/data/com.example.app`).
    pub root: String,
    /// The path, absolute, without `.`, empty components or a trailing `/`.
    pub path: String,
    /// The path as the caller gave it, for messages.
    pub given: String,
}

impl Location {
    /// Whether the path is the container itself.
    pub fn is_root(&self) -> bool {
        self.path == self.root
    }

    /// The path's parent directory.
    fn parent(&self) -> &str {
        self.path.rsplit_once('/').map_or("/", |(dir, _)| if dir.is_empty() { "/" } else { dir })
    }

    /// Refuses a change to an APK's dir.
    fn writable(&self) -> Result<(), String> {
        match self.kind {
            Kind::Bundle => Err(format!("{} is in {}'s APK directory, which is read only", self.given, self.package)),
            Kind::Data | Kind::External => Ok(()),
        }
    }
}

/// `path` as a place in one app's container, or why it isn't one. Pure: nothing runs.
pub fn locate(path: &str) -> Result<Location, String> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("path escapes the container: {path}")),
            p => parts.push(p),
        }
    }
    let outside = || {
        format!(
            "path is outside every app container: {path} (data/data/<package>, data/user/<n>/<package>, \
             sdcard/Android/data/<package>, or the APK directory `containers` lists)"
        )
    };
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (kind, len) = match parts.as_slice() {
        ["data", "data", _, ..] => (Kind::Data, 3),
        ["data", "user", n, _, ..] if number(n) => (Kind::Data, 4),
        ["sdcard", "Android", "data", _, ..] => (Kind::External, 4),
        ["storage", "emulated", n, "Android", "data", _, ..] if number(n) => (Kind::External, 6),
        // API 30+: /data/app/~~<random>/<package>-<random>; before: /data/app/<package>-<n>.
        ["data", "app", dir, _, ..] if dir.starts_with("~~") => (Kind::Bundle, 4),
        ["data", "app", _, ..] => (Kind::Bundle, 3),
        _ => return Err(outside()),
    };
    let dir = parts[len - 1];
    let package = if kind == Kind::Bundle { dir.rsplit_once('-').map_or(dir, |(p, _)| p) } else { dir };
    let package = package_name(package).map_err(|_| outside())?.to_owned();
    Ok(Location {
        package,
        kind,
        root: format!("/{}", parts[..len].join("/")),
        path: format!("/{}", parts.join("/")),
        given: path.to_owned(),
    })
}

/// How a container is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// As the app's own uid: `run-as <package>` (a debuggable app).
    RunAs(String),
    /// adbd runs as root.
    Root,
    /// `su 0` (userdebug and eng images).
    Su,
    /// As the shell's own uid (an APK's dir).
    Shell,
}

impl Access {
    /// `script` as one command line for the guest's shell, run with this access.
    fn wrap(&self, script: &str) -> String {
        let script = shell_quote(script);
        match self {
            Access::RunAs(package) => format!("run-as {package} sh -c {script}"),
            Access::Su => format!("su 0 sh -c {script}"),
            Access::Root | Access::Shell => format!("sh -c {script}"),
        }
    }

    fn is_root(&self) -> bool {
        matches!(self, Access::Root | Access::Su)
    }
}

/// One directory entry, as iOS's `ls` has it (`bytes` 0 for a directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub bytes: u64,
}

/// An app's containers, as iOS's `containers` has them: paths relative to the device's `/`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppContainers {
    pub package: String,
    /// Its data dir for user 0.
    pub data: Option<String>,
    /// Its APK's dir.
    pub bundle: Option<String>,
    pub data_bytes: u64,
    pub bundle_bytes: u64,
}

/// The start of every script: fails unless the container is there and `check`'s deepest existing
/// ancestor, symlinks resolved, is inside it (printing [`ESCAPE`] when it isn't).
fn guard(loc: &Location, check: &str) -> String {
    let (root, check) = (shell_quote(&loc.root), shell_quote(check));
    format!(
        "[ -d {root} ] || {{ echo '{NO_CONTAINER}' >&2; exit 1; }}; c=$(realpath {root}) || exit 1; p={check}; \
         while [ ! -e \"$p\" ] && [ ! -L \"$p\" ]; do p=\"${{p%/*}}\"; done; r=$(realpath \"$p\") || exit 1; \
         case \"$r/\" in \"$c\"/*) ;; *) echo '{ESCAPE}' >&2; exit {ESCAPE_EXIT};; esac"
    )
}

/// Fails unless `path` (quoted) is there, with the error text iOS's has.
fn exists(path: &str) -> String {
    format!("{{ [ -e {path} ] || [ -L {path} ]; }} || {{ echo 'No such file or directory' >&2; exit 1; }}")
}

/// The last line a command printed.
fn last_line(text: &str) -> &str {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no output")
}

/// The sum of the numbers, one per line, `stat -c %s` printed.
fn sum_sizes(out: &str) -> u64 {
    out.lines().filter_map(|l| l.trim().parse::<u64>().ok()).sum()
}

/// `find … -exec stat -c '%F|%s|%n'`'s lines as entries, directories first, then by name.
fn parse_entries(out: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = out
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '|');
            let (kind, size, path) = (fields.next()?, fields.next()?.trim().parse::<u64>().ok()?, fields.next()?);
            let name = path.rsplit('/').next().filter(|n| !n.is_empty())?;
            let dir = kind == "directory";
            Some(Entry { name: name.to_owned(), dir, bytes: if dir { 0 } else { size } })
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
    entries
}

/// A root probe's answer, kept for the rest of one call.
type RootProbe = Option<Result<Access, String>>;

impl Adb {
    /// How this device reaches `loc`'s container: see the module docs. Fails, naming why each
    /// way in failed, when there is none.
    pub fn access(&self, loc: &Location) -> Result<Access, String> {
        self.access_with(loc, &mut None)
    }

    /// [`Adb::access`] with the root probe's answer kept in `root` for the next call.
    fn access_with(&self, loc: &Location, root: &mut RootProbe) -> Result<Access, String> {
        if loc.kind == Kind::Bundle {
            return Ok(Access::Shell);
        }
        let package = &loc.package;
        let (ok, text) = self.run(&["shell", &format!("run-as {package} id -u")], self.deadlines.query)?;
        if ok && text.trim().parse::<u32>().is_ok() {
            return Ok(Access::RunAs(package.clone()));
        }
        let run_as = last_line(&text).to_owned();
        if run_as.contains("unknown package") {
            return Err(format!("{package} isn't installed (`apps` lists what is)"));
        }
        root.get_or_insert_with(|| self.root()).clone().map_err(|no_root| {
            format!(
                "can't reach {package}'s files: it isn't debuggable ({run_as}) and the device has no root ({no_root}); \
                 install a debuggable build of the app, or use a userdebug or eng system image (google_apis, \
                 not google_apis_playstore)"
            )
        })
    }

    /// Root on this device: adbd already root, then `su 0`, then `adb root`. Err says why not.
    fn root(&self) -> Result<Access, String> {
        let q = self.deadlines.query;
        let uid = |command: &str| self.run(&["shell", command], q).map(|(ok, text)| ok && text.trim() == "0");
        if uid("id -u")? {
            return Ok(Access::Root);
        }
        if uid("su 0 id -u")? {
            return Ok(Access::Su);
        }
        let (ok, text) = self.run(&["root"], q)?;
        if !ok || text.contains("cannot run as root") {
            return Err(last_line(&text).to_owned());
        }
        // adbd restarts: wait for it to come back as root.
        let deadline = Instant::now() + self.deadlines.start;
        loop {
            self.run(&["wait-for-device"], self.deadlines.start)?;
            if uid("id -u").unwrap_or(false) {
                return Ok(Access::Root);
            }
            if Instant::now() >= deadline {
                return Err(format!("adb root said `{}` but adbd isn't root", last_line(&text)));
            }
            sleep(Duration::from_millis(200));
        }
    }

    /// Runs `body` on the device with `access`: its output, or an error that names the path.
    fn script(&self, access: &Access, loc: &Location, body: &str) -> Result<String, String> {
        match self.run(&["shell", &access.wrap(body)], self.deadlines.install)? {
            (_, text) if text.contains(ESCAPE) => Err(format!("path escapes the container: {}", loc.given)),
            (true, text) => Ok(text),
            (false, text) => Err(format!("{}: {}", loc.given, last_line(&text))),
        }
    }

    /// `adb exec-out <command>`: its output, byte for byte.
    fn exec_out(&self, command: &str) -> Result<Vec<u8>, String> {
        let what = format!("adb -s {} exec-out", self.serial);
        let mut adb = Command::new(&self.adb);
        adb.args(["-s", &self.serial, "exec-out", command]);
        match io_within(&mut adb, None, self.deadlines.install, &what)? {
            (true, out, _) => Ok(out),
            (false, out, err) => {
                Err(failure("adb exec-out", &format!("{}{}", String::from_utf8_lossy(&err), String::from_utf8_lossy(&out))))
            }
        }
    }

    /// `adb exec-in <command>` with `bytes` as its input.
    fn exec_in(&self, command: &str, bytes: &[u8]) -> Result<(), String> {
        let what = format!("adb -s {} exec-in", self.serial);
        let mut adb = Command::new(&self.adb);
        adb.args(["-s", &self.serial, "exec-in", command]);
        match io_within(&mut adb, Some(bytes.to_vec()), self.deadlines.install, &what)? {
            (true, ..) => Ok(()),
            (false, out, err) => {
                Err(failure("adb exec-in", &format!("{}{}", String::from_utf8_lossy(&err), String::from_utf8_lossy(&out))))
            }
        }
    }

    /// Every third-party app's data dir (user 0's) and APK dir, sorted by package; with `sizes`,
    /// each one's bytes where it can be reached (0 where it can't: a release app without root).
    pub fn containers(&self, sizes: bool) -> Result<Vec<AppContainers>, String> {
        let out = self.shell("pm list packages -3 -f", self.deadlines.query)?;
        let mut apps: Vec<AppContainers> = out
            .lines()
            .filter_map(|line| {
                // package:/data/app/~~Ab==/com.example.app-Cd==/base.apk=com.example.app (the
                // dirs' base64 can hold `=`; a package name can't).
                let (apk, package) = line.trim().strip_prefix("package:")?.rsplit_once('=')?;
                let package = package_name(package).ok()?.to_owned();
                let bundle = apk.rsplit_once('/').map(|(dir, _)| dir.trim_start_matches('/').to_owned()).filter(|d| !d.is_empty());
                Some(AppContainers { data: Some(format!("data/user/0/{package}")), package, bundle, ..AppContainers::default() })
            })
            .collect();
        apps.sort_by(|a, b| a.package.cmp(&b.package));
        apps.dedup_by(|a, b| a.package == b.package);
        if sizes {
            let mut root = None;
            let mut size = |path: &Option<String>| -> Option<u64> {
                let loc = locate(path.as_deref()?).ok()?;
                let access = self.access_with(&loc, &mut root).ok()?;
                self.du_at(&access, &loc).ok()
            };
            for app in &mut apps {
                app.data_bytes = size(&app.data).unwrap_or(0);
                app.bundle_bytes = size(&app.bundle).unwrap_or(0);
            }
        }
        Ok(apps)
    }

    /// The entries of a directory.
    pub fn ls(&self, path: &str) -> Result<Vec<Entry>, String> {
        let loc = locate(path)?;
        let access = self.access(&loc)?;
        let p = shell_quote(&loc.path);
        let listed = shell_quote(&format!("{}/", loc.path));
        let body = format!(
            "{}; {}; [ -d {p} ] || {{ echo 'Not a directory' >&2; exit 1; }}; \
             find {listed} -mindepth 1 -maxdepth 1 -exec stat -c '%F|%s|%n' {{}} +",
            guard(&loc, &loc.path),
            exists(&p)
        );
        Ok(parse_entries(&self.script(&access, &loc, &body)?))
    }

    /// A file's bytes.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let loc = locate(path)?;
        let access = self.access(&loc)?;
        let p = shell_quote(&loc.path);
        let body = format!(
            "{}; {}; [ -f {p} ] || {{ echo 'Not a file' >&2; exit 1; }}; [ -r {p} ] || {{ echo 'Permission denied' >&2; exit 1; }}; wc -c < {p}",
            guard(&loc, &loc.path),
            exists(&p)
        );
        let size = self.script(&access, &loc, &body)?;
        let want: usize = last_line(&size).parse().map_err(|_| format!("{}: no size in `{}`", loc.given, size.trim()))?;
        let bytes = self.exec_out(&access.wrap(&format!("cat {p} 2>/dev/null")))?;
        if bytes.len() != want {
            return Err(format!("{}: read {} of its {want} bytes (did it change while it was read?)", loc.given, bytes.len()));
        }
        Ok(bytes)
    }

    /// Writes a file, creating its parent directories, through a temp file beside it (see the
    /// module docs for its owner and context).
    pub fn write_file(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let loc = locate(path)?;
        loc.writable()?;
        if loc.is_root() {
            return Err(format!("{} is the container itself, a directory", loc.given));
        }
        let access = self.access(&loc)?;
        let (p, dir, root) = (shell_quote(&loc.path), shell_quote(loc.parent()), shell_quote(&loc.root));
        let tmp = shell_quote(&format!("{}/.simee-write-{}-{}", loc.parent(), std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        // Checked, parents made, and whose the file is (or, new, would be).
        let prepare = format!(
            "{}; [ -d {p} ] && {{ echo 'Is a directory' >&2; exit 1; }}; mkdir -p {dir} || exit 1; \
             if [ -e {p} ]; then stat -c '%u:%g:%a' {p}; else stat -c '%u:%g:' {root}; fi",
            guard(&loc, &loc.path)
        );
        let owner = self.script(&access, &loc, &prepare)?;
        let restore = access.is_root() && loc.kind == Kind::Data;
        let (ids, mode) = owner_of(last_line(&owner));
        if restore && ids.is_none() {
            return Err(format!("{}: unexpected owner `{}`", loc.given, last_line(&owner)));
        }
        if let Err(e) = self.exec_in(&access.wrap(&format!("cat > {tmp}")), bytes) {
            let _ = self.script(&access, &loc, &format!("rm -f {tmp}"));
            return Err(e);
        }
        let len = bytes.len();
        let mut finish = format!(
            "{}; n=$(wc -c < {tmp}) || exit 1; [ $n -eq {len} ] || {{ rm -f {tmp}; echo \"wrote $n of {len} bytes\" >&2; exit 1; }}; \
             mv -f {tmp} {p}",
            guard(&loc, &loc.path)
        );
        if let (true, Some(ids)) = (restore, ids) {
            finish.push_str(&format!(" && chown {ids} {p}"));
            if let Some(mode) = mode {
                finish.push_str(&format!(" && chmod {mode} {p}"));
            }
            finish.push_str(&format!(" && restorecon {p}"));
        }
        self.script(&access, &loc, &finish).map(|_| ())
    }

    /// Removes a file, a symlink (not what it points at) or a directory tree; never the
    /// container itself.
    pub fn remove(&self, path: &str) -> Result<(), String> {
        let loc = locate(path)?;
        loc.writable()?;
        if loc.is_root() {
            return Err(format!("refusing to remove the whole container: {} (clear_container empties it)", loc.given));
        }
        let access = self.access(&loc)?;
        let p = shell_quote(&loc.path);
        self.script(&access, &loc, &format!("{}; {}; rm -rf {p}", guard(&loc, loc.parent()), exists(&p))).map(|_| ())
    }

    /// The bytes of a file, or of the files in a tree (symlinks not followed); 0 for nothing
    /// there, as iOS's.
    pub fn du(&self, path: &str) -> Result<u64, String> {
        let loc = locate(path)?;
        let access = self.access(&loc)?;
        self.du_at(&access, &loc)
    }

    fn du_at(&self, access: &Access, loc: &Location) -> Result<u64, String> {
        let p = shell_quote(&loc.path);
        let body = format!("{}; [ -e {p} ] || exit 0; find {p} -type f -exec stat -c %s {{}} +", guard(loc, &loc.path));
        self.script(access, loc, &body).map(|out| sum_sizes(&out))
    }

    /// Empties a directory in a container, keeping it (and, in a data dir itself, the system's
    /// `lib`). An app's data dir it can't reach (a release app, no root) is cleared with
    /// `pm clear`, which also clears its external dir and stops it.
    pub fn clear_container(&self, path: &str) -> Result<(), String> {
        let loc = locate(path)?;
        loc.writable()?;
        let whole = loc.kind == Kind::Data && loc.is_root();
        let access = match self.access(&loc) {
            Ok(access) => access,
            Err(why) if whole && why.starts_with("can't reach") => {
                let out = self.shell(&format!("pm clear {}", loc.package), self.deadlines.install)?;
                return if out.lines().any(|l| l.trim() == "Success") {
                    Ok(())
                } else {
                    Err(format!("{why}; pm clear: {}", last_line(&out)))
                };
            }
            Err(why) => return Err(why),
        };
        let (p, listed) = (shell_quote(&loc.path), shell_quote(&format!("{}/", loc.path)));
        let keep = if whole { "! -name lib " } else { "" };
        let body = format!(
            "{}; [ -d {p} ] || {{ echo 'Not a directory' >&2; exit 1; }}; find {listed} -mindepth 1 -maxdepth 1 {keep}-exec rm -rf {{}} +",
            guard(&loc, &loc.path)
        );
        self.script(&access, &loc, &body).map(|_| ())
    }
}

/// `stat -c '%u:%g:%a'`'s answer as `uid:gid` and the mode (none when it printed none).
fn owner_of(line: &str) -> (Option<String>, Option<String>) {
    let mut fields = line.split(':');
    let (uid, gid, mode) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""), fields.next().unwrap_or(""));
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if fields.next().is_some() || !number(uid) || !number(gid) {
        return (None, None);
    }
    (Some(format!("{uid}:{gid}")), number(mode).then(|| mode.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::fake::{FakeFiles, Root, fake_files_adb};
    use super::*;
    use crate::Android;
    use crate::testing::{FakeEmulator, fake_emulator};

    #[test]
    fn paths_are_located_in_one_apps_container() {
        let at = |p: &str| locate(p).map(|l| (l.package, l.kind, l.root, l.path));
        let app = "com.example.app".to_owned();
        assert_eq!(
            at("data/data/com.example.app/files/./a.txt"),
            Ok((app.clone(), Kind::Data, "/data/data/com.example.app".into(), "/data/data/com.example.app/files/a.txt".into()))
        );
        assert_eq!(
            at("/data/user/10/com.example.app//files/"),
            Ok((app.clone(), Kind::Data, "/data/user/10/com.example.app".into(), "/data/user/10/com.example.app/files".into()))
        );
        assert_eq!(
            at("sdcard/Android/data/com.example.app/files/x"),
            Ok((
                app.clone(),
                Kind::External,
                "/sdcard/Android/data/com.example.app".into(),
                "/sdcard/Android/data/com.example.app/files/x".into()
            ))
        );
        assert_eq!(
            at("/storage/emulated/0/Android/data/com.example.app").map(|l| (l.1, l.2)),
            Ok((Kind::External, "/storage/emulated/0/Android/data/com.example.app".into()))
        );
        assert_eq!(
            at("data/app/~~Ab==/com.example.app-Cd==/base.apk"),
            Ok((
                app.clone(),
                Kind::Bundle,
                "/data/app/~~Ab==/com.example.app-Cd==".into(),
                "/data/app/~~Ab==/com.example.app-Cd==/base.apk".into()
            ))
        );
        assert_eq!(at("data/app/com.example.app-1/base.apk").map(|l| (l.0, l.2)), Ok((app, "/data/app/com.example.app-1".into())));
        assert!(locate("data/data/com.example.app").expect("root").is_root());
        assert!(!locate("data/data/com.example.app/files").expect("files").is_root());
    }

    #[test]
    fn paths_that_leave_a_container_are_refused_before_anything_runs() {
        for escape in ["data/data/com.example.app/../com.other.app/files", "data/data/com.example.app/files/../../../../etc", "../etc"] {
            assert_eq!(locate(escape), Err(format!("path escapes the container: {escape}")), "{escape}");
        }
        for outside in
            ["/etc/passwd", "data/local/tmp/x", "", "/", "data/data", "data/user/x/com.example.app", "data/app/~~Ab==", "sdcard/DCIM/a.jpg"]
        {
            let err = locate(outside).expect_err(outside);
            assert!(err.starts_with(&format!("path is outside every app container: {outside} ")), "{outside}: {err}");
        }
        // The package is checked, so it is safe in a run-as line.
        assert!(locate("data/data/com.x;reboot/files").is_err());
        assert!(locate("data/data/$(id)/files").is_err());
    }

    struct Fixture {
        root: PathBuf,
        adb: Adb,
        fake: FakeFiles,
        _emulator: FakeEmulator,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A running AVD whose adb is the files fake, with `root`.
    fn fixture(tag: &str, root: Root) -> Fixture {
        let dir = PathBuf::from("/tmp").join(format!("sa-files-{tag}-{}", std::process::id()));
        let image = dir.join("sdk/system-images/android-34/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=34\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(dir.join("sdk")), avd_home: dir.join("avd") });
        android.running_dir = Some(dir.join("running"));
        android.create("phone", "system-images;android-34;google_apis;arm64-v8a", None).expect("create");
        let fake = fake_files_adb(&android, root);
        let _emulator = fake_emulator(&android, "phone");
        let adb = android.device("phone").expect("device").adb(&android).expect("adb");
        Fixture { root: dir, adb, fake, _emulator }
    }

    const NOTES: &str = "data/data/com.example.app/files/notes.txt";
    const RELEASE: &str = "data/data/com.example.release/files/r.txt";

    #[test]
    fn a_debuggable_app_is_reached_through_run_as() {
        let f = fixture("runas", Root::None);
        assert_eq!(f.adb.access(&locate(NOTES).expect("loc")), Ok(Access::RunAs("com.example.app".into())));
        assert_eq!(f.adb.read_file(NOTES).as_deref(), Ok(&b"hello"[..]));
        let calls = f.fake.calls();
        assert!(calls.iter().all(|c| c != "root" && !c.contains("su 0")), "run-as needs no root: {calls:?}");
        let read = calls.iter().find(|c| c.starts_with("exec-out ")).expect("an exec-out read");
        assert!(read.starts_with("exec-out run-as com.example.app sh -c "), "{read}");
    }

    #[test]
    fn a_release_app_is_reached_as_root() {
        // adbd already root: the commands run as they are.
        let f = fixture("adbd", Root::Adbd);
        assert_eq!(f.adb.access(&locate(RELEASE).expect("loc")), Ok(Access::Root));
        assert_eq!(f.adb.read_file(RELEASE).as_deref(), Ok(&b"release"[..]));
        let calls = f.fake.calls();
        assert!(calls.iter().any(|c| c.starts_with("exec-out sh -c ")), "{calls:?}");
        assert!(!calls.contains(&"root".to_owned()), "adbd was root already: {calls:?}");

        // su 0, without restarting adbd.
        let f = fixture("su", Root::Su);
        assert_eq!(f.adb.access(&locate(RELEASE).expect("loc")), Ok(Access::Su));
        assert_eq!(f.adb.read_file(RELEASE).as_deref(), Ok(&b"release"[..]));
        assert!(f.fake.calls().iter().any(|c| c.starts_with("exec-out su 0 sh -c ")), "{:?}", f.fake.calls());
        assert!(!f.fake.rooted());

        // adb root, when neither is there yet.
        let f = fixture("adbroot", Root::AdbRoot);
        assert_eq!(f.adb.read_file(RELEASE).as_deref(), Ok(&b"release"[..]));
        let calls = f.fake.calls();
        assert!(f.fake.rooted(), "adb root ran: {calls:?}");
        let root = calls.iter().position(|c| c == "root").expect("adb root");
        assert_eq!(calls[root + 1], "wait-for-device");
    }

    #[test]
    fn an_app_neither_debuggable_nor_rootable_says_why() {
        let f = fixture("neither", Root::None);
        let err = f.adb.read_file(RELEASE).expect_err("no way in");
        assert!(err.starts_with("can't reach com.example.release's files: "), "{err}");
        assert!(err.contains("run-as: package not debuggable: com.example.release"), "{err}");
        assert!(err.contains("adbd cannot run as root in production builds"), "{err}");
        assert!(err.contains("debuggable build") && err.contains("userdebug"), "says what would work: {err}");
        assert!(f.fake.calls().iter().all(|c| !c.starts_with("exec-out")), "nothing was read: {:?}", f.fake.calls());
        // An app that isn't installed is said to be so.
        let err = f.adb.ls("data/data/com.example.gone").expect_err("not installed");
        assert!(err.contains("com.example.gone isn't installed"), "{err}");
    }

    #[test]
    fn escapes_are_refused_lexically_and_through_symlinks() {
        let f = fixture("escape", Root::Adbd);
        for path in ["data/data/com.example.app/../com.example.release/files/r.txt", "/etc/hosts", "data/local/tmp/x"] {
            assert!(f.adb.read_file(path).is_err(), "{path}");
            assert!(f.adb.write_file(path, b"x").is_err(), "{path}");
            assert!(f.adb.remove(path).is_err(), "{path}");
        }
        assert!(f.fake.calls().is_empty(), "refused before adb: {:?}", f.fake.calls());
        // files/out links outside the container.
        let out = "data/data/com.example.app/files/out/secret.txt";
        assert_eq!(f.adb.read_file(out), Err(format!("path escapes the container: {out}")));
        assert_eq!(
            f.adb.ls("data/data/com.example.app/files/out"),
            Err("path escapes the container: data/data/com.example.app/files/out".into())
        );
        let new = "data/data/com.example.app/files/out/new/planted.txt";
        assert_eq!(f.adb.write_file(new, b"x"), Err(format!("path escapes the container: {new}")));
        assert!(!f.fake.outside.join("new").exists(), "nothing was made outside");
        assert_eq!(f.adb.remove(out), Err(format!("path escapes the container: {out}")));
        assert!(f.fake.outside.join("secret.txt").exists());
        assert_eq!(
            f.adb.du("data/data/com.example.app/files/out"),
            Err("path escapes the container: data/data/com.example.app/files/out".into())
        );
        // Removing the link itself stays inside: it goes, what it points at stays.
        f.adb.remove("data/data/com.example.app/files/out").expect("remove the link");
        assert!(f.fake.outside.join("secret.txt").exists());
        // The whole container is never removed.
        assert!(f.adb.remove("data/data/com.example.app").expect_err("root").starts_with("refusing to remove the whole container"));
    }

    /// Every byte value, CR LF pairs and a NUL run: what a pty or a text read would mangle.
    fn binary() -> Vec<u8> {
        let mut bytes: Vec<u8> = (0..=255).collect();
        bytes.extend(b"\r\n\n\r\0\0\0\x1a\xff");
        bytes
    }

    #[test]
    fn binary_files_round_trip_through_run_as_and_root() {
        for (tag, root, path) in [
            ("binrunas", Root::None, "data/data/com.example.app/files/blob.bin"),
            ("binroot", Root::Adbd, "data/user/0/com.example.release/files/new/dir/blob.bin"),
            ("binext", Root::None, "sdcard/Android/data/com.example.app/files/blob.bin"),
        ] {
            let f = fixture(tag, root);
            f.adb.write_file(path, &binary()).expect(tag);
            assert_eq!(std::fs::read(f.fake.on_device(path)).expect("on device"), binary(), "{tag}");
            assert_eq!(f.adb.read_file(path), Ok(binary()), "{tag}");
            let calls = f.fake.calls();
            assert!(calls.iter().any(|c| c.starts_with("exec-in ")), "{tag}: written through exec-in: {calls:?}");
            let leftovers: Vec<_> = std::fs::read_dir(f.fake.on_device(path).parent().expect("dir"))
                .expect("dir")
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".simee-"))
                .collect();
            assert!(leftovers.is_empty(), "{tag}: the temp file was moved: {leftovers:?}");
        }
    }

    #[test]
    fn a_root_write_keeps_the_files_owner_mode_and_context() {
        let f = fixture("owner", Root::Adbd);
        f.fake.set_owner("10123:10123:660");
        f.adb.write_file(RELEASE, b"changed").expect("write");
        assert_eq!(std::fs::read(f.fake.on_device(RELEASE)).expect("file"), b"changed");
        let last = f.fake.calls().pop().expect("calls");
        let p = "'\\''/data/data/com.example.release/files/r.txt'\\''";
        let mv = last.find("mv -f ").expect("mv");
        let chown = last.find(&format!("chown 10123:10123 {p}")).expect("chown to the file's owner");
        let chmod = last.find(&format!("chmod 660 {p}")).expect("chmod to its mode");
        let restorecon = last.find(&format!("restorecon {p}")).expect("restorecon");
        assert!(mv < chown && chown < chmod && chmod < restorecon, "in order, after the move: {last}");

        // A new file gets the container's owner, and its default mode.
        f.adb.write_file("data/data/com.example.release/files/fresh.txt", b"new").expect("new");
        let last = f.fake.calls().pop().expect("calls");
        assert!(last.contains("chown 10123:10123 ") && last.contains("restorecon ") && !last.contains("chmod"), "{last}");

        // Through run-as the app's own uid writes it: owner and context are the app's already.
        let f = fixture("ownerrunas", Root::Adbd);
        f.adb.write_file(NOTES, b"mine").expect("run-as write");
        let last = f.fake.calls().pop().expect("calls");
        assert!(last.starts_with("shell run-as com.example.app ") && !last.contains("chown") && !last.contains("restorecon"), "{last}");

        // External storage's owner and context are its FUSE daemon's to say, even as root.
        let f = fixture("ownerext", Root::Adbd);
        f.adb.write_file("sdcard/Android/data/com.example.app/x.txt", b"x").expect("external write");
        let last = f.fake.calls().pop().expect("calls");
        assert!(last.contains("mv -f ") && !last.contains("chown") && !last.contains("restorecon"), "{last}");
        // A container that isn't there (the release app has no external dir) is said to be missing.
        let err = f.adb.write_file("sdcard/Android/data/com.example.release/x.txt", b"x").expect_err("no external dir");
        assert!(err.contains("no such container"), "{err}");
    }

    #[test]
    fn writes_refuse_directories_and_the_apk() {
        let f = fixture("wrefuse", Root::Adbd);
        let err = f.adb.write_file("data/data/com.example.app/files", b"x").expect_err("a directory");
        assert!(err.contains("Is a directory"), "{err}");
        let apk = "data/app/~~Ab==/com.example.app-Cd==/base.apk";
        for err in [f.adb.write_file(apk, b"x"), f.adb.remove(apk), f.adb.clear_container("data/app/~~Ab==/com.example.app-Cd==")] {
            assert!(err.expect_err("read only").contains("read only"));
        }
        // But it reads, as the shell.
        assert_eq!(f.adb.read_file(apk).as_deref(), Ok(&b"PK\x03\x04app"[..]));
        assert_eq!(f.adb.du(apk), Ok(7));
    }

    #[test]
    fn ls_lists_directories_first_with_sizes() {
        let f = fixture("ls", Root::None);
        assert_eq!(
            f.adb.ls("data/data/com.example.app"),
            Ok(vec![
                Entry { name: "cache".into(), dir: true, bytes: 0 },
                Entry { name: "files".into(), dir: true, bytes: 0 },
                Entry { name: "lib".into(), dir: true, bytes: 0 },
                Entry { name: "shared_prefs".into(), dir: true, bytes: 0 },
            ])
        );
        let files = f.adb.ls("/data/user/0/com.example.app/files/").expect("files");
        assert_eq!(files.iter().map(|e| (e.name.as_str(), e.dir)).collect::<Vec<_>>(), [("notes.txt", false), ("out", false)]);
        assert_eq!(files[0].bytes, 5);
        assert!(f.adb.ls(NOTES).expect_err("a file").contains("Not a directory"));
        assert!(f.adb.ls("data/data/com.example.app/nope").expect_err("missing").contains("No such file or directory"));
    }

    #[test]
    fn rm_removes_a_file_or_a_tree() {
        let f = fixture("rm", Root::None);
        f.adb.remove(NOTES).expect("file");
        assert!(!f.fake.on_device(NOTES).exists());
        f.adb.remove("data/data/com.example.app/shared_prefs").expect("tree");
        assert!(!f.fake.on_device("data/data/com.example.app/shared_prefs").exists());
        assert!(f.adb.remove(NOTES).expect_err("gone").contains("No such file or directory"));
    }

    #[test]
    fn du_sums_the_files_in_a_tree() {
        let f = fixture("du", Root::None);
        // notes.txt (5), a.xml (7), c.bin (3), lib/.keep (0); files/out's target isn't counted.
        assert_eq!(f.adb.du("data/data/com.example.app"), Ok(15));
        assert_eq!(f.adb.du(NOTES), Ok(5));
        assert_eq!(f.adb.du("data/data/com.example.app/missing"), Ok(0), "as iOS's: nothing there is 0 bytes");
        let f = fixture("duroot", Root::Su);
        assert_eq!(f.adb.du("data/data/com.example.release"), Ok(7));
    }

    #[test]
    fn clear_container_empties_it_and_keeps_it() {
        let f = fixture("clear", Root::None);
        f.adb.clear_container("data/data/com.example.app").expect("clear");
        let left: Vec<String> = std::fs::read_dir(f.fake.on_device("data/data/com.example.app"))
            .expect("still there")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["lib"], "the system's lib dir stays");
        assert!(f.fake.outside.join("secret.txt").exists(), "a symlink's target isn't followed");
        assert!(f.fake.calls().iter().all(|c| !c.contains("pm clear")), "{:?}", f.fake.calls());
        // A directory inside it.
        f.adb.write_file("sdcard/Android/data/com.example.app/files/more.txt", b"m").expect("write");
        f.adb.clear_container("sdcard/Android/data/com.example.app/files").expect("external");
        assert_eq!(std::fs::read_dir(f.fake.on_device("sdcard/Android/data/com.example.app/files")).expect("dir").count(), 0);

        // No run-as and no root: the data container is cleared by `pm clear`.
        f.adb.clear_container("data/user/0/com.example.release").expect("pm clear");
        assert!(f.fake.calls().contains(&"shell pm clear com.example.release".to_owned()), "{:?}", f.fake.calls());
        assert_eq!(std::fs::read_dir(f.fake.on_device("data/user/0/com.example.release")).expect("dir").count(), 0);
        // But not a directory inside it.
        let err = f.adb.clear_container("data/user/0/com.example.release/files").expect_err("no access");
        assert!(err.starts_with("can't reach com.example.release's files"), "{err}");
    }

    #[test]
    fn containers_lists_each_apps_data_and_apk_dirs() {
        let f = fixture("containers", Root::None);
        let app = AppContainers {
            package: "com.example.app".into(),
            data: Some("data/user/0/com.example.app".into()),
            bundle: Some("data/app/~~Ab==/com.example.app-Cd==".into()),
            ..AppContainers::default()
        };
        let release = AppContainers {
            package: "com.example.release".into(),
            data: Some("data/user/0/com.example.release".into()),
            bundle: Some("data/app/~~Ef==/com.example.release-Gh==".into()),
            ..AppContainers::default()
        };
        assert_eq!(f.adb.containers(false), Ok(vec![app.clone(), release.clone()]));
        assert_eq!(f.fake.calls(), ["shell pm list packages -3 -f"]);
        // Sizes: what each can be reached for (the release app's data can't be, here).
        let sized = f.adb.containers(true).expect("sizes");
        assert_eq!((sized[0].data_bytes, sized[0].bundle_bytes), (15, 7));
        assert_eq!((sized[1].data_bytes, sized[1].bundle_bytes), (0, 11));
        let roots = f.fake.calls().iter().filter(|c| *c == "root").count();
        assert_eq!(roots, 1, "root is asked for once per call, not once per app: {:?}", f.fake.calls());
    }
}
