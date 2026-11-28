//! App lifecycle on a running device, over adb: install, uninstall, launch, terminate, list and
//! open a URL. Any emulator that has an adb serial works (simee's `emu` and the stock one); adb
//! comes from the SDK's platform-tools or the PATH ([`Android::adb`]).
//!
//! `adb shell` hands its arguments to the guest's shell as one string, so every value that came
//! from a caller is either checked against what it must be (a package name) or quoted
//! ([`shell_quote`]).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::{Android, Device};

/// How long `launch` waits, by default, for the app's process to show up after `am start -W`
/// returned.
const PID_WAIT: Duration = Duration::from_secs(3);
const PID_POLL: Duration = Duration::from_millis(100);

/// The intent a launcher sends: what `launch` resolves and starts.
const LAUNCHER_INTENT: &str = "-a android.intent.action.MAIN -c android.intent.category.LAUNCHER";

/// adb bound to one running device.
#[derive(Clone, Debug)]
pub struct Adb {
    pub adb: PathBuf,
    pub serial: String,
    /// `bundletool`, when the host has one: what installs an `.aab`.
    pub bundletool: Option<PathBuf>,
    /// How long `launch` looks for the app's process.
    pub pid_wait: Duration,
    /// How long each kind of adb call may take before it is stopped and reported.
    pub deadlines: Deadlines,
}

/// How long each kind of adb (and bundletool) call may run. One that runs longer, as on a guest
/// whose adbd is wedged but still listed as a `device`, is killed and reported as timed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    /// `adb install`, bundletool and `pm uninstall`: copying and optimizing a large app.
    pub install: Duration,
    /// `am start`: with `-W` it waits for the activity's first frame.
    pub start: Duration,
    /// Everything else (`cmd package`, `pidof`, `pm list`, `am force-stop`): answers at once.
    pub query: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Deadlines { install: Duration::from_secs(300), start: Duration::from_secs(30), query: Duration::from_secs(10) }
    }
}

/// What `launch` started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launched {
    /// The app's process, `None` when no process named for the package showed up.
    pub pid: Option<u32>,
    /// The launcher activity it started (`com.example.app/.MainActivity`).
    pub activity: String,
}

impl Device {
    /// adb for this device. Fails when it isn't running or the host has no adb.
    pub fn adb(&self, android: &Android) -> Result<Adb, String> {
        let serial = self.running()?.serial();
        let adb = android.adb().ok_or("adb not found: install the SDK's platform-tools (`emu -provision`) or put adb on the PATH")?;
        Ok(Adb { adb, serial, bundletool: on_path("bundletool"), pid_wait: PID_WAIT, deadlines: Deadlines::default() })
    }
}

impl Adb {
    /// Runs adb on the device with `args` after `-s <serial>`, for at most `limit`: its success
    /// and its stdout and stderr, in that order, as text.
    pub(crate) fn run(&self, args: &[&str], limit: Duration) -> Result<(bool, String), String> {
        let what = format!("adb -s {} {}", self.serial, args.join(" "));
        let (ok, mut text, stderr) = output_within(Command::new(&self.adb).args(["-s", &self.serial]).args(args), limit, &what)?;
        text.push_str(&stderr);
        Ok((ok, text))
    }

    /// `adb shell <command>`, for at most `limit`: what it printed, or why it failed.
    pub(crate) fn shell(&self, command: &str, limit: Duration) -> Result<String, String> {
        match self.run(&["shell", command], limit)? {
            (true, text) => Ok(text),
            (false, text) => Err(failure(command, &text)),
        }
    }

    /// Installs an app over the one of the same name, if any: an `.apk` with `adb install -r`, an
    /// `.aab` through `bundletool` when the host has it.
    pub fn install(&self, path: &Path) -> Result<(), String> {
        if !path.is_file() {
            return Err(format!("{} isn't a file", path.display()));
        }
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("apk") => {
                let (ok, text) = self.run(&["install", "-r", &path.to_string_lossy()], self.deadlines.install)?;
                install_result(ok, &text)
            }
            Some("aab") => self.install_bundle(path),
            _ => Err(format!("{} isn't an .apk or .aab", path.display())),
        }
    }

    /// An `.aab`: `bundletool build-apks` for this device, then `install-apks`, through a scratch
    /// directory of this call's own (calls can run at once on one device), removed after.
    fn install_bundle(&self, bundle: &Path) -> Result<(), String> {
        static SCRATCH: AtomicU64 = AtomicU64::new(0);
        let bundletool = self.bundletool.as_ref().ok_or(
            "installing an .aab needs `bundletool` on the PATH (https://developer.android.com/tools/bundletool); an .apk installs without it",
        )?;
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("simee-aab-{}-{}-{n}", std::process::id(), self.serial));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let apks = dir.join("app.apks");
        let tool = |args: Vec<String>| -> Result<(), String> {
            let what = format!("bundletool {}", args[0]);
            match output_within(Command::new(bundletool).args(args), self.deadlines.install, &what)? {
                (true, ..) => Ok(()),
                (false, stdout, stderr) => Err(failure("bundletool", &format!("{stderr}{stdout}"))),
            }
        };
        let (adb, id) = (format!("--adb={}", self.adb.display()), format!("--device-id={}", self.serial));
        let built = tool(vec![
            "build-apks".into(),
            format!("--bundle={}", bundle.display()),
            format!("--output={}", apks.display()),
            "--connected-device".into(),
            "--overwrite".into(),
            adb.clone(),
            id.clone(),
        ]);
        let result = built.and_then(|()| tool(vec!["install-apks".into(), format!("--apks={}", apks.display()), adb, id]));
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// `pm uninstall`.
    pub fn uninstall(&self, package: &str) -> Result<(), String> {
        let package = package_name(package)?;
        install_result(true, &self.shell(&format!("pm uninstall {package}"), self.deadlines.install)?)
    }

    /// Starts the app's own launcher activity, the first one its package lists (an app can have
    /// more than one, as a debug build with LeakCanary's "Leaks" alias does, where resolving one
    /// gives the system's chooser), and returns it with the app's process id (`None` when no
    /// process named for the package shows up: an activity declared in another process). With
    /// `restart` a running instance is stopped first (`am start -S`); without, it is brought to
    /// the front.
    pub fn launch(&self, package: &str, restart: bool) -> Result<Launched, String> {
        let package = package_name(package)?;
        let found =
            self.shell(&format!("cmd package query-activities --brief --components {LAUNCHER_INTENT} {package}"), self.deadlines.query)?;
        let activity = launcher_component(&found, package)
            .ok_or_else(|| format!("{package} has no launcher activity (is it installed? `apps` lists what is)"))?;
        let stop = if restart { " -S" } else { "" };
        let started = self.shell(&format!("am start -W{stop} {LAUNCHER_INTENT} -n {}", shell_quote(&activity)), self.deadlines.start)?;
        if let Some(error) = am_error(&started) {
            return Err(format!("launching {package}: {error}"));
        }
        let deadline = Instant::now() + self.pid_wait;
        loop {
            let pid = self.pid(package)?;
            if pid.is_some() || Instant::now() >= deadline {
                return Ok(Launched { pid, activity });
            }
            sleep(PID_POLL);
        }
    }

    /// The process named `package`, if one runs. toybox's `pidof` exits 1 when none does, and adb
    /// passes that on: a failure with no output is "no process (yet)", not an error.
    fn pid(&self, package: &str) -> Result<Option<u32>, String> {
        let command = format!("pidof -s {package}");
        match self.run(&["shell", &command], self.deadlines.query)? {
            (true, text) => Ok(parse_pid(&text)),
            (false, text) if text.trim().is_empty() => Ok(None),
            (false, text) => Err(failure(&command, &text)),
        }
    }

    /// `am force-stop`.
    pub fn terminate(&self, package: &str) -> Result<(), String> {
        let package = package_name(package)?;
        self.shell(&format!("am force-stop {package}"), self.deadlines.query).map(|_| ())
    }

    /// The installed packages, sorted: the third-party ones, or with `all` the system's too.
    pub fn apps(&self, all: bool) -> Result<Vec<String>, String> {
        Ok(parse_packages(&self.shell(if all { "pm list packages" } else { "pm list packages -3" }, self.deadlines.query)?))
    }

    /// Opens `url` with whatever handles it (`am start -a android.intent.action.VIEW -d`).
    pub fn open_url(&self, url: &str) -> Result<(), String> {
        let url = checked_url(url)?;
        let started = self.shell(&format!("am start -a android.intent.action.VIEW -d {}", shell_quote(url)), self.deadlines.start)?;
        match am_error(&started) {
            Some(error) => Err(format!("opening {url}: {error}")),
            None => Ok(()),
        }
    }
}

/// Runs `command` with its output captured, for at most `limit`: whether it succeeded, then its
/// stdout and stderr. Past `limit` it is killed through its own handle (so only that process,
/// never one reusing its PID) and the error names `what`. Output is read as it comes, so the
/// call returns once the command exits even when a process it started still holds its pipes.
fn output_within(command: &mut Command, limit: Duration, what: &str) -> Result<(bool, String, String), String> {
    let ran = capture(command, None, limit, [usize::MAX; 2], what)?;
    if ran.timed_out {
        return Err(format!("`{what}` timed out after {limit:?}; stopped it"));
    }
    let [stdout, stderr] = [ran.stdout, ran.stderr].map(|b| String::from_utf8_lossy(&b).into_owned());
    Ok((ran.code == Some(0), stdout, stderr))
}

/// What [`capture`] collected.
#[derive(Debug, Default)]
pub(crate) struct Captured {
    /// The exit status; `None` when it was stopped (past its time or its cap) or died of a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// A stream reached its cap and was cut there.
    pub truncated: bool,
    /// It ran past its time.
    pub timed_out: bool,
    /// It was stopped, at a cap or past its time, while it still ran: it never exited by itself.
    pub stopped: bool,
}

/// [`output_within`]'s run, keeping at most `caps` bytes of stdout and of stderr: a stream that
/// reaches its cap while the command runs stops it (`truncated`, `stopped`), as running past `limit`
/// does (`timed_out`, `stopped`). One that reaches it in what is read after the command exited is
/// only cut (`truncated`), the command's status kept. `stdin`, when given, is written to the
/// command's input from a thread of its own (so a command that writes before it reads can't stall
/// it) and then closed; without it the input is null.
pub(crate) fn capture(
    command: &mut Command,
    stdin: Option<Vec<u8>>,
    limit: Duration,
    caps: [usize; 2],
    what: &str,
) -> Result<Captured, String> {
    /// How often a quiet command is checked for having exited.
    const TICK: Duration = Duration::from_millis(20);
    let deadline = Instant::now().checked_add(limit).ok_or_else(|| format!("`{what}`: a time limit of {limit:?} is too long"))?;
    let mut child = command
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run {}: {e}", Path::new(command.get_program()).display()))?;
    if let (Some(bytes), Some(mut input)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = input.write_all(&bytes);
        });
    }
    let (tx, rx) = mpsc::channel::<(usize, Option<Vec<u8>>)>();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ];
    let mut open = 0;
    for (i, pipe) in pipes.into_iter().enumerate() {
        let Some(mut pipe) = pipe else { continue };
        open += 1;
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) if tx.send((i, Some(buf[..n].to_vec()))).is_ok() => {}
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    _ => break,
                }
            }
            let _ = tx.send((i, None));
        });
    }
    drop(tx);
    let mut ran = Captured::default();
    let mut out = [Vec::new(), Vec::new()];
    // Keeps what fits of `bytes` under stream `i`'s cap; whether any didn't fit.
    let keep = |out: &mut [Vec<u8>; 2], i: usize, bytes: &[u8]| {
        let room = caps[i].saturating_sub(out[i].len());
        out[i].extend_from_slice(&bytes[..bytes.len().min(room)]);
        bytes.len() > room
    };
    let status = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if open > 0 && !left.is_zero() {
            match rx.recv_timeout(left.min(TICK)) {
                Ok((i, Some(bytes))) => {
                    if keep(&mut out, i, &bytes) {
                        ran.truncated = true;
                        break None;
                    }
                    continue;
                }
                Ok((_, None)) => {
                    open -= 1;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => open = 0,
                Err(RecvTimeoutError::Timeout) => {}
            }
        } else if !left.is_zero() {
            // Both pipes closed: the command is exiting.
            sleep(left.min(Duration::from_millis(1)));
        }
        if let Some(status) = child.try_wait().map_err(|e| format!("`{what}`: {e}"))? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            ran.timed_out = true;
            break None;
        }
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        ran.stopped = true;
    }
    // What it wrote last, or before it was stopped, may still be on its way from the readers. A
    // process it left holding the pipes gets a moment, not the deadline.
    let grace = Instant::now() + Duration::from_millis(100);
    while open > 0 {
        match rx.recv_timeout(grace.saturating_duration_since(Instant::now())) {
            Ok((i, Some(bytes))) => ran.truncated |= keep(&mut out, i, &bytes),
            Ok((_, None)) => open -= 1,
            Err(_) => break,
        }
    }
    ran.code = status.and_then(|s| s.code());
    [ran.stdout, ran.stderr] = out;
    Ok(ran)
}

/// `name` if it is a package name: dot-separated segments of letters, digits and underscores,
/// each starting with a letter, at least two (Android's rule), so it is safe in a shell line.
pub fn package_name(name: &str) -> Result<&str, String> {
    let segment = |s: &str| s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if name.split('.').count() >= 2 && name.split('.').all(segment) {
        Ok(name)
    } else {
        Err(format!("`{name}` isn't a package name (like com.example.app)"))
    }
}

/// `url` if it starts with a scheme (`https:`, `myapp:`).
fn checked_url(url: &str) -> Result<&str, String> {
    let scheme_end = url.find(':').unwrap_or(0);
    let scheme = &url[..scheme_end];
    let valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if valid { Ok(url) } else { Err(format!("`{url}` isn't a URL (it needs a scheme, like https://example.com)")) }
}

/// `s` as one word for the guest's shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The outcome of `pm uninstall` and `adb install`, which print `Success` or `Failure [REASON]`
/// and exit 0 or not depending on the adb version.
fn install_result(ok: bool, text: &str) -> Result<(), String> {
    let says_success = text.lines().any(|l| l.trim() == "Success");
    if says_success && !text.contains("Failure") {
        return Ok(());
    }
    let reason = text.lines().rev().find(|l| l.contains("Failure") || l.contains("Error")).unwrap_or("").trim();
    if reason.is_empty() && ok {
        return Err(format!("no `Success` in: {}", text.trim()));
    }
    Err(failure("adb", if reason.is_empty() { text } else { reason }))
}

/// An error from a command's output, its last line if it printed any.
pub(crate) fn failure(what: &str, text: &str) -> String {
    let text = text.trim();
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
    format!("{what} failed: {}", line.trim())
}

/// `package`'s first launcher component (`com.example.app/.MainActivity`) in `cmd package
/// query-activities --brief --components`'s output, which lists one per line in the launcher's
/// order (`query-receivers` prints receivers the same way). A component of another package (the
/// system's chooser) is never one.
pub(crate) fn launcher_component(out: &str, package: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| !l.contains(' ') && l.split_once('/').is_some_and(|(p, class)| p == package && !class.is_empty()))
        .map(str::to_owned)
}

/// What `am start` reports as an error (it exits 0 either way): the first `Error` line.
fn am_error(out: &str) -> Option<String> {
    out.lines().map(str::trim).find(|l| l.starts_with("Error") && !l.starts_with("Error type")).map(str::to_owned)
}

/// The first process id `pidof` printed.
fn parse_pid(out: &str) -> Option<u32> {
    out.split_whitespace().next()?.parse().ok()
}

/// The names in `pm list packages`'s `package:<name>` lines, sorted.
pub(crate) fn parse_packages(out: &str) -> Vec<String> {
    let mut names: Vec<String> = out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(str::to_owned).collect();
    names.sort();
    names.dedup();
    names
}

/// `name` in the PATH's directories.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Run against `testing`'s fake adb and emulator, which are unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::{fake_adb, fake_emulator};

    #[test]
    fn package_names_are_checked_before_they_reach_a_shell() {
        assert_eq!(package_name("com.example.app"), Ok("com.example.app"));
        assert_eq!(package_name("com.example.app_2"), Ok("com.example.app_2"));
        for bad in ["a", "", "com.example;reboot", "com..app", "com.1app", "$(id).x", "com.example app", "-n.x"] {
            assert!(package_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_need_a_scheme_and_are_quoted() {
        assert!(checked_url("https://example.com/a?b=1&c=2").is_ok());
        assert!(checked_url("myapp:open").is_ok());
        for bad in ["example.com", "a", "", "://x", "1http://x"] {
            assert!(checked_url(bad).is_err(), "{bad}");
        }
        assert_eq!(shell_quote("https://x/?a=1&b=it's"), r"'https://x/?a=1&b=it'\''s'");
    }

    #[test]
    fn install_and_uninstall_outcomes() {
        assert_eq!(install_result(true, "Performing Streamed Install\nSuccess\n"), Ok(()));
        assert_eq!(
            install_result(true, "Failure [DELETE_FAILED_INTERNAL_ERROR]\n"),
            Err("adb failed: Failure [DELETE_FAILED_INTERNAL_ERROR]".into())
        );
        assert_eq!(
            install_result(false, "Performing Streamed Install\nadb: failed to install a.apk: Failure [INSTALL_PARSE_FAILED_NOT_APK]\n"),
            Err("adb failed: adb: failed to install a.apk: Failure [INSTALL_PARSE_FAILED_NOT_APK]".into())
        );
        assert!(install_result(true, "").expect_err("silence isn't success").contains("no `Success`"));
        assert!(install_result(false, "error: device offline").expect_err("offline").contains("device offline"));
    }

    #[test]
    fn the_launcher_component_is_the_packages_first() {
        let app = "com.example.app";
        let leaky = "com.example.app/.MainActivity\ncom.example.app/leakcanary.internal.activity.LeakLauncherActivity\n";
        assert_eq!(launcher_component(leaky, app).as_deref(), Some("com.example.app/.MainActivity"));
        assert_eq!(launcher_component("com.example.app/.Main$Inner", app).as_deref(), Some("com.example.app/.Main$Inner"));
        // Without --components (an older guest) the component follows each match's details.
        let brief = "1 activities found:\n  Activity #0:\n    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\n    com.example.app/.MainActivity\n";
        assert_eq!(launcher_component(brief, app).as_deref(), Some("com.example.app/.MainActivity"));
        assert_eq!(launcher_component("android/com.android.internal.app.ResolverActivity\n", app), None, "the chooser is never the app");
        assert_eq!(launcher_component("com.example.app2/.Main\n", app), None, "another package");
        assert_eq!(launcher_component("No activities found\n", app), None);
        assert_eq!(launcher_component("", app), None);
    }

    #[test]
    fn am_start_errors_are_found_in_its_output() {
        assert_eq!(am_error("Starting: Intent { }\nStatus: ok\nComplete\n"), None);
        assert_eq!(
            am_error("Starting: Intent { }\nError type 3\nError: Activity class {a/b} does not exist.\n").as_deref(),
            Some("Error: Activity class {a/b} does not exist.")
        );
        assert_eq!(am_error("Warning: Activity not started, its current task has been brought to the front\n"), None);
    }

    #[test]
    fn pids_and_package_lists_parse() {
        assert_eq!(parse_pid("4321\n"), Some(4321));
        assert_eq!(parse_pid("4321 4400\n"), Some(4321));
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_packages("package:b.x\r\npackage:a.x\n\nWarning: x\n"), ["a.x", "b.x"]);
    }

    struct Fixture {
        root: PathBuf,
        adb: Adb,
        calls: crate::testing::FakeAdb,
        _fake: crate::testing::FakeEmulator,
    }

    /// A running AVD whose adb is the fake.
    fn fixture(tag: &str) -> Fixture {
        let root = PathBuf::from("/tmp").join(format!("sa-apps-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let calls = fake_adb(&android);
        let _fake = fake_emulator(&android, "phone");
        let device = android.device("phone").expect("device");
        let adb = Adb { pid_wait: Duration::ZERO, ..device.adb(&android).expect("adb") };
        Fixture { root, adb, calls, _fake }
    }

    #[test]
    fn launch_resolves_the_activity_and_returns_the_pid() {
        let f = fixture("launch");
        assert_eq!(
            f.adb.launch("com.example.app", false),
            Ok(Launched { pid: Some(4321), activity: "com.example.app/.MainActivity".into() })
        );
        assert_eq!(
            f.calls.calls(),
            [
                "shell cmd package query-activities --brief --components -a android.intent.action.MAIN -c android.intent.category.LAUNCHER com.example.app",
                "shell am start -W -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n 'com.example.app/.MainActivity'",
                "shell pidof -s com.example.app",
            ]
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn launch_with_restart_stops_the_running_instance() {
        let f = fixture("restart");
        assert_eq!(f.adb.launch("com.example.app", true).map(|l| l.pid), Ok(Some(4321)));
        assert!(f.calls.calls()[1].starts_with("shell am start -W -S "), "{:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn launch_failures_say_why() {
        let f = fixture("launchfail");
        let none = f.adb.launch("com.missing.app", true).expect_err("not installed");
        assert_eq!(none, "com.missing.app has no launcher activity (is it installed? `apps` lists what is)");
        let gone = f.adb.launch("com.example.crash", true).expect_err("activity is gone");
        assert!(gone.starts_with("launching com.example.crash: Error: Activity class"), "{gone}");
        assert!(f.adb.launch("com.example;reboot", true).expect_err("not a package").contains("isn't a package name"));
        assert_eq!(f.calls.calls().len(), 3, "the bad name never reached adb: {:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_process_that_never_shows_up_is_no_pid() {
        // toybox `pidof` exits 1 when nothing matches: launch keeps looking, then says null.
        let mut f = fixture("nopid");
        f.adb.pid_wait = Duration::from_millis(250);
        assert_eq!(f.adb.launch("com.example.svc", false).map(|l| l.pid), Ok(None));
        let probes = f.calls.calls().iter().filter(|c| *c == "shell pidof -s com.example.svc").count();
        assert!(probes > 1, "it polled: {:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_process_that_shows_up_late_is_found() {
        let mut f = fixture("latepid");
        f.adb.pid_wait = Duration::from_secs(5);
        assert_eq!(f.adb.launch("com.example.late", false).map(|l| l.pid), Ok(Some(7777)));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn two_launcher_activities_start_the_apps_own_first_one() {
        // A debug build with LeakCanary has a second launcher (its "Leaks" alias): resolving one
        // gives the system's chooser, which launch must not start.
        let f = fixture("leaky");
        let launched = f.adb.launch("com.example.leaky", false).expect("launch");
        assert_eq!(launched.activity, "com.example.leaky/.MainActivity");
        let starts: Vec<String> = f.calls.calls().into_iter().filter(|c| c.starts_with("shell am start")).collect();
        assert_eq!(
            starts,
            ["shell am start -W -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n 'com.example.leaky/.MainActivity'"]
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn concurrent_aab_installs_on_one_device_keep_their_own_apks() {
        let mut f = fixture("aabs");
        let tool = f.root.join("bundletool");
        // build-apks writes the bundle's name into its output and lingers, so two calls overlap;
        // install-apks records what it was handed.
        crate::testing::write_script(
            &tool,
            &format!(
                "for a; do case $a in --bundle=*) b=${{a#--bundle=}};; --output=*) o=${{a#--output=}};; --apks=*) i=${{a#--apks=}};; esac; done\n\
                 case $1 in\n\
                 build-apks) echo \"$o\" >> {root}/outputs; echo \"$b\" > \"$o\"; sleep 0.3;;\n\
                 install-apks) cat \"$i\" >> {root}/installed || exit 1;;\n\
                 esac\n",
                root = f.root.display()
            ),
        );
        f.adb.bundletool = Some(tool);
        let bundles: Vec<PathBuf> = ["a.aab", "b.aab"].iter().map(|n| f.root.join(n)).collect();
        for b in &bundles {
            std::fs::write(b, b"PK").expect("aab");
        }
        let results: Vec<Result<(), String>> = std::thread::scope(|s| {
            let handles: Vec<_> = bundles.iter().map(|b| s.spawn(|| f.adb.install(b))).collect();
            handles.into_iter().map(|h| h.join().expect("install thread")).collect()
        });
        assert_eq!(results, [Ok(()), Ok(())]);
        let mut installed: Vec<String> =
            std::fs::read_to_string(f.root.join("installed")).expect("installed").lines().map(str::to_owned).collect();
        installed.sort();
        assert_eq!(installed, bundles.iter().map(|b| b.display().to_string()).collect::<Vec<_>>(), "each installed its own bundle");
        let outputs = std::fs::read_to_string(f.root.join("outputs")).expect("outputs");
        let outputs: Vec<&Path> = outputs.lines().map(Path::new).collect();
        assert_eq!(outputs.len(), 2);
        assert_ne!(outputs[0], outputs[1], "each call has its own scratch file");
        for o in outputs {
            assert!(!o.parent().expect("scratch dir").exists(), "{} was cleaned up", o.display());
        }
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_command_that_exits_returns_its_output_while_a_child_holds_the_pipes() {
        // As adb does when it starts its server: the server keeps the pipes, adb exits.
        let started = Instant::now();
        let out = output_within(Command::new("sh").args(["-c", "sleep 3 & echo out; echo err >&2; exit 3"]), Duration::from_secs(10), "sh");
        assert_eq!(out, Ok((false, "out\n".to_owned(), "err\n".to_owned())));
        assert!(started.elapsed() < Duration::from_secs(2), "it waited {:?}", started.elapsed());
    }

    #[test]
    fn an_adb_call_past_its_deadline_is_killed_and_named() {
        let mut f = fixture("hang");
        f.adb.deadlines.query = Duration::from_millis(300);
        let started = Instant::now();
        let err = f.adb.terminate("com.example.hang").expect_err("wedged");
        assert!(started.elapsed() < Duration::from_secs(5), "it waited {:?}", started.elapsed());
        assert_eq!(err, format!("`adb -s {} shell am force-stop com.example.hang` timed out after 300ms; stopped it", f.adb.serial));
        let pid = std::fs::read_to_string(f.root.join("sdk/platform-tools/hang.pid")).expect("hang.pid");
        let alive = Command::new("kill").args(["-0", pid.trim()]).stderr(Stdio::null()).status().expect("kill -0");
        assert!(!alive.success(), "adb {} was killed", pid.trim());
        // The deadline is per call: the next one runs normally.
        assert_eq!(f.adb.terminate("com.example.app"), Ok(()));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn install_runs_adb_install_r_and_reports_failures() {
        let f = fixture("install");
        let apk = f.root.join("app.apk");
        std::fs::write(&apk, b"PK").expect("apk");
        assert_eq!(f.adb.install(&apk), Ok(()));
        assert_eq!(f.calls.calls(), [format!("install -r {}", apk.display())]);
        let broken = f.root.join("broken.apk");
        std::fs::write(&broken, b"PK").expect("apk");
        let err = f.adb.install(&broken).expect_err("broken");
        assert!(err.contains("INSTALL_PARSE_FAILED_NOT_APK"), "{err}");
        assert!(f.adb.install(&f.root.join("absent.apk")).expect_err("absent").ends_with("isn't a file"));
        let txt = f.root.join("app.txt");
        std::fs::write(&txt, b"x").expect("txt");
        assert!(f.adb.install(&txt).expect_err("extension").ends_with("isn't an .apk or .aab"));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn an_aab_needs_bundletool() {
        let mut f = fixture("aab");
        let aab = f.root.join("app.aab");
        std::fs::write(&aab, b"PK").expect("aab");
        f.adb.bundletool = None;
        let err = f.adb.install(&aab).expect_err("no bundletool");
        assert!(err.starts_with("installing an .aab needs `bundletool`"), "{err}");
        assert!(f.calls.calls().is_empty(), "nothing ran");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn an_aab_is_built_and_installed_by_bundletool() {
        let mut f = fixture("bundletool");
        let aab = f.root.join("app.aab");
        std::fs::write(&aab, b"PK").expect("aab");
        let tool = f.root.join("bundletool");
        let log = f.root.join("bundletool.log");
        crate::testing::write_script(&tool, &format!("echo \"$@\" >> {}\n", log.display()));
        f.adb.bundletool = Some(tool);
        assert_eq!(f.adb.install(&aab), Ok(()));
        let log = std::fs::read_to_string(&log).expect("log");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].starts_with(&format!("build-apks --bundle={} --output=", aab.display())), "{log}");
        assert!(lines[0].contains("--connected-device --overwrite") && lines[0].contains("--device-id=emulator-"), "{log}");
        assert!(lines[1].starts_with("install-apks --apks=") && lines[1].contains("--adb="), "{log}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn uninstall_terminate_apps_and_open_url() {
        let f = fixture("rest");
        assert_eq!(f.adb.uninstall("com.example.app"), Ok(()));
        assert_eq!(f.adb.uninstall("com.missing.app"), Err("adb failed: Failure [DELETE_FAILED_INTERNAL_ERROR]".into()));
        assert_eq!(f.adb.terminate("com.example.app"), Ok(()));
        assert_eq!(f.adb.apps(false), Ok(vec!["com.example.app".to_owned(), "com.example.zeta".to_owned()]));
        assert_eq!(f.adb.apps(true).expect("all"), ["android", "com.android.settings", "com.example.app", "com.example.zeta"]);
        assert_eq!(f.adb.open_url("https://example.com/"), Ok(()));
        assert!(f.adb.open_url("nowhere://x").expect_err("no handler").starts_with("opening nowhere://x: Error: Activity not started"));
        assert!(f.adb.open_url("example.com").expect_err("no scheme").contains("isn't a URL"));
        assert_eq!(
            f.calls.calls(),
            [
                "shell pm uninstall com.example.app",
                "shell pm uninstall com.missing.app",
                "shell am force-stop com.example.app",
                "shell pm list packages -3",
                "shell pm list packages",
                "shell am start -a android.intent.action.VIEW -d 'https://example.com/'",
                "shell am start -a android.intent.action.VIEW -d 'nowhere://x'",
            ]
        );
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn a_device_that_isnt_running_or_a_host_without_adb_is_refused() {
        let f = fixture("refused");
        let mut bare = Android::new(emu_avd::Env { sdk_root: Some(f.root.join("empty-sdk")), avd_home: f.root.join("avd") });
        bare.running_dir = Some(f.root.join("running"));
        let d = bare.device("phone").expect("device");
        // With no adb in this SDK the PATH's is used; a host without one says so.
        if bare.adb().is_none() {
            assert!(d.adb(&bare).expect_err("no adb").starts_with("adb not found"));
        }
        let idle = Android { running_dir: Some(f.root.join("nothing-running")), ..bare };
        assert_eq!(idle.device("phone").expect("device").adb(&idle).expect_err("not running"), "phone isn't running; boot it first");
        std::fs::remove_dir_all(&f.root).ok();
    }
}
