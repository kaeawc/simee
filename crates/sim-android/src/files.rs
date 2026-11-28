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
//!   debuggable; otherwise root: adbd already root, or `su 0` (userdebug and eng images; it
//!   leaves adbd, and the forwards through it, alone). `adb root` runs only with
//!   [`Allow::adb_root`] (the API's `root: true`), for reads and writes alike: it restarts adbd,
//!   which drops the device's adb forwards and open shells. None: an error naming why each failed
//!   and the opt-in. An APK's dir is read as the shell, which may read it.
//! - **Reads** come back through `adb exec-out` (no pty, so binary-safe), checked against the
//!   size read first (at most [`MAX_READ`]): exec-out keeps neither the exit status nor stderr
//!   apart.
//! - **Writes** make the parent dirs one at a time (as root, each new one gets the container's
//!   owner with `chown -h` and its context with `restorecon`), then go through `adb exec-in` into
//!   a temp file with a random name beside the target, made with `O_EXCL` (`set -C`) under the
//!   file's mode (`umask`). The finish step checks that the temp file is still the regular file
//!   it made and of the right size; as root, a data-dir file gets the owner the file had (or the
//!   container's, new) and its context, on the temp file and never through a symlink; then one
//!   `mv` within the dir renames it over the target. Any failure removes the temp file and leaves
//!   the target as it was. Through `run-as` the app's uid wrote it, so owner and context are the
//!   app's already, and on external storage they are its FUSE daemon's. Every step works from the
//!   target's dir, entered with `cd -P` and checked to be in the container, so a parent swapped
//!   for a symlink afterwards can't move it.
//! - **Clearing** a container empties it in place; `pm clear --user <n>` runs instead only with
//!   [`Allow::pm_clear`], as it also stops the app, wipes its external dir and resets its
//!   runtime permissions.

use std::io::Read;
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
/// The largest file `read` returns: the caller holds it, its base64 and its text at once.
pub const MAX_READ: usize = 128 << 20;

/// What a file call may do beyond reaching the files. The default: nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allow {
    /// Run `adb root` when the app isn't debuggable, adbd isn't root and `su 0` isn't there. It
    /// restarts adbd, which drops this device's adb forwards and open shells (the API's
    /// `root: true`).
    pub adb_root: bool,
    /// Clear an app's whole data dir with `pm clear --user <n>`, which also stops the app, wipes
    /// its external dir and resets its runtime permissions (the API's `pm_clear: true`).
    pub pm_clear: bool,
}

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

    /// The path's last component.
    fn name(&self) -> &str {
        self.path.rsplit_once('/').map_or(&self.path, |(_, name)| name)
    }

    /// The components between the container and the path's parent, outermost first.
    fn dirs(&self) -> Vec<&str> {
        let below = self.parent().strip_prefix(&self.root).unwrap_or("");
        below.split('/').filter(|d| !d.is_empty()).collect()
    }

    /// The user whose container it is (`data/data` is user 0's), as `pm --user` takes it.
    fn user(&self) -> &str {
        self.root.strip_prefix("/data/user/").and_then(|r| r.split('/').next()).unwrap_or("0")
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

/// After [`guard`]: makes `dir` (quoted, or relative to where the script is) the script's
/// working directory, symlinks resolved, and fails unless it is in the container. What runs
/// after it names paths relative to it, so a parent swapped for a symlink later can't move them.
fn enter(dir: &str) -> String {
    format!("cd -P {dir} || exit 1; case \"$(pwd -P)/\" in \"$c\"/*) ;; *) echo '{ESCAPE}' >&2; exit {ESCAPE_EXIT};; esac")
}

/// 128 random bits as hex, for a temp name nothing on the device can guess.
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        use std::hash::{BuildHasher, Hasher};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        for half in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(nanos);
            h.write_u64(NEXT.fetch_add(1, Ordering::Relaxed));
            half.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    /// way in failed (and what `allow` would add), when there is none.
    pub fn access(&self, loc: &Location, allow: Allow) -> Result<Access, String> {
        self.access_with(loc, &mut None, allow)
    }

    /// [`Adb::access`] with the root probe's answer kept in `root` for the next call.
    fn access_with(&self, loc: &Location, root: &mut RootProbe, allow: Allow) -> Result<Access, String> {
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
        root.get_or_insert_with(|| self.root(allow.adb_root)).clone().map_err(|no_root| {
            format!(
                "can't reach {package}'s files: it isn't debuggable ({run_as}) and {no_root}; install a debuggable \
                 build of the app, or use a userdebug or eng system image (google_apis, not google_apis_playstore)"
            )
        })
    }

    /// Root on this device: adbd already root, then `su 0`, then (with `adb_root`) `adb root`.
    /// Err says why not, as the end of a sentence.
    fn root(&self, adb_root: bool) -> Result<Access, String> {
        let q = self.deadlines.query;
        let uid = |command: &str| self.run(&["shell", command], q).map(|(ok, text)| ok && text.trim() == "0");
        if uid("id -u")? {
            return Ok(Access::Root);
        }
        if uid("su 0 id -u")? {
            return Ok(Access::Su);
        }
        if !adb_root {
            return Err("adbd isn't root and `su 0` isn't there; `root: true` lets simee run `adb root`, which restarts adbd \
                        (dropping this device's adb forwards and open shells) and works on userdebug and eng images only"
                .into());
        }
        let (ok, text) = self.run(&["root"], q)?;
        if !ok || text.contains("cannot run as root") {
            return Err(format!("the device has no root (adb root: {})", last_line(&text)));
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
    pub fn containers(&self, sizes: bool, allow: Allow) -> Result<Vec<AppContainers>, String> {
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
                let access = self.access_with(&loc, &mut root, allow).ok()?;
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
    pub fn ls(&self, path: &str, allow: Allow) -> Result<Vec<Entry>, String> {
        let loc = locate(path)?;
        let access = self.access(&loc, allow)?;
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

    /// A file's bytes, at most [`MAX_READ`] of them.
    pub fn read_file(&self, path: &str, allow: Allow) -> Result<Vec<u8>, String> {
        let loc = locate(path)?;
        let access = self.access(&loc, allow)?;
        let p = shell_quote(&loc.path);
        let body = format!(
            "{}; {}; [ -f {p} ] || {{ echo 'Not a file' >&2; exit 1; }}; [ -r {p} ] || {{ echo 'Permission denied' >&2; exit 1; }}; wc -c < {p}",
            guard(&loc, &loc.path),
            exists(&p)
        );
        let size = self.script(&access, &loc, &body)?;
        let want: usize = last_line(&size).parse().map_err(|_| format!("{}: no size in `{}`", loc.given, size.trim()))?;
        if want > MAX_READ {
            return Err(format!("{}: {want} bytes, more than `read` returns ({} MiB)", loc.given, MAX_READ >> 20));
        }
        // The guard again, in the same script as the read: what was checked is what is read.
        let read = self.exec_out(&access.wrap(&format!("{}; cat {p} 2>/dev/null", guard(&loc, &loc.path))));
        let escaped = |text: &[u8]| String::from_utf8_lossy(text).contains(ESCAPE);
        match read {
            Err(e) if e.contains(ESCAPE) => Err(format!("path escapes the container: {}", loc.given)),
            Ok(bytes) if bytes.len() != want && escaped(&bytes) => Err(format!("path escapes the container: {}", loc.given)),
            Ok(bytes) if bytes.len() != want => {
                Err(format!("{}: read {} of its {want} bytes (did it change while it was read?)", loc.given, bytes.len()))
            }
            read => read,
        }
    }

    /// Writes a file, creating its parent directories, through a temp file beside it (see the
    /// module docs for its owner, mode and context, and why each step is safe as root in a
    /// directory the app owns).
    pub fn write_file(&self, path: &str, bytes: &[u8], allow: Allow) -> Result<(), String> {
        let loc = locate(path)?;
        loc.writable()?;
        if loc.is_root() {
            return Err(format!("{} is the container itself, a directory", loc.given));
        }
        let access = self.access(&loc, allow)?;
        let restore = access.is_root() && loc.kind == Kind::Data;
        let (p, root, name) = (shell_quote(&loc.path), shell_quote(&loc.root), shell_quote(&format!("./{}", loc.name())));
        // Checked; the parents made one at a time, each new one the container's owner's (as root);
        // then whose the file is (or, new, would be).
        let mut prepare = format!(
            "{}; [ -d {p} ] && {{ echo 'Is a directory' >&2; exit 1; }}; umask 077; {}; o=$(stat -c '%u:%g' .) || exit 1",
            guard(&loc, &loc.path),
            enter(&root)
        );
        for dir in loc.dirs() {
            let d = shell_quote(&format!("./{dir}"));
            let own =
                if restore { format!(" && chown -h \"$o\" {d} && restorecon \"$(pwd -P)\"/{}", shell_quote(dir)) } else { String::new() };
            prepare.push_str(&format!("; if [ ! -d {d} ]; then mkdir {d}{own} || exit 1; fi; {}", enter(&d)));
        }
        prepare.push_str(&format!("; if [ -e {name} ] || [ -L {name} ]; then stat -c '%u:%g:%a' {name}; else echo \"$o:\"; fi"));
        let owner = self.script(&access, &loc, &prepare)?;
        let (ids, mode) = owner_of(last_line(&owner));
        if restore && ids.is_none() {
            return Err(format!("{}: unexpected owner `{}`", loc.given, last_line(&owner)));
        }
        // The file's mode, from the moment it is made; a new file is the app's alone, as an
        // app's own files are.
        let mode = mode.as_deref().and_then(|m| u32::from_str_radix(m, 8).ok()).filter(|m| *m <= 0o7777).unwrap_or(0o600);
        let dir = shell_quote(loc.parent());
        // Hex only, so it needs no quoting, even inside the trap's.
        let tmp_name = format!(".simee-write-{}", random_hex());
        let tmp = format!("./{tmp_name}");
        let here = format!("{}; {}", guard(&loc, loc.parent()), enter(&dir));
        // Made with O_EXCL (`set -C`): never through anything already there, a symlink included.
        let made = self.exec_in(&access.wrap(&format!("{here}; set -C; umask {:03o}; cat > {tmp}", !mode & 0o777)), bytes);
        if let Err(e) = made {
            let _ = self.script(&access, &loc, &format!("{here}; rm -f {tmp}"));
            return Err(e);
        }
        let len = bytes.len();
        // From here every failure removes the temp file, and the target changes only by the
        // last step's rename.
        let mut finish = format!(
            "{here}; trap 'rm -f {tmp}' EXIT; {{ [ -f {tmp} ] && [ ! -L {tmp} ] && [ -O {tmp} ]; }} || \
             {{ echo 'the temp file simee made is gone or was replaced' >&2; exit 1; }}; \
             n=$(wc -c < {tmp}) || exit 1; [ \"$n\" -eq {len} ] || {{ echo \"wrote $n of {len} bytes\" >&2; exit 1; }}"
        );
        // umask can't give the execute and special bits.
        if mode & !0o666 != 0 {
            finish.push_str(&format!("; chmod {mode:o} {tmp} || exit 1"));
        }
        if let (true, Some(ids)) = (restore, ids) {
            finish.push_str(&format!("; restorecon \"$(pwd -P)\"/{tmp_name} && chown -h {ids} {tmp} || exit 1"));
        }
        finish.push_str(&format!("; mv -f {tmp} {name} || exit 1; trap - EXIT"));
        self.script(&access, &loc, &finish).map(|_| ())
    }

    /// Removes a file, a symlink (not what it points at) or a directory tree; never the
    /// container itself.
    pub fn remove(&self, path: &str, allow: Allow) -> Result<(), String> {
        let loc = locate(path)?;
        loc.writable()?;
        if loc.is_root() {
            return Err(format!("refusing to remove the whole container: {} (clear_container empties it)", loc.given));
        }
        let access = self.access(&loc, allow)?;
        let (dir, name) = (shell_quote(loc.parent()), shell_quote(&format!("./{}", loc.name())));
        let body = format!("{}; {}; {}; rm -rf {name}", guard(&loc, loc.parent()), enter(&dir), exists(&name));
        self.script(&access, &loc, &body).map(|_| ())
    }

    /// The bytes of a file, or of the files in a tree (symlinks not followed); 0 for nothing
    /// there, as iOS's.
    pub fn du(&self, path: &str, allow: Allow) -> Result<u64, String> {
        let loc = locate(path)?;
        let access = self.access(&loc, allow)?;
        self.du_at(&access, &loc)
    }

    fn du_at(&self, access: &Access, loc: &Location) -> Result<u64, String> {
        let p = shell_quote(&loc.path);
        let body = format!("{}; [ -e {p} ] || exit 0; find {p} -type f -exec stat -c %s {{}} +", guard(loc, &loc.path));
        self.script(access, loc, &body).map(|out| sum_sizes(&out))
    }

    /// Empties a directory in a container, keeping it (and, in a data dir itself, the system's
    /// `lib`). With [`Allow::pm_clear`], an app's whole data dir is cleared with
    /// `pm clear --user <n>` instead, which also stops the app, wipes its external dir and
    /// resets its runtime permissions; it reaches a release app with no root.
    pub fn clear_container(&self, path: &str, allow: Allow) -> Result<(), String> {
        let loc = locate(path)?;
        loc.writable()?;
        let whole = loc.kind == Kind::Data && loc.is_root();
        let pm_clear = format!("pm clear --user {} {}", loc.user(), loc.package);
        if allow.pm_clear {
            if !whole {
                return Err(format!(
                    "{}: `pm_clear` clears an app's whole data dir; give the container (data/user/<n>/<package>)",
                    loc.given
                ));
            }
            let out = self.shell(&pm_clear, self.deadlines.install)?;
            return if out.lines().any(|l| l.trim() == "Success") { Ok(()) } else { Err(format!("{pm_clear}: {}", last_line(&out))) };
        }
        let access = self.access(&loc, allow).map_err(|why| {
            if whole && why.starts_with("can't reach") {
                format!(
                    "{why}; or `pm_clear: true` clears it with `{pm_clear}`, which also stops the app, wipes its external \
                     dir and resets its runtime permissions"
                )
            } else {
                why
            }
        })?;
        let keep = if whole { "! -name lib " } else { "" };
        let body = format!(
            "{}; [ -d {p} ] || {{ echo 'Not a directory' >&2; exit 1; }}; {}; find . -mindepth 1 -maxdepth 1 {keep}-exec rm -rf {{}} +",
            guard(&loc, &loc.path),
            enter(&shell_quote(&loc.path)),
            p = shell_quote(&loc.path)
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
    use std::os::unix::fs::PermissionsExt;
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
    /// No opt-ins: what the API does unless a call asks for more.
    const NONE: Allow = Allow { adb_root: false, pm_clear: false };
    const ADB_ROOT: Allow = Allow { adb_root: true, pm_clear: false };
    const PM_CLEAR: Allow = Allow { adb_root: false, pm_clear: true };

    #[test]
    fn a_debuggable_app_is_reached_through_run_as() {
        let f = fixture("runas", Root::None);
        assert_eq!(f.adb.access(&locate(NOTES).expect("loc"), NONE), Ok(Access::RunAs("com.example.app".into())));
        assert_eq!(f.adb.read_file(NOTES, NONE).as_deref(), Ok(&b"hello"[..]));
        let calls = f.fake.calls();
        assert!(calls.iter().all(|c| c != "root" && !c.contains("su 0")), "run-as needs no root: {calls:?}");
        let read = calls.iter().find(|c| c.starts_with("exec-out ")).expect("an exec-out read");
        assert!(read.starts_with("exec-out run-as com.example.app sh -c "), "{read}");
    }

    #[test]
    fn a_release_app_is_reached_as_root() {
        // adbd already root: the commands run as they are.
        let f = fixture("adbd", Root::Adbd);
        assert_eq!(f.adb.access(&locate(RELEASE).expect("loc"), NONE), Ok(Access::Root));
        assert_eq!(f.adb.read_file(RELEASE, NONE).as_deref(), Ok(&b"release"[..]));
        let calls = f.fake.calls();
        assert!(calls.iter().any(|c| c.starts_with("exec-out sh -c ")), "{calls:?}");
        assert!(!calls.contains(&"root".to_owned()), "adbd was root already: {calls:?}");

        // su 0, without restarting adbd.
        let f = fixture("su", Root::Su);
        assert_eq!(f.adb.access(&locate(RELEASE).expect("loc"), NONE), Ok(Access::Su));
        assert_eq!(f.adb.read_file(RELEASE, NONE).as_deref(), Ok(&b"release"[..]));
        assert!(f.fake.calls().iter().any(|c| c.starts_with("exec-out su 0 sh -c ")), "{:?}", f.fake.calls());
        assert!(!f.fake.rooted());

        // adb root, when neither is there yet and the call allows it.
        let f = fixture("adbroot", Root::AdbRoot);
        assert_eq!(f.adb.read_file(RELEASE, ADB_ROOT).as_deref(), Ok(&b"release"[..]));
        let calls = f.fake.calls();
        assert!(f.fake.rooted(), "adb root ran: {calls:?}");
        let root = calls.iter().position(|c| c == "root").expect("adb root");
        assert_eq!(calls[root + 1], "wait-for-device");
    }

    #[test]
    fn an_app_neither_debuggable_nor_rootable_says_why() {
        let f = fixture("neither", Root::None);
        let err = f.adb.read_file(RELEASE, ADB_ROOT).expect_err("no way in");
        assert!(err.starts_with("can't reach com.example.release's files: "), "{err}");
        assert!(err.contains("run-as: package not debuggable: com.example.release"), "{err}");
        assert!(err.contains("adbd cannot run as root in production builds"), "{err}");
        // Without the opt-in, adb root isn't tried, and the error names the opt-in.
        let err = f.adb.read_file(RELEASE, NONE).expect_err("no way in");
        assert!(err.contains("`su 0` isn't there; `root: true` lets simee run `adb root`"), "{err}");
        assert!(err.contains("debuggable build") && err.contains("userdebug"), "says what would work: {err}");
        assert!(f.fake.calls().iter().all(|c| !c.starts_with("exec-out")), "nothing was read: {:?}", f.fake.calls());
        // An app that isn't installed is said to be so.
        let err = f.adb.ls("data/data/com.example.gone", NONE).expect_err("not installed");
        assert!(err.contains("com.example.gone isn't installed"), "{err}");
    }

    #[test]
    fn escapes_are_refused_lexically_and_through_symlinks() {
        let f = fixture("escape", Root::Adbd);
        for path in ["data/data/com.example.app/../com.example.release/files/r.txt", "/etc/hosts", "data/local/tmp/x"] {
            assert!(f.adb.read_file(path, NONE).is_err(), "{path}");
            assert!(f.adb.write_file(path, b"x", NONE).is_err(), "{path}");
            assert!(f.adb.remove(path, NONE).is_err(), "{path}");
        }
        assert!(f.fake.calls().is_empty(), "refused before adb: {:?}", f.fake.calls());
        // files/out links outside the container.
        let out = "data/data/com.example.app/files/out/secret.txt";
        assert_eq!(f.adb.read_file(out, NONE), Err(format!("path escapes the container: {out}")));
        assert_eq!(
            f.adb.ls("data/data/com.example.app/files/out", NONE),
            Err("path escapes the container: data/data/com.example.app/files/out".into())
        );
        let new = "data/data/com.example.app/files/out/new/planted.txt";
        assert_eq!(f.adb.write_file(new, b"x", NONE), Err(format!("path escapes the container: {new}")));
        assert!(!f.fake.outside.join("new").exists(), "nothing was made outside");
        assert_eq!(f.adb.remove(out, NONE), Err(format!("path escapes the container: {out}")));
        assert!(f.fake.outside.join("secret.txt").exists());
        assert_eq!(
            f.adb.du("data/data/com.example.app/files/out", NONE),
            Err("path escapes the container: data/data/com.example.app/files/out".into())
        );
        // Removing the link itself stays inside: it goes, what it points at stays.
        f.adb.remove("data/data/com.example.app/files/out", NONE).expect("remove the link");
        assert!(f.fake.outside.join("secret.txt").exists());
        // The whole container is never removed.
        assert!(f.adb.remove("data/data/com.example.app", NONE).expect_err("root").starts_with("refusing to remove the whole container"));
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
            f.adb.write_file(path, &binary(), NONE).expect(tag);
            assert_eq!(std::fs::read(f.fake.on_device(path)).expect("on device"), binary(), "{tag}");
            assert_eq!(f.adb.read_file(path, NONE), Ok(binary()), "{tag}");
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
        f.adb.write_file(RELEASE, b"changed", NONE).expect("write");
        assert_eq!(std::fs::read(f.fake.on_device(RELEASE)).expect("file"), b"changed");
        let calls = f.fake.calls();
        let made = calls.iter().find(|c| c.starts_with("exec-in ")).expect("exec-in");
        assert!(made.contains("set -C; umask 117; cat > ./.simee-write-"), "made with its mode, 660: {made}");
        // The temp file gets its context and owner, never through a symlink, and only then
        // replaces the file.
        let last = calls.last().expect("calls");
        let tmp = last.split("mv -f ").nth(1).and_then(|r| r.split(' ').next()).expect("mv's source");
        assert!(tmp.starts_with("./.simee-write-"), "{last}");
        let restorecon = last.find(&format!("restorecon \"$(pwd -P)\"/{}", &tmp[2..])).expect("restorecon the temp file");
        let chown = last.find(&format!("chown -h 10123:10123 {tmp}")).expect("chown the temp file to the file's owner");
        let mv = last.find(&format!("mv -f {tmp} '\\''./r.txt'\\''")).expect("one rename within the dir");
        assert!(restorecon < chown && chown < mv && !last.contains("chmod"), "before the rename: {last}");
        assert_eq!(f.fake.chowns().last().map(|c| c.contains(".simee-write-")), Some(true), "{:?}", f.fake.chowns());

        // A new file gets the container's owner, and the mode an app's own files have.
        f.adb.write_file("data/data/com.example.release/files/fresh.txt", b"new", NONE).expect("new");
        let calls = f.fake.calls();
        assert!(calls.iter().rev().find(|c| c.starts_with("exec-in ")).expect("exec-in").contains("umask 177"), "0600");
        let last = calls.last().expect("calls");
        assert!(last.contains("chown -h 10123:10123 ") && last.contains("restorecon ") && !last.contains("chmod"), "{last}");
        // A mode umask can't give (execute bits) is set with chmod, on the temp file, while it
        // is still root's.
        f.fake.set_owner("10123:10123:770");
        f.adb.write_file(RELEASE, b"x", NONE).expect("770");
        let last = f.fake.calls().pop().expect("calls");
        let chmod = last.find("chmod 770 ./.simee-write-").expect("chmod");
        assert!(chmod < last.find("chown -h").expect("chown") && chmod < last.find("mv -f").expect("mv"), "{last}");

        // Through run-as the app's own uid writes it: owner and context are the app's already.
        let f = fixture("ownerrunas", Root::Adbd);
        f.adb.write_file(NOTES, b"mine", NONE).expect("run-as write");
        let last = f.fake.calls().pop().expect("calls");
        assert!(last.starts_with("shell run-as com.example.app ") && !last.contains("chown") && !last.contains("restorecon"), "{last}");

        // External storage's owner and context are its FUSE daemon's to say, even as root.
        let f = fixture("ownerext", Root::Adbd);
        f.adb.write_file("sdcard/Android/data/com.example.app/x.txt", b"x", NONE).expect("external write");
        let last = f.fake.calls().pop().expect("calls");
        assert!(last.contains("mv -f ") && !last.contains("chown") && !last.contains("restorecon"), "{last}");
        // A container that isn't there (the release app has no external dir) is said to be missing.
        let err = f.adb.write_file("sdcard/Android/data/com.example.release/x.txt", b"x", NONE).expect_err("no external dir");
        assert!(err.contains("no such container"), "{err}");
    }

    #[test]
    fn writes_refuse_directories_and_the_apk() {
        let f = fixture("wrefuse", Root::Adbd);
        let err = f.adb.write_file("data/data/com.example.app/files", b"x", NONE).expect_err("a directory");
        assert!(err.contains("Is a directory"), "{err}");
        let apk = "data/app/~~Ab==/com.example.app-Cd==/base.apk";
        for err in [
            f.adb.write_file(apk, b"x", NONE),
            f.adb.remove(apk, NONE),
            f.adb.clear_container("data/app/~~Ab==/com.example.app-Cd==", NONE),
        ] {
            assert!(err.expect_err("read only").contains("read only"));
        }
        // But it reads, as the shell.
        assert_eq!(f.adb.read_file(apk, NONE).as_deref(), Ok(&b"PK\x03\x04app"[..]));
        assert_eq!(f.adb.du(apk, NONE), Ok(7));
    }

    #[test]
    fn ls_lists_directories_first_with_sizes() {
        let f = fixture("ls", Root::None);
        assert_eq!(
            f.adb.ls("data/data/com.example.app", NONE),
            Ok(vec![
                Entry { name: "cache".into(), dir: true, bytes: 0 },
                Entry { name: "files".into(), dir: true, bytes: 0 },
                Entry { name: "lib".into(), dir: true, bytes: 0 },
                Entry { name: "shared_prefs".into(), dir: true, bytes: 0 },
            ])
        );
        let files = f.adb.ls("/data/user/0/com.example.app/files/", NONE).expect("files");
        assert_eq!(files.iter().map(|e| (e.name.as_str(), e.dir)).collect::<Vec<_>>(), [("notes.txt", false), ("out", false)]);
        assert_eq!(files[0].bytes, 5);
        assert!(f.adb.ls(NOTES, NONE).expect_err("a file").contains("Not a directory"));
        assert!(f.adb.ls("data/data/com.example.app/nope", NONE).expect_err("missing").contains("No such file or directory"));
    }

    #[test]
    fn rm_removes_a_file_or_a_tree() {
        let f = fixture("rm", Root::None);
        f.adb.remove(NOTES, NONE).expect("file");
        assert!(!f.fake.on_device(NOTES).exists());
        f.adb.remove("data/data/com.example.app/shared_prefs", NONE).expect("tree");
        assert!(!f.fake.on_device("data/data/com.example.app/shared_prefs").exists());
        assert!(f.adb.remove(NOTES, NONE).expect_err("gone").contains("No such file or directory"));
    }

    #[test]
    fn du_sums_the_files_in_a_tree() {
        let f = fixture("du", Root::None);
        // notes.txt (5), a.xml (7), c.bin (3), lib/.keep (0); files/out's target isn't counted.
        assert_eq!(f.adb.du("data/data/com.example.app", NONE), Ok(15));
        assert_eq!(f.adb.du(NOTES, NONE), Ok(5));
        assert_eq!(f.adb.du("data/data/com.example.app/missing", NONE), Ok(0), "as iOS's: nothing there is 0 bytes");
        let f = fixture("duroot", Root::Su);
        assert_eq!(f.adb.du("data/data/com.example.release", NONE), Ok(7));
    }

    #[test]
    fn clear_container_empties_it_and_keeps_it() {
        let f = fixture("clear", Root::None);
        f.adb.clear_container("data/data/com.example.app", NONE).expect("clear");
        let left: Vec<String> = std::fs::read_dir(f.fake.on_device("data/data/com.example.app"))
            .expect("still there")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["lib"], "the system's lib dir stays");
        assert!(f.fake.outside.join("secret.txt").exists(), "a symlink's target isn't followed");
        assert!(f.fake.calls().iter().all(|c| !c.contains("pm clear")), "{:?}", f.fake.calls());
        // A directory inside it.
        f.adb.write_file("sdcard/Android/data/com.example.app/files/more.txt", b"m", NONE).expect("write");
        f.adb.clear_container("sdcard/Android/data/com.example.app/files", NONE).expect("external");
        assert_eq!(std::fs::read_dir(f.fake.on_device("sdcard/Android/data/com.example.app/files")).expect("dir").count(), 0);

        // No run-as and no root: the data container is cleared by `pm clear`, when asked for.
        f.adb.clear_container("data/user/0/com.example.release", PM_CLEAR).expect("pm clear");
        assert!(f.fake.calls().contains(&"shell pm clear --user 0 com.example.release".to_owned()), "{:?}", f.fake.calls());
        assert_eq!(std::fs::read_dir(f.fake.on_device("data/user/0/com.example.release")).expect("dir").count(), 0);
        // But not a directory inside it: pm clears only the whole of it.
        let err = f.adb.clear_container("data/user/0/com.example.release/files", NONE).expect_err("no access");
        assert!(err.starts_with("can't reach com.example.release's files") && !err.contains("pm_clear"), "{err}");
        let err = f.adb.clear_container("data/user/0/com.example.release/files", PM_CLEAR).expect_err("not the whole");
        assert!(err.contains("`pm_clear` clears an app's whole data dir"), "{err}");
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
        assert_eq!(f.adb.containers(false, NONE), Ok(vec![app.clone(), release.clone()]));
        assert_eq!(f.fake.calls(), ["shell pm list packages -3 -f"]);
        // Sizes: what each can be reached for (the release app's data can't be, here).
        let sized = f.adb.containers(true, ADB_ROOT).expect("sizes");
        assert_eq!((sized[0].data_bytes, sized[0].bundle_bytes), (15, 7));
        assert_eq!((sized[1].data_bytes, sized[1].bundle_bytes), (0, 11));
        let roots = f.fake.calls().iter().filter(|c| *c == "root").count();
        assert_eq!(roots, 1, "root is asked for once per call, not once per app: {:?}", f.fake.calls());
    }

    // Review of #470, finding 1: `pm clear` clears the container's own user, never user 0's.
    #[test]
    fn pm_clear_clears_the_containers_own_user() {
        let f = fixture("pmuser10", Root::None);
        f.adb.clear_container("data/user/10/com.example.release", PM_CLEAR).expect("user 10");
        assert!(f.fake.calls().contains(&"shell pm clear --user 10 com.example.release".to_owned()), "{:?}", f.fake.calls());
        assert_eq!(std::fs::read_dir(f.fake.on_device("data/user/10/com.example.release")).expect("dir").count(), 0);
        assert!(f.fake.on_device(RELEASE).exists(), "user 0's data is untouched");

        let f = fixture("pmuser0", Root::None);
        f.adb.clear_container("data/data/com.example.release", PM_CLEAR).expect("user 0");
        assert!(f.fake.calls().contains(&"shell pm clear --user 0 com.example.release".to_owned()), "{:?}", f.fake.calls());
        assert!(!f.fake.on_device(RELEASE).exists());
        assert!(f.fake.on_device("data/user/10/com.example.release/files/r10.txt").exists(), "user 10's data is untouched");
    }

    // Finding 2: the directories a root write makes are the app's, with its context.
    #[test]
    fn a_root_write_makes_its_parent_dirs_the_apps() {
        let f = fixture("rootdirs", Root::Adbd);
        f.adb.write_file("data/user/0/com.example.release/files/new/dir/blob.bin", b"x", NONE).expect("write");
        let (chowns, labels) = (f.fake.chowns(), f.fake.restorecons());
        for dir in ["data/user/0/com.example.release/files/new", "data/user/0/com.example.release/files/new/dir"] {
            assert!(chowns.contains(&format!("10123:10123 {dir}")), "{dir} is the app's: {chowns:?}");
            assert!(labels.contains(&dir.to_owned()), "{dir} has the app's context: {labels:?}");
        }
        // The dir that was there already keeps its owner: only what the write made is changed.
        assert!(!chowns.iter().any(|c| c.ends_with("com.example.release/files")), "{chowns:?}");
    }

    // Finding 3: a temp file swapped for a symlink is refused, and nothing outside is changed.
    #[test]
    fn a_root_write_never_follows_a_swapped_temp_file() {
        let f = fixture("swap", Root::Adbd);
        let victim = f.fake.outside.join("victim");
        std::fs::write(&victim, b"victim!").expect("victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).expect("mode");
        f.fake.swap_temp_files();
        let err = f.adb.write_file(RELEASE, b"hostile", NONE).expect_err("refused");
        assert!(err.contains("temp file"), "{err}");
        assert_eq!(std::fs::read(f.fake.on_device(RELEASE)).expect("file"), b"release", "the file is as it was");
        assert_eq!(std::fs::read(&victim).expect("victim"), b"victim!");
        assert_eq!(std::fs::symlink_metadata(&victim).expect("victim").permissions().mode() & 0o777, 0o644, "not chmodded");
        assert!(f.fake.chowns().iter().all(|c| !c.contains("outside")), "nothing outside is chowned: {:?}", f.fake.chowns());
        let left = std::fs::read_dir(f.fake.on_device("data/data/com.example.release/files")).expect("dir").flatten();
        assert!(left.filter(|e| e.file_name().to_string_lossy().starts_with(".simee-")).count() == 0, "no temp file is left");
    }

    // Finding 3: the temp file is made with O_EXCL (`set -C`) under a name nobody can guess.
    #[test]
    fn a_write_makes_its_temp_file_exclusively_under_a_random_name() {
        let f = fixture("tmpname", Root::Adbd);
        f.adb.write_file(RELEASE, b"one", NONE).expect("one");
        f.adb.write_file(RELEASE, b"two", NONE).expect("two");
        let names: Vec<String> = f
            .fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("exec-in "))
            .map(|c| {
                assert!(c.contains("set -C"), "made with O_EXCL: {c}");
                let name = c.split(".simee-write-").nth(1).expect("temp name");
                name.chars().take_while(char::is_ascii_hexdigit).collect()
            })
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().all(|n| n.len() == 32), "128 random bits: {names:?}");
        assert_ne!(names[0], names[1]);
    }

    // Finding 5: a write whose finish fails leaves the file as it was and no temp file.
    #[test]
    fn a_failed_write_leaves_the_file_and_no_temp_file() {
        let f = fixture("chownfails", Root::Adbd);
        f.fake.fail_chown();
        assert!(f.adb.write_file(RELEASE, b"changed", NONE).is_err());
        assert_eq!(std::fs::read(f.fake.on_device(RELEASE)).expect("file"), b"release", "not replaced");
        let left = std::fs::read_dir(f.fake.on_device("data/data/com.example.release/files")).expect("dir").flatten();
        let left: Vec<_> = left.map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["r.txt"], "the temp file is removed");
    }

    // Finding 4: no read restarts adbd unless asked to.
    #[test]
    fn reads_never_restart_adbd_without_consent() {
        let f = fixture("noconsent", Root::AdbRoot);
        let errs = [
            f.adb.read_file(RELEASE, NONE).map(|_| ()),
            f.adb.ls("data/data/com.example.release", NONE).map(|_| ()),
            f.adb.du("data/data/com.example.release", NONE).map(|_| ()),
            f.adb.write_file(RELEASE, b"x", NONE),
            f.adb.remove(RELEASE, NONE),
        ];
        for err in errs {
            let err = err.expect_err("needs consent");
            assert!(err.contains("`root: true`") && err.contains("restarts adbd"), "{err}");
        }
        let sized = f.adb.containers(true, NONE).expect("containers");
        assert_eq!(sized[1].data_bytes, 0, "the release app's data can't be sized without consent");
        assert!(!f.fake.rooted() && !f.fake.calls().contains(&"root".to_owned()), "{:?}", f.fake.calls());
    }

    // Finding 6: `pm clear` (it kills the app, resets its permissions and wipes its external dir)
    // runs only when asked for.
    #[test]
    fn clear_container_never_falls_back_to_pm_clear() {
        let f = fixture("nopmclear", Root::None);
        let err = f.adb.clear_container("data/user/0/com.example.release", NONE).expect_err("no way in");
        assert!(err.contains("`pm_clear: true`"), "names the opt-in: {err}");
        assert!(f.fake.calls().iter().all(|c| !c.contains("pm clear")), "{:?}", f.fake.calls());
        assert!(f.fake.on_device(RELEASE).exists());
    }

    // Finding 7: a read is bounded.
    #[test]
    fn a_read_larger_than_the_bound_is_refused_before_it_is_read() {
        let f = fixture("bigread", Root::None);
        let big = "data/data/com.example.app/files/big.bin";
        std::fs::File::create(f.fake.on_device(big)).and_then(|file| file.set_len(129 << 20)).expect("sparse file");
        let err = f.adb.read_file(big, NONE).expect_err("too big");
        assert!(err.contains("128 MiB"), "{err}");
        assert!(f.fake.calls().iter().all(|c| !c.starts_with("exec-out")), "nothing was read: {:?}", f.fake.calls());
    }

    // A pin: quotes, `$()` and newlines in a file name stay literal through both quoting layers.
    #[test]
    fn shell_metacharacters_in_a_file_name_stay_literal() {
        for (tag, root) in [("metarunas", Root::None), ("metaroot", Root::Adbd)] {
            let f = fixture(tag, root);
            let dir = if root == Root::None { "com.example.app" } else { "com.example.release" };
            let path = format!("data/data/{dir}/files/it's $(touch pwned) `touch pwned2`\nx.txt");
            f.adb.write_file(&path, b"odd", NONE).expect(tag);
            assert_eq!(f.adb.read_file(&path, NONE).as_deref(), Ok(&b"odd"[..]), "{tag}");
            f.adb.remove(&path, NONE).expect(tag);
            assert!(!f.fake.on_device(&path).exists(), "{tag}");
            // The fake's shell runs in the test's own directory.
            let cwd = std::env::current_dir().expect("cwd");
            let pwned = std::fs::read_dir(cwd).expect("cwd").flatten().any(|e| e.file_name().to_string_lossy().starts_with("pwned"));
            assert!(!pwned, "{tag}: nothing ran");
        }
    }
}
