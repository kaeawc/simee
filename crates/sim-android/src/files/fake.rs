//! A stand-in adb for the app-file methods ([`super`]), for tests here and in crates that drive
//! Android devices (feature `testing`). It keeps a device's filesystem in a host directory and
//! runs each command the way the guest would: `run-as <package>` only for a debuggable package,
//! `su 0` and `adb root` only when the image allows them, the shell's own uid kept out of the apps'
//! data dirs, and the rest run by the host's `sh` with the device's paths mapped into that
//! directory. `stat` (toybox's `-c` formats), `chown` and `restorecon` are stand-ins too: `stat`
//! answers every file's owner and mode from [`FakeFiles::set_owner`].
//!
//! The device has two third-party apps: `com.example.app`, debuggable, with `files/notes.txt`
//! (`hello`), `shared_prefs/a.xml`, `cache/c.bin`, a `lib` dir (as the system leaves one) and
//! `files/out`, a symlink to a directory outside the container holding `secret.txt`; and
//! `com.example.release`, a release build with `files/r.txt` (`release`) and, for user 10,
//! `data/user/10/com.example.release/files/r10.txt`. Each has an APK
//! (`data/app/~~<x>==/<package>-<y>==/base.apk`), `data/data` links to `data/user/0`, and
//! `com.example.app` has `sdcard/Android/data/com.example.app/files/ext.txt`.

use std::path::{Path, PathBuf};

use crate::Android;
use crate::testing::write_script;

/// What root the fake device's image offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    /// None: a production image (`adb root` is refused, there is no `su`).
    None,
    /// adbd already runs as root.
    Adbd,
    /// `su 0` works (a userdebug image whose adbd isn't root).
    Su,
    /// `adb root` restarts adbd as root; until then it isn't.
    AdbRoot,
}

impl Root {
    fn name(self) -> &'static str {
        match self {
            Root::None => "none",
            Root::Adbd => "adbd",
            Root::Su => "su",
            Root::AdbRoot => "adb-root",
        }
    }
}

/// A [`fake_files_adb`]: its log, the device's filesystem, and the directory outside it.
pub struct FakeFiles {
    tools: PathBuf,
    /// The device's `/`.
    pub device: PathBuf,
    /// A directory outside every container (`files/out` in `com.example.app` links to it).
    pub outside: PathBuf,
}

/// Installs the fake as `android`'s adb (the SDK's `platform-tools/adb`), its device seeded as
/// the module docs say, with `root`.
pub fn fake_files_adb(android: &Android, root: Root) -> FakeFiles {
    let tools = android.env.sdk_root.as_ref().expect("sdk root").join("platform-tools");
    let device = tools.join("device");
    let outside = tools.join("outside");
    let _ = std::fs::remove_dir_all(&device);
    let file = |rel: &str, bytes: &[u8]| {
        let path = device.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, bytes).expect("seed");
    };
    file("data/user/0/com.example.app/files/notes.txt", b"hello");
    file("data/user/0/com.example.app/shared_prefs/a.xml", b"<map />");
    file("data/user/0/com.example.app/cache/c.bin", &[0, 1, 2]);
    file("data/user/0/com.example.app/lib/.keep", b"");
    file("data/user/0/com.example.release/files/r.txt", b"release");
    file("data/user/10/com.example.release/files/r10.txt", b"user 10");
    file("sdcard/Android/data/com.example.app/files/ext.txt", b"external");
    file("data/app/~~Ab==/com.example.app-Cd==/base.apk", b"PK\x03\x04app");
    file("data/app/~~Ef==/com.example.release-Gh==/base.apk", b"PK\x03\x04release");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("secret.txt"), b"secret").expect("secret");
    std::os::unix::fs::symlink("user/0", device.join("data/data")).expect("data/data");
    std::os::unix::fs::symlink(&outside, device.join("data/user/0/com.example.app/files/out")).expect("out");
    std::fs::write(tools.join("debuggable"), "com.example.app\n").expect("debuggable");
    std::fs::write(tools.join("owner"), "10123:10123:660\n").expect("owner");
    let _ = std::fs::remove_file(tools.join("rooted"));
    for state in ["adb.log", "chown.log", "restorecon.log", "attack", "chown-fails"] {
        let _ = std::fs::remove_file(tools.join(state));
    }
    std::fs::write(tools.join("root"), root.name()).expect("root");
    let bin = tools.join("fakebin");
    std::fs::create_dir_all(&bin).expect("fakebin");
    write_script(&bin.join("stat"), FAKE_STAT);
    write_script(&bin.join("chown"), FAKE_CHOWN);
    write_script(&bin.join("restorecon"), FAKE_RESTORECON);
    write_script(&tools.join("adb"), FAKE_ADB);
    FakeFiles { tools, device, outside }
}

impl FakeFiles {
    /// Each invocation's arguments, in order, without the leading `-s <serial>`.
    pub fn calls(&self) -> Vec<String> {
        let text = std::fs::read_to_string(self.tools.join("adb.log")).unwrap_or_default();
        text.lines().map(|l| l.strip_prefix("-s ").and_then(|r| r.split_once(' ')).map_or(l, |(_, r)| r).to_owned()).collect()
    }

    /// The `uid:gid:mode` `stat` answers for every file.
    pub fn set_owner(&self, owner: &str) {
        std::fs::write(self.tools.join("owner"), format!("{owner}\n")).expect("owner");
    }

    /// Where the device's `path` is on the host.
    pub fn on_device(&self, path: &str) -> PathBuf {
        self.device.join(path.trim_start_matches('/'))
    }

    /// Each `chown` as `<uid:gid> <path>`: the path it changed (a symlink's target, unless `-h`),
    /// relative to the device's `/` with symlinks resolved (`data/user/0`, not `data/data`).
    pub fn chowns(&self) -> Vec<String> {
        self.changed("chown.log")
    }

    /// Each path `restorecon` relabelled, as [`FakeFiles::chowns`] has them.
    pub fn restorecons(&self) -> Vec<String> {
        self.changed("restorecon.log")
    }

    fn changed(&self, log: &str) -> Vec<String> {
        let device = format!("{}/", std::fs::canonicalize(&self.device).expect("device").display());
        let text = std::fs::read_to_string(self.tools.join(log)).unwrap_or_default();
        text.lines().map(|l| l.replace(&device, "")).collect()
    }

    /// From now on a hostile app swaps each temp file a write makes for a symlink to
    /// `outside/victim`, as soon as `exec-in` has written it.
    pub fn swap_temp_files(&self) {
        std::fs::write(self.tools.join("attack"), "swap").expect("attack");
    }

    /// From now on `chown` fails.
    pub fn fail_chown(&self) {
        std::fs::write(self.tools.join("chown-fails"), "").expect("chown-fails");
    }

    /// Whether `adb root` restarted adbd as root.
    pub fn rooted(&self) -> bool {
        Path::new(&self.tools.join("rooted")).exists()
    }
}

const FAKE_STAT: &str = r#"[ "$1" = -c ] || { echo "fake stat: -c only" >&2; exit 2; }
fmt=$2; shift 2; rc=0
IFS=: read -r ou og om < "$(dirname "$0")/../owner"
for f in "$@"; do
  if [ -L "$f" ]; then t="symbolic link"; s=$(readlink "$f" | tr -d '\n' | wc -c | tr -d ' ')
  elif [ -d "$f" ]; then t=directory; s=4096
  elif [ -e "$f" ]; then t="regular file"; s=$(wc -c < "$f" | tr -d ' ')
  else echo "stat: '$f': No such file or directory" >&2; rc=1; continue; fi
  printf '%s\n' "$fmt" | sed -e "s|%F|$t|g" -e "s|%s|$s|g" -e "s|%u|$ou|g" -e "s|%g|$og|g" -e "s|%a|$om|g" -e "s|%n|$f|g"
done
exit $rc
"#;

/// Logs what it changes to `chown.log`: with `-h` the link itself, otherwise what it points at.
const FAKE_CHOWN: &str = r#"here=$(dirname "$0")/..
h=; [ "$1" = -h ] && { h=1; shift; }
[ -e "$here/chown-fails" ] && { echo "chown: Operation not permitted" >&2; exit 1; }
ids=$1; shift; rc=0
for f in "$@"; do
  { [ -e "$f" ] || [ -L "$f" ]; } || { echo "chown: $f: No such file or directory" >&2; rc=1; continue; }
  if [ -n "$h" ]; then r="$(cd "$(dirname "$f")" && pwd -P)/$(basename "$f")"; else r=$(realpath "$f"); fi
  echo "$ids $r" >> "$here/chown.log"
done
exit $rc
"#;

/// Logs what it relabels to `restorecon.log`, never following a symlink (toybox's doesn't).
const FAKE_RESTORECON: &str = r#"here=$(dirname "$0")/..
rc=0
for f in "$@"; do
  { [ -e "$f" ] || [ -L "$f" ]; } || { echo "restorecon: $f: No such file or directory" >&2; rc=1; continue; }
  echo "$(cd "$(dirname "$f")" && pwd -P)/$(basename "$f")" >> "$here/restorecon.log"
done
exit $rc
"#;

const FAKE_ADB: &str = r#"here=$(dirname "$0")
echo "$*" >> "$here/adb.log"
[ "$1" = "-s" ] && shift 2
dev="$here/device"
mode=$(cat "$here/root")
rooted() { [ "$mode" = adbd ] || [ -e "$here/rooted" ]; }
case "$1" in
root)
  if rooted; then echo "adbd is already running as root"; exit 0; fi
  if [ "$mode" = adb-root ]; then touch "$here/rooted"; echo "restarting adbd as root"; exit 0; fi
  echo "adbd cannot run as root in production builds"; exit 1;;
wait-for-device) exit 0;;
shell|exec-out|exec-in) verb=$1;;
*) echo "fake adb: unhandled: $*" >&2; exit 2;;
esac
shift
cmd="$*"
case "$cmd" in
"id -u") if rooted; then echo 0; else echo 2000; fi; exit 0;;
"pm list packages -3 -f")
  for apk in "$dev"/data/app/*/*/base.apk; do
    d=${apk#"$dev"}; p=${d%/base.apk}; p=${p##*/}; p=${p%-*}
    echo "package:$d=$p"
  done
  exit 0;;
"pm clear "*)
  # pm clear [--user <n>] <package>: user 0 when no --user is given, as pm's.
  pkg=${cmd#pm clear }; user=0
  case "$pkg" in "--user "*) pkg=${pkg#--user }; user=${pkg%% *}; pkg=${pkg#"$user" };; esac
  if [ -d "$dev/data/user/$user/$pkg" ]; then
    find "$dev/data/user/$user/$pkg" -mindepth 1 -maxdepth 1 ! -name lib -exec rm -rf {} +
    echo Success
  else
    echo Failed
  fi
  exit 0;;
"run-as "*)
  rest=${cmd#run-as }; pkg=${rest%% *}; cmd=${rest#"$pkg" }
  [ -d "$dev/data/user/0/$pkg" ] || { echo "run-as: unknown package: $pkg" >&2; exit 1; }
  grep -qx "$pkg" "$here/debuggable" || { echo "run-as: package not debuggable: $pkg" >&2; exit 1; }
  [ "$cmd" = "id -u" ] && { echo 10123; exit 0; };;
"su 0 "*)
  [ "$mode" = su ] || { echo "/system/bin/sh: su: inaccessible or not found" >&2; exit 127; }
  cmd=${cmd#su 0 }
  [ "$cmd" = "id -u" ] && { echo 0; exit 0; };;
*)
  if ! rooted; then
    case "$cmd" in *"/data/user/"*|*"/data/data/"*) echo "/system/bin/sh: Permission denied" >&2; exit 1;; esac
  fi;;
esac
# Every device path the client sends is quoted: map those (and only those, not the `/data/` in
# `/sdcard/Android/data/`) into the device's directory.
cmd=$(printf '%s' "$cmd" | sed -e "s#'/data/#'$dev/data/#g" -e "s#'/sdcard/#'$dev/sdcard/#g" -e "s#'/storage/emulated/0/#'$dev/sdcard/#g")
if [ "$verb" = exec-in ] && [ -e "$here/attack" ]; then
  PATH="$here/fakebin:$PATH" sh -c "$cmd"; rc=$?
  find "$dev" -name '.simee-write-*' ! -type l | while read -r t; do rm -f "$t"; ln -s "$here/outside/victim" "$t"; done
  exit $rc
fi
PATH="$here/fakebin:$PATH" exec sh -c "$cmd"
"#;
