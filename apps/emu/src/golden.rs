//! The golden user data behind `-create-avd` (#226; the store and its rules are
//! `emu_avd::golden`): boots the first AVD of an image once, to the launcher, and stops it
//! through emu's console, so its settled user data can be kept and cloned.
//!
//! The boot is a child `emu -avd NAME` (one emulator process per guest): its stdout says which
//! console port it took and when `boot completed`; adb, when the SDK has it, says when the
//! launcher is up and the guest's CPUs are idle. Without adb the guest gets a fixed dwell after
//! boot completed. Nothing here needs a window, audio or a GPU beyond what `-avd` would use.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use emu_avd::golden::{Outcome, Source};
use emu_avd::{Avd, Env, Error};

use crate::args::Opts;

/// How long the build waits for `boot completed`: a first boot takes about 6 minutes in software
/// mode on 4 vCPUs (docs/research/software-mode-profile.md).
const BOOT_LIMIT: Duration = Duration::from_secs(30 * 60);
/// How long, after `boot completed`, the build waits for the launcher and an idle guest before it
/// settles for what it has.
const SETTLE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// What the guest gets after `boot completed` when there is no adb to ask (the post-boot jobs of
/// a fresh guest take 20 to 90 s, docs/farm-android.md).
const NO_ADB_DWELL: Duration = Duration::from_secs(120);
/// The guest counts as idle below this share of its CPU time busy, twice in a row.
const IDLE_BELOW: f64 = 0.25;
/// How long the guest gets to power itself off (`reboot -p`) before the console stops emu instead.
const POWEROFF_LIMIT: Duration = Duration::from_secs(120);
/// How long emu may take to exit after the console's `kill`.
const STOP_LIMIT: Duration = Duration::from_secs(60);

/// `-create-avd`'s second half: starts AVD `name` (just made) from its image's golden user data,
/// building the golden first when the image has none. Prints what it did; never fails the create:
/// the AVD exists either way, and without a golden it first-boots fresh. `-no-golden` and
/// `-dry-run` skip it.
pub fn start_from_golden(env: &Env, name: &str, opts: &Opts) {
    if opts.no_golden || opts.dry_run {
        return;
    }
    let avd = match env.load(name) {
        Ok(a) => a,
        // `create_avd` has already warned that the image isn't installed.
        Err(_) => return,
    };
    if let Err(why) = can_boot(&avd) {
        println!("golden: skipped, {why}");
        return;
    }
    match env.ensure_golden(name, |a| build(env, opts, a, &mut |m| println!("golden: {m}")), &mut |m| println!("golden: {m}")) {
        Ok(o) => println!("golden: {}", report(&o)),
        Err(e) => eprintln!("emu: warning: no golden for {name}: {e}; it first-boots fresh (-no-golden skips this step)"),
    }
}

/// The line that says how AVD `o`'s user data came about.
fn report(o: &Outcome) -> String {
    match o.source {
        Source::Cloned => format!(
            "started from the golden of this image ({}) in {:.1} s, {}{}; no first boot",
            o.key.name,
            o.took.as_secs_f64(),
            o.copied.label(),
            if o.waited.is_zero() { String::new() } else { format!(", after waiting {:.0} s for another build", o.waited.as_secs_f64()) }
        ),
        Source::Built => format!(
            "built in {:.0} s and kept for this image ({}); later AVDs of it start from a clone ({}). Guest identity is duplicated: {}",
            o.took.as_secs_f64(),
            o.key.name,
            o.copied.label(),
            emu_avd::golden::DUPLICATED_IDENTITY
        ),
    }
}

/// Whether this host can boot `avd` to settle it, or why not.
#[cfg(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64")))]
fn can_boot(avd: &Avd) -> Result<(), String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    if avd.is_x86_64() {
        return Err("this host boots arm64 images only (x86_64 needs software mode on x86_64 Linux)".into());
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if !avd.is_x86_64() {
            return Err("software mode boots x86_64 images only".into());
        }
        let (ok, what) = crate::software::accel_check();
        if !ok {
            return Err(format!("no way to boot it here: {what}"));
        }
    }
    match crate::instance::unsupported(avd).filter(|_| !crate::instance::allow_old_images()) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

#[cfg(not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))))]
fn can_boot(_: &Avd) -> Result<(), String> {
    Err("simee boots guests only on Apple silicon and x86_64 Linux".into())
}

/// Boots `avd` to the launcher in a child emu, waits for the guest to quiet down and stops it
/// through the console, leaving its settled disks in `<avd>/simee/`.
fn build(env: &Env, opts: &Opts, avd: &Avd, say: &mut dyn FnMut(&str)) -> emu_avd::Result<()> {
    let state = emu_avd::state_dir(&avd.dir);
    std::fs::create_dir_all(&state).map_err(|e| Error::Io(state.clone(), e))?;
    let log_path = state.join("golden-build.log");
    let log = std::fs::File::create(&log_path).map_err(|e| Error::Io(log_path.clone(), e))?;
    let exe = std::env::current_exe().map_err(|e| Error::Config(format!("finding emu's own path: {e}")))?;
    let mut cmd = Command::new(exe);
    cmd.args(["-avd", &avd.name, "-no-window", "-no-audio"]);
    if let Some(gpu) = &opts.gpu {
        cmd.args(["-gpu", gpu]);
    }
    cmd.env("ANDROID_AVD_HOME", &env.avd_home);
    if let Some(sdk) = &env.sdk_root {
        cmd.env("ANDROID_SDK_ROOT", sdk);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log.try_clone().map_err(|e| Error::Io(log_path.clone(), e))?)
        .spawn()
        .map_err(|e| Error::Config(format!("starting emu for {}: {e}", avd.name)))?;
    let started = Instant::now();
    let result = drive(&mut child, env, avd, log, &log_path, say);
    if result.is_err() {
        stop_child(&mut child);
    }
    result.inspect(|()| say(&format!("settled and stopped after {:.0} s", started.elapsed().as_secs_f64())))
}

/// What emu printed on stdout that the build acts on.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// `emu: NAME on emulator-PORT (adb port N)`
    Port(u16),
    /// `emu: boot completed in X s`
    Booted,
    End,
    Other,
}

fn classify(line: &str) -> Line {
    if line.starts_with("emu: boot completed") {
        return Line::Booted;
    }
    line.strip_prefix("emu: ")
        .and_then(|r| r.split_once(" on emulator-"))
        .and_then(|(_, r)| r.split_whitespace().next())
        .and_then(|p| p.parse().ok())
        .map_or(Line::Other, Line::Port)
}

fn drive(
    child: &mut Child,
    env: &Env,
    avd: &Avd,
    mut log: std::fs::File,
    log_path: &Path,
    say: &mut dyn FnMut(&str),
) -> emu_avd::Result<()> {
    let out = child.stdout.take().ok_or_else(|| Error::Config("emu has no stdout".into()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("golden-stdout".into())
        .spawn(move || {
            for line in BufReader::new(out).lines().map_while(std::io::Result::ok) {
                let _ = writeln!(log, "{line}");
                let kind = classify(&line);
                if tx.send(kind).is_err() {
                    return;
                }
            }
            let _ = tx.send(Line::End);
        })
        .map_err(|e| Error::Config(format!("reading emu's output: {e}")))?;
    let died = |child: &mut Child, what: &str| {
        let status = child.try_wait().ok().flatten().map(|s| s.to_string()).unwrap_or_else(|| "still running".into());
        Error::Config(format!("emu {what} ({status}); see {}", log_path.display()))
    };
    let (mut port, mut booted_at) = (None, None);
    let started = Instant::now();
    while booted_at.is_none() {
        if started.elapsed() > BOOT_LIMIT {
            return Err(Error::Config(format!(
                "{} didn't reach boot completed in {} s; see {}",
                avd.name,
                BOOT_LIMIT.as_secs(),
                log_path.display()
            )));
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Line::Port(p)) => {
                port = Some(p);
                say(&format!("{} is booting on emulator-{p}", avd.name));
            }
            Ok(Line::Booted) => booted_at = Some(Instant::now()),
            Ok(Line::End) | Err(mpsc::RecvTimeoutError::Disconnected) => return Err(died(child, "exited before boot completed")),
            Ok(Line::Other) | Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let booted_at = booted_at.unwrap_or_else(Instant::now);
    let port = port.ok_or_else(|| Error::Config("emu booted without saying which console port it took".into()))?;
    say(&format!("boot completed after {:.0} s; waiting for the launcher and an idle guest", started.elapsed().as_secs_f64()));
    let adb = env.sdk_root.as_ref().map(|r| r.join("platform-tools/adb")).filter(|p| p.is_file());
    match &adb {
        Some(adb) => settle(adb, port, booted_at, say).map_err(|why| Error::Config(format!("{why}; see {}", log_path.display())))?,
        None => {
            say(&format!("no adb in the SDK (emu -provision installs it): giving the guest {} s instead", NO_ADB_DWELL.as_secs()));
            std::thread::sleep(NO_ADB_DWELL);
        }
    }
    if child.try_wait().ok().flatten().is_some() {
        return Err(died(child, "exited while settling"));
    }
    if let Some(adb) = &adb {
        // The guest syncs, unmounts user data and powers off; emu exits when it does.
        let _ = adb_shell(adb, port, "reboot -p", Duration::from_secs(30));
        match wait_exit(child, POWEROFF_LIMIT)? {
            Some(status) if status.success() => {
                say("stopped by the guest's own power-off (adb shell reboot -p)");
                return Ok(());
            }
            Some(status) => {
                return Err(Error::Config(format!("emu exited with {status} after the guest's power-off; see {}", log_path.display())));
            }
            None => {
                say(&format!("the guest didn't power off within {} s; stopping through emu's console instead", POWEROFF_LIMIT.as_secs()))
            }
        }
    } else {
        say("stopping through emu's console (without adb the guest isn't shut down first)");
    }
    console_kill(port).map_err(|e| Error::Config(format!("stopping emu through its console on port {port}: {e}")))?;
    match wait_exit(child, STOP_LIMIT)? {
        Some(status) if status.success() => {
            say("stopped through emu's console (kill)");
            Ok(())
        }
        Some(status) => Err(Error::Config(format!("emu exited with {status} after the console stop; see {}", log_path.display()))),
        None => Err(died(child, "didn't exit after the console stop")),
    }
}

/// The child's exit status, None if it is still running after `limit`.
fn wait_exit(child: &mut Child, limit: Duration) -> emu_avd::Result<Option<std::process::ExitStatus>> {
    let until = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) if Instant::now() > until => return Ok(None),
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(Error::Config(format!("waiting for emu: {e}"))),
        }
    }
}

fn stop_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::INT);
    }
    // No SIGINT on Windows; a clean stop (a console control event) comes with its signals (#245).
    #[cfg(windows)]
    let _ = child.kill();
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Waits (up to [`SETTLE_LIMIT`] after `since`) for the home app to be the resumed activity and
/// the guest's CPUs to be idle in two samples in a row. Err when it never got there, or adb never
/// answered: a guest that didn't settle isn't a golden.
fn settle(adb: &Path, port: u16, since: Instant, say: &mut dyn FnMut(&str)) -> Result<(), String> {
    // /proc/stat before and after a pause, the home activity, then the resumed activity: one round trip.
    let probe = "head -1 /proc/stat; sleep 5; head -1 /proc/stat; \
         echo HOME=$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.HOME | tail -1); \
         dumpsys activity activities | grep -E 'topResumedActivity|mResumedActivity'";
    let (mut quiet, mut answered) = (0, false);
    while since.elapsed() < SETTLE_LIMIT {
        if let Some(out) = adb_shell(adb, port, probe, Duration::from_secs(180)) {
            answered = true;
            let (busy, launcher) = (busy_share(&out), launcher_resumed(&out));
            if launcher && busy.is_some_and(|b| b < IDLE_BELOW) {
                quiet += 1;
                if quiet >= 2 {
                    say(&format!("launcher resumed, guest {:.0}% busy", busy.unwrap_or(0.0) * 100.0));
                    return Ok(());
                }
            } else {
                quiet = 0;
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    Err(if answered {
        format!("the guest didn't settle (launcher resumed, CPUs idle) within {} s of boot completed", SETTLE_LIMIT.as_secs())
    } else {
        format!("adb never answered within {} s of boot completed", SETTLE_LIMIT.as_secs())
    })
}

/// `adb -s emulator-PORT shell CMD`'s stdout, None when it failed or took longer than `limit`.
fn adb_shell(adb: &Path, port: u16, cmd: &str, limit: Duration) -> Option<String> {
    let mut child = Command::new(adb)
        .args(["-s", &format!("emulator-{port}"), "shell", cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let until = Instant::now() + limit;
    let status = loop {
        match child.try_wait().ok()? {
            Some(s) => break s,
            None if Instant::now() > until => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let text = reader.join().ok()?;
    status.success().then_some(text)
}

/// The share of the guest's CPU time spent busy between the two `cpu` lines of `/proc/stat` in
/// `text` (user, nice, system, irq, softirq and steal over everything; idle and iowait are not
/// busy). None without two lines.
fn busy_share(text: &str) -> Option<f64> {
    let mut samples = text.lines().filter_map(|l| {
        let f: Vec<u64> = l.strip_prefix("cpu ")?.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        (f.len() >= 8).then(|| (f.iter().take(8).sum::<u64>(), f[3] + f[4]))
    });
    let (a, b) = (samples.next()?, samples.next()?);
    let total = b.0.checked_sub(a.0).filter(|t| *t > 0)?;
    let idle = b.1.saturating_sub(a.1);
    Some(1.0 - (idle.min(total) as f64 / total as f64))
}

/// Whether the resumed activity in the probe's output (`dumpsys activity activities`) is the home
/// app: the package of the `HOME=<package>/<activity>` line the probe prints (what
/// `cmd package resolve-activity` finds for the HOME category), or, when that is missing or says
/// the resolver activity, any resumed activity whose name has "launcher" in it.
fn launcher_resumed(text: &str) -> bool {
    let home = text
        .lines()
        .find_map(|l| l.strip_prefix("HOME="))
        .and_then(|c| c.trim().split_once('/'))
        .map(|(pkg, _)| pkg.trim())
        .filter(|pkg| !pkg.is_empty() && !pkg.contains(' ') && *pkg != "android");
    text.lines().filter(|l| l.contains("ResumedActivity")).any(|l| match home {
        Some(pkg) => l.contains(&format!("{pkg}/")),
        None => l.to_ascii_lowercase().contains("launcher"),
    })
}

/// `kill` on emu's console at `port`, the stock text protocol: the greeting, `auth <token>` from
/// `~/.emulator_console_auth_token`, then the command. emu stops the guest and exits.
pub fn console_kill(port: u16) -> std::io::Result<()> {
    let token = std::env::home_dir().map(|h| h.join(".emulator_console_auth_token"));
    console_kill_with(port, token.and_then(|p| std::fs::read_to_string(p).ok()).as_deref().map(str::trim))
}

pub(crate) fn console_kill_with(port: u16, token: Option<&str>) -> std::io::Result<()> {
    let mut s = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_secs(10))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(s.try_clone()?);
    let reply = |reader: &mut BufReader<TcpStream>| -> std::io::Result<String> {
        let mut last = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(last);
            }
            let line = line.trim_end().to_owned();
            if line.starts_with("OK") || line.starts_with("KO") {
                return Ok(line);
            }
            last = line;
        }
    };
    reply(&mut reader)?;
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        write!(s, "auth {t}\r\n")?;
        let r = reply(&mut reader)?;
        if r.starts_with("KO") {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, r));
        }
    }
    write!(s, "kill\r\n")?;
    let r = reply(&mut reader).unwrap_or_default();
    if r.starts_with("KO") {
        return Err(std::io::Error::other(r));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT_BUSY: &str = "cpu  1000 0 500 8000 100 0 0 0 0 0\ncpu  1900 0 900 8100 100 0 0 0 0 0\n";
    const STAT_IDLE: &str = "cpu  1000 0 500 8000 100 0 0 0 0 0\ncpu  1010 0 505 8985 100 0 0 0 0 0\n";

    #[test]
    fn emus_stdout_lines_are_classified() {
        assert_eq!(classify("emu: golden on emulator-5554 (adb port 5555)"), Line::Port(5554));
        assert_eq!(classify("emu: Pixel_9-API.36 on emulator-5560 (adb port 5561)"), Line::Port(5560));
        assert_eq!(classify("emu: boot completed in 361.9s"), Line::Booted);
        assert_eq!(classify("emu: stopped after 12.0s: Stop"), Line::Other);
        assert_eq!(classify("hello"), Line::Other);
    }

    #[test]
    fn the_guests_busy_share_comes_from_two_proc_stat_lines() {
        // 900 user + 400 system of 1400 ticks (idle 100): 93% busy.
        let busy = busy_share(STAT_BUSY).expect("two samples");
        assert!((busy - 1300.0 / 1400.0).abs() < 1e-9, "{busy}");
        let idle = busy_share(STAT_IDLE).expect("two samples");
        assert!(idle < IDLE_BELOW && idle > 0.0, "{idle}");
        assert_eq!(busy_share("cpu  1 2 3 4 5 6 7 8\n"), None, "one sample isn't a rate");
        assert_eq!(busy_share("cpu  1 2 3 4 5 6 7 8\ncpu  1 2 3 4 5 6 7 8\n"), None, "no time passed");
        assert_eq!(busy_share(""), None);
        // The probe's output has the activity lines after the samples.
        assert!(busy_share(&format!("{STAT_IDLE}  topResumedActivity=ActivityRecord{{1 u0 x/.Y t5}}\n")).is_some());
    }

    #[test]
    fn the_launcher_is_what_the_resumed_activity_says() {
        let launcher = "  topResumedActivity=ActivityRecord{ab u0 com.google.android.apps.nexuslauncher/.NexusLauncherActivity t4}\n";
        assert!(launcher_resumed(launcher));
        assert!(launcher_resumed("  mResumedActivity: ActivityRecord{ab u0 com.android.launcher3/.Launcher t2}\n"));
        assert!(!launcher_resumed("  topResumedActivity=ActivityRecord{ab u0 com.android.settings/.Settings t7}\n"));
        assert!(!launcher_resumed("  Launcher is mentioned here but nothing is resumed\n"));
        assert!(!launcher_resumed(""));
    }

    #[test]
    fn the_home_app_the_guest_names_is_the_one_that_must_be_resumed() {
        let resumed = "  topResumedActivity=ActivityRecord{ab u0 com.google.android.wearable.app/.HomeActivity t4}\n";
        // A home app with no "launcher" in its name counts once the guest says it is home...
        assert!(!launcher_resumed(resumed));
        assert!(launcher_resumed(&format!("HOME=com.google.android.wearable.app/.HomeActivity\n{resumed}")));
        // ...and another app resumed over it doesn't, even if its name says launcher.
        let other = "  topResumedActivity=ActivityRecord{ab u0 com.example.launcherish/.Main t9}\n";
        assert!(!launcher_resumed(&format!("HOME=com.google.android.wearable.app/.HomeActivity\n{other}")));
        // No answer or a chooser (several homes): the name decides.
        let nexus = "  topResumedActivity=ActivityRecord{ab u0 com.google.android.apps.nexuslauncher/.NexusLauncherActivity t4}\n";
        assert!(launcher_resumed(&format!("HOME=No activity found\n{nexus}")));
        assert!(launcher_resumed(&format!("HOME=android/com.android.internal.app.ResolverActivity\n{nexus}")));
    }

    #[test]
    fn the_report_says_how_the_avd_got_its_user_data() {
        let key = emu_avd::golden::GoldenKey {
            name: "36_default_x86_64_ab".into(),
            package: String::new(),
            image: String::new(),
            shape: String::new(),
        };
        let mut o = Outcome {
            key,
            source: Source::Cloned,
            took: Duration::from_millis(1234),
            copied: emu_avd::golden::CloneKind::Reflink,
            waited: Duration::ZERO,
        };
        let r = report(&o);
        assert!(r.contains("started from the golden") && r.contains("1.2 s") && r.contains("reflink") && !r.contains("waiting"), "{r}");
        o.waited = Duration::from_secs(300);
        assert!(report(&o).contains("after waiting 300 s for another build"));
        o.source = Source::Built;
        let r = report(&o);
        assert!(r.contains("built in") && r.contains("duplicated"), "{r}");
    }
}
