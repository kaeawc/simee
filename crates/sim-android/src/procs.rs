//! A running device's processes, over adb: list them (`processes`), signal one (`kill`) and run a
//! program with an argv (`exec`), in the shapes the iOS backend returns.

use std::time::Duration;

use crate::apps::Adb;

/// One process running inside the device, as iOS's `processes` rows are, plus its app package.
#[derive(Clone, Debug, PartialEq)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    /// toybox `ps`'s `%CPU`: CPU time over the process's life, percent of one CPU.
    pub cpu: f64,
    /// Resident memory in KiB.
    pub rss_kb: u64,
    /// Time since it started (`[[dd-]hh:]mm:ss`).
    pub elapsed: String,
    pub name: String,
    /// argv[0] (an app process's is its process name).
    pub path: String,
    /// The installed package whose process this is (`com.example.app` for `com.example.app` and
    /// `com.example.app:remote`), `None` for a native process.
    pub package: Option<String>,
}

/// What `exec` ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Executed {
    pub pid: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The program's exit status; `None` when it was stopped at the output cap.
    pub exit_code: Option<i32>,
    /// Whether stdout or stderr reached the cap and was cut there: the program was stopped if it
    /// still ran (`exit_code` `None`), and keeps its status if it had exited.
    pub truncated: bool,
}

/// How long `exec` lets a program run and how much of each stream it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecLimits {
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Default for ExecLimits {
    fn default() -> Self {
        ExecLimits { timeout: Duration::from_secs(30), max_bytes: 8 << 20 }
    }
}

/// The guest's process table: toybox `ps`, every process, the columns iOS's rows have (CMDLINE
/// last, as the only one that can hold spaces).
const PS: &str = "ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE";

/// The line between `ps`'s rows and `pm list packages`'s in `processes`' one adb call.
const PACKAGES_MARK: &str = "@packages";

/// What the guest's shell writes to stderr, with its pid and start time (`simee-exec-pid:<pid>:
/// <start>`), before it `exec`s the program (which keeps both).
const PID_MARK: &str = "simee-exec-pid:";

/// The most stderr bytes [`PID_MARK`]'s line can take: the mark, a pid, and a start time (a count
/// of clock ticks; `ps`'s date where there is no /proc).
const PID_LINE_MAX: usize = 96;

/// Processes the device can't run without, by `ps` name: signalling one reboots the framework or
/// the guest (init restarts zygote, and the framework with it, when one of them dies; a `critical`
/// one that dies again reboots the device), or cuts simee off from it (adbd).
const ESSENTIAL: &[&str] = &[
    "init",
    "zygote",
    "zygote64",
    "system_server",
    "surfaceflinger",
    "servicemanager",
    "hwservicemanager",
    "vndservicemanager",
    "vold",
    "lmkd",
    "logd",
    "adbd",
    "netd",
    "installd",
    "ueventd",
    "keystore2",
    "statsd",
];

fn essential(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    ESSENTIAL.contains(&base)
}

impl Adb {
    /// The device's processes, sorted as iOS's are (CPU, then memory, descending), each app
    /// process with its package: one adb call, `ps` and `pm list packages` together.
    pub fn processes(&self) -> Result<Vec<Process>, String> {
        let command = format!("{PS}; echo {PACKAGES_MARK}; pm list packages");
        let (_, out) = self.run(&["shell", &command], self.deadlines.query)?;
        let (ps, packages) = split_mark(&out);
        let rows = parse_ps(ps, &crate::apps::parse_packages(packages));
        if rows.is_empty() {
            return Err(crate::apps::failure(PS, &out));
        }
        Ok(rows)
    }

    /// Sends `signal` (0 to 64) to `pid`, one of the device's processes, as the shell user, or as
    /// root (`su 0`, which google_apis images have) when the shell user may not. A process the
    /// device can't run without ([`ESSENTIAL`]) is refused unless `force`, before either.
    pub fn kill(&self, pid: i64, signal: i64, force: bool) -> Result<(), String> {
        let signal = u8::try_from(signal).ok().filter(|s| *s <= 64).ok_or_else(|| format!("kill {pid} failed: bad signal {signal}"))?;
        let rows = parse_ps(&self.shell(PS, self.deadlines.query)?, &[]);
        let Some(row) = rows.iter().find(|p| i64::from(p.pid) == pid) else {
            return Err(format!("pid {pid} is not a process of this device"));
        };
        if essential(&row.name) && !force {
            return Err(format!("pid {pid} is {}, which the device can't run without; pass force to signal it anyway", row.name));
        }
        let what = format!("kill {pid}");
        let command = format!("kill -{signal} {pid}");
        match self.run(&["shell", &command], self.deadlines.query)? {
            (true, _) => Ok(()),
            (false, text) if text.contains("not permitted") => {
                match self.run(&["shell", &format!("su 0 {command}")], self.deadlines.query)? {
                    (true, _) => Ok(()),
                    (false, root) => Err(format!("{}; as root: {}", crate::apps::failure(&what, &text), crate::apps::failure("su", &root))),
                }
            }
            (false, text) => Err(crate::apps::failure(&what, &text)),
        }
    }

    /// Runs `program` with `args` (and `env` added to its environment) inside the device as the
    /// shell user: stdout, stderr, its exit status and its pid. The argv reaches the program as
    /// given: adbd hands the line to the guest's shell, so each word is quoted for it, and the
    /// shell `exec`s the program, in a process group of its own. Past `limits.timeout` it is
    /// stopped and the call fails; a stream that reaches `limits.max_bytes` is cut there, and the
    /// program stopped if it still runs (`truncated`). Stopping it stops its group, so what it
    /// started too, and only once the guest confirms its pid is still the program's.
    pub fn exec(&self, program: &str, args: &[String], env: &[(String, String)], limits: ExecLimits) -> Result<Executed, String> {
        let line = exec_line(program, args, env)?;
        let what = format!("adb -s {} shell {program}", self.serial);
        let mut command = std::process::Command::new(&self.adb);
        command.args(["-s", &self.serial, "shell", &line]);
        // stderr's cap leaves room for the pid line, which isn't the program's; what is past
        // `max_bytes` once it is taken off is cut below.
        let caps = [limits.max_bytes, limits.max_bytes.saturating_add(PID_LINE_MAX)];
        let ran = crate::apps::capture(&mut command, limits.timeout, caps, &what)?;
        let stdout = String::from_utf8_lossy(&ran.stdout).into_owned();
        let (started, mut stderr) = take_pid(&ran.stderr);
        let truncated = ran.truncated || stderr.len() > limits.max_bytes;
        stderr.truncate(limits.max_bytes);
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let Some((pid, start)) = started else {
            if ran.timed_out {
                return Err(format!(
                    "`{program}` timed out after {:?} before the device said it had started it; it may still be running",
                    limits.timeout
                ));
            }
            // The shell never ran the line: adb itself failed (an offline device, no adbd).
            return Err(crate::apps::failure("adb", &format!("{stdout}{stderr}")));
        };
        if ran.stopped {
            // adb is stopped; the program, and what it started, may still run in the guest.
            let _ = self.run(&["shell", &stop_line(pid, &start)], self.deadlines.query);
        }
        if ran.timed_out {
            return Err(format!("`{program}` timed out after {:?}; stopped it", limits.timeout));
        }
        Ok(Executed { pid: Some(pid), stdout, stderr, exit_code: ran.code, truncated })
    }
}

/// The guest's `simee_start <pid>`: the process's start time, which a process that reuses its pid
/// doesn't share. /proc's (field 22 of `stat`, counted after the command name, which can hold
/// spaces and parentheses), or `ps`'s on a host with no /proc; empty when there is no such process.
const START_FN: &str = r#"simee_start() { if [ -r /proc/$1/stat ]; then read -r s </proc/$1/stat; s=${s##*\)}; set -- $s; echo "${20}"; else echo $(ps -o lstart= -p $1); fi; }"#;

/// The guest shell line that runs `program` with exactly `args` and `env`, in a new process group
/// whose leader it is: `setsid -w` (which waits for it and exits with its status) where the guest
/// has one, else as a job of a shell with job control (`set -m`), waited for (that shell's own
/// stderr, where it reports its jobs, goes nowhere; the job's is the line's). Its shell first
/// writes [`PID_MARK`]'s line to stderr, then `exec`s `[env K=V...] program args...`, every word
/// quoted.
fn exec_line(program: &str, args: &[String], env: &[(String, String)]) -> Result<String, String> {
    use crate::apps::shell_quote;
    if program.is_empty() {
        return Err("`program` is empty".into());
    }
    if program.starts_with('-') {
        // `exec` would read it as an option (`exec -a NAME` names argv[0], `exec -c` clears env).
        return Err(format!("`{program}` can't be run: a program can't start with `-`"));
    }
    let mut argv = Vec::new();
    if !env.is_empty() {
        argv.push("env".to_owned());
    }
    for (key, value) in env {
        let name =
            key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name {
            return Err(format!("`{key}` isn't an environment variable name"));
        }
        argv.push(format!("{key}={value}"));
    }
    argv.push(program.to_owned());
    argv.extend(args.iter().cloned());
    let words: Vec<String> = argv.iter().map(|w| shell_quote(w)).collect();
    let words = words.join(" ");
    let run = shell_quote(&format!("{START_FN}; echo {PID_MARK}$$:\"$(simee_start $$)\" >&2; exec \"$@\""));
    Ok(format!(
        "r={run}; set -- {words}; if setsid -w true 2>/dev/null; then exec setsid -w sh -c \"$r\" sh \"$@\"; \
         else exec 3>&2 2>/dev/null; set -m; sh -c \"$r\" sh \"$@\" 2>&3 3>&- & wait $!; fi"
    ))
}

/// The guest shell line that stops what `exec` started as `pid` at `start`: its process group,
/// unless `pid` is now another process's (one started later). A group whose leader is gone keeps
/// its id while any process of it runs, so it is still the program's. A guest whose shell gave the
/// program no group of its own gets the program alone.
fn stop_line(pid: i32, start: &str) -> String {
    let start = crate::apps::shell_quote(start);
    format!(
        "{START_FN}; t=$(simee_start {pid}); case $t in ''|{start}) kill -9 -{pid} 2>/dev/null || {{ [ -n \"$t\" ] && kill -9 {pid}; }};; esac"
    )
}

/// The pid and start time [`PID_MARK`] gave on stderr's first line, and stderr without that line.
fn take_pid(stderr: &[u8]) -> (Option<(i32, String)>, Vec<u8>) {
    let end = stderr.iter().position(|b| *b == b'\n').unwrap_or(stderr.len());
    let started = std::str::from_utf8(&stderr[..end]).ok().and_then(|l| {
        let (pid, start) = l.trim().strip_prefix(PID_MARK)?.split_once(':')?;
        Some((pid.parse().ok()?, start.to_owned()))
    });
    match started {
        Some(started) => (Some(started), stderr.get(end + 1..).unwrap_or_default().to_vec()),
        None => (None, stderr.to_vec()),
    }
}

/// `ps`'s part and `pm list packages`' part of `processes`' output.
fn split_mark(out: &str) -> (&str, &str) {
    let mark = format!("\n{PACKAGES_MARK}\n");
    match out.find(&mark) {
        Some(at) => (&out[..at], &out[at + mark.len()..]),
        None => (out, ""),
    }
}

/// The rows of toybox `ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE` (the header and anything
/// else that isn't a row are skipped), each app process matched to a package of `installed`: its
/// process name, or the part before the `:` of a named process (`com.example.app:remote`).
fn parse_ps(out: &str, installed: &[String]) -> Vec<Process> {
    let mut rows: Vec<Process> = out
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid = cols.next()?.parse().ok()?;
            let ppid = cols.next()?.parse().ok()?;
            let cpu = cols.next()?.parse().ok()?;
            let rss_kb = cols.next()?.parse().ok()?;
            let elapsed = cols.next()?.to_owned();
            let (name, path) = name_and_path(&cols.collect::<Vec<_>>())?;
            let base = name.split(':').next().unwrap_or(&name);
            let package = installed.iter().find(|p| *p == base).cloned();
            Some(Process { pid, ppid, cpu, rss_kb, elapsed, name, path, package })
        })
        .collect();
    rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.rss_kb.cmp(&a.rss_kb)).then(a.pid.cmp(&b.pid)));
    rows
}

/// NAME and argv[0] from a `ps` row's words after ETIME. NAME is argv[0]'s base name and CMDLINE
/// starts with argv[0], so NAME is the fewest words that the next as many words then are (whole,
/// or as a path's base name). With no such split NAME is one word: a kernel thread's or a zombie's,
/// which have no cmdline, or one whose argv[0] isn't its name.
fn name_and_path(words: &[&str]) -> Option<(String, String)> {
    let first = *words.first()?;
    for k in 1..=words.len() / 2 {
        let (name, argv0) = (words[..k].join(" "), words[k..2 * k].join(" "));
        if argv0.rsplit('/').next() == Some(name.as_str()) {
            return Some((name, argv0));
        }
    }
    Some((first.to_owned(), words.get(1).unwrap_or(&first).to_string()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::*;
    use crate::Android;
    use crate::testing::{FakeAdb, FakeEmulator, fake_adb, fake_emulator};

    const PS: &str = "  PID  PPID %CPU    RSS ELAPSED NAME                    CMDLINE
    1     0  0.0  11304   15:02 init                    /system/bin/init second_stage
  520     1  0.4  20480 1-02:14:58 surfaceflinger      /system/bin/surfaceflinger --flag
  900   310  1.5 180224   14:40 com.android.systemui    com.android.systemui
 4321   310  7.9 120332   00:42 com.example.app         com.example.app
 4400   310  7.9  60112   00:40 com.example.app:remote  com.example.app:remote
 4500   310  0.0  50000   00:30 com.google.android.gms.persistent com.google.android.gms.persistent
    2     0  0.0      0   15:02 [kthreadd]
garbage line
";

    #[test]
    fn ps_rows_parse_and_app_processes_map_to_their_packages() {
        let installed = ["android".to_owned(), "com.android.systemui".to_owned(), "com.example.app".to_owned()];
        let rows = parse_ps(PS, &installed);
        let pids: Vec<i32> = rows.iter().map(|p| p.pid).collect();
        // CPU, then memory, descending, then pid: as iOS sorts them.
        assert_eq!(pids, [4321, 4400, 900, 520, 4500, 1, 2]);
        assert_eq!(
            rows[0],
            Process {
                pid: 4321,
                ppid: 310,
                cpu: 7.9,
                rss_kb: 120_332,
                elapsed: "00:42".into(),
                name: "com.example.app".into(),
                path: "com.example.app".into(),
                package: Some("com.example.app".into()),
            }
        );
        assert_eq!(rows[1].package.as_deref(), Some("com.example.app"), "a :remote process is its app's");
        assert_eq!(rows[2].package.as_deref(), Some("com.android.systemui"));
        let sf = &rows[3];
        assert_eq!(
            (sf.name.as_str(), sf.path.as_str(), sf.elapsed.as_str()),
            ("surfaceflinger", "/system/bin/surfaceflinger", "1-02:14:58")
        );
        assert_eq!(sf.package, None, "a native process has no package");
        assert_eq!(rows[4].package, None, "a process named for no installed package");
        assert_eq!((rows[6].name.as_str(), rows[6].path.as_str()), ("[kthreadd]", "[kthreadd]"), "a kernel thread has no cmdline");
    }

    #[test]
    fn ps_rows_with_spaces_in_their_name_zombies_and_no_packages_parse() {
        let ps = "  PID  PPID %CPU    RSS ELAPSED NAME                    CMDLINE
  950   310  0.2  40000   10:00 my app worker           my app worker --x
  951   310  0.1  30000   10:00 com.example.app         com.example.app
  777   900  0.0      0   00:05 sh
  520     1  0.4  20480 1-02:14:58 surfaceflinger      /system/bin/surfaceflinger --flag
";
        let rows = parse_ps(ps, &[]);
        let names: Vec<(&str, &str)> = rows.iter().map(|p| (p.name.as_str(), p.path.as_str())).collect();
        assert_eq!(
            names,
            [
                ("surfaceflinger", "/system/bin/surfaceflinger"),
                ("my app worker", "my app worker"),
                ("com.example.app", "com.example.app"),
                ("sh", "sh"),
            ],
            "a NAME with spaces is the whole name, and a zombie (no cmdline) is its name"
        );
        assert!(rows.iter().all(|p| p.package.is_none()), "with no packages installed no process is an app's");
    }

    struct Fixture {
        root: PathBuf,
        adb: Adb,
        calls: FakeAdb,
        _fake: FakeEmulator,
    }

    /// A running AVD whose adb is the fake.
    fn fixture(tag: &str) -> Fixture {
        let root = PathBuf::from("/tmp").join(format!("sa-procs-{tag}-{}", std::process::id()));
        let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        let mut android = Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        android.create("phone", "system-images;android-36;google_apis;arm64-v8a", None).expect("create");
        let calls = fake_adb(&android);
        let _fake = fake_emulator(&android, "phone");
        let adb = android.device("phone").expect("device").adb(&android).expect("adb");
        Fixture { root, adb, calls, _fake }
    }

    #[test]
    fn processes_lists_the_guests_ps_with_packages_in_one_adb_call() {
        let f = fixture("list");
        let rows = f.adb.processes().expect("processes");
        assert_eq!(rows.len(), 23);
        assert_eq!(rows[0].pid, 4321);
        assert_eq!(rows.iter().find(|p| p.pid == 900).and_then(|p| p.package.as_deref()), Some("com.android.systemui"));
        assert_eq!(f.calls.calls(), ["shell ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE; echo @packages; pm list packages"]);
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn kill_signals_a_process_of_the_device() {
        let f = fixture("kill");
        assert_eq!(f.adb.kill(4321, 15, false), Ok(()));
        assert_eq!(f.adb.kill(4400, 9, false), Ok(()));
        assert_eq!(
            f.calls.calls(),
            [
                "shell ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE",
                "shell kill -15 4321",
                "shell ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE",
                "shell kill -9 4400",
            ]
        );
        assert_eq!(f.adb.kill(77, 15, false), Err("pid 77 is not a process of this device".into()));
        assert_eq!(f.adb.kill(4321, 99, false), Err("kill 4321 failed: bad signal 99".into()));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn kill_retries_as_root_when_the_shell_user_may_not() {
        let f = fixture("root");
        assert_eq!(f.adb.kill(530, 9, false), Ok(()));
        assert_eq!(f.calls.calls()[1..], ["shell kill -9 530", "shell su 0 kill -9 530"]);
        // An image without su (google_apis_playstore) says what failed.
        let err = f.adb.kill(530, 15, false).expect_err("no su");
        assert!(err.starts_with("kill 530 failed: ") && err.contains("Operation not permitted") && err.contains("su"), "{err}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    /// The fake's processes the device can't run without, as `ps` names them.
    const ESSENTIAL_ROWS: [(i64, &str); 17] = [
        (1, "init"),
        (310, "zygote64"),
        (311, "zygote"),
        (612, "system_server"),
        (520, "surfaceflinger"),
        (200, "servicemanager"),
        (201, "hwservicemanager"),
        (202, "vndservicemanager"),
        (203, "vold"),
        (204, "lmkd"),
        (205, "logd"),
        (206, "adbd"),
        (207, "netd"),
        (208, "installd"),
        (209, "ueventd"),
        (210, "keystore2"),
        (211, "statsd"),
    ];

    #[test]
    fn kill_refuses_every_process_the_device_cant_run_without_unless_forced() {
        let f = fixture("protected");
        for (pid, name) in ESSENTIAL_ROWS {
            for signal in [9, 15] {
                let err = f.adb.kill(pid, signal, false).expect_err(name);
                assert_eq!(err, format!("pid {pid} is {name}, which the device can't run without; pass force to signal it anyway"));
            }
        }
        assert!(f.calls.calls().iter().all(|c| c.starts_with("shell ps ")), "nothing was signalled: {:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn kill_refuses_an_essential_process_the_shell_user_could_signal() {
        let f = fixture("protectedshell");
        // adbd runs as the shell user, so its signal would go through without su.
        assert!(f.adb.kill(206, 9, false).is_err());
        assert_eq!(f.calls.calls(), ["shell ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE"]);
        assert_eq!(f.adb.kill(206, 9, true), Ok(()));
        assert_eq!(f.calls.calls()[2..], ["shell kill -9 206"]);
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn kill_refuses_an_essential_process_before_the_root_retry() {
        let f = fixture("protectedroot");
        // surfaceflinger isn't the shell user's: only `su 0` could signal it.
        assert!(f.adb.kill(520, 9, false).is_err());
        assert_eq!(f.calls.calls(), ["shell ps -A -o PID,PPID,%CPU,RSS,ETIME,NAME,CMDLINE"], "neither as the shell user nor as root");
        assert_eq!(f.adb.kill(520, 9, true), Ok(()));
        assert_eq!(f.calls.calls()[2..], ["shell kill -9 520", "shell su 0 kill -9 520"]);
        std::fs::remove_dir_all(&f.root).ok();
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn exec_passes_the_argv_through_unchanged_and_returns_both_streams_and_the_status() {
        let f = fixture("exec");
        let args = strings(&[
            "-c",
            "printf '%s|' \"$@\"; echo err >&2; exit 3",
            "sh",
            "a b",
            "it's",
            "$(id)",
            "`id`",
            ";",
            "*",
            "",
            "two\nlines",
            "-n",
            "--",
        ]);
        let out = f.adb.exec("sh", &args, &[], ExecLimits::default()).expect("exec");
        assert_eq!(out.stdout, "a b|it's|$(id)|`id`|;|*||two\nlines|-n|--|");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.exit_code, Some(3));
        assert!(out.pid.is_some_and(|p| p > 0), "{out:?}");
        assert!(!out.truncated);
        let env = [("SIMEE_X".to_owned(), "b c".to_owned())];
        let out = f.adb.exec("sh", &strings(&["-c", "echo \"$SIMEE_X\""]), &env, ExecLimits::default()).expect("env");
        assert_eq!((out.stdout.as_str(), out.exit_code), ("b c\n", Some(0)));
        assert_eq!(
            f.adb.exec("sh", &[], &[("A;B".to_owned(), "x".to_owned())], ExecLimits::default()).map(|_| ()),
            Err("`A;B` isn't an environment variable name".into())
        );
        assert_eq!(f.adb.exec("", &[], &[], ExecLimits::default()).map(|_| ()), Err("`program` is empty".into()));
        // (An argument's newline splits its call over two of the log's lines.)
        assert_eq!(f.calls.calls().iter().filter(|c| c.starts_with("shell ")).count(), 2, "refused calls never reached adb");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_refuses_a_program_the_shell_would_read_as_an_option() {
        let f = fixture("execdash");
        // `exec -a x id` would run id, and `exec -c` nothing at all.
        for program in ["-a", "-c", "--"] {
            let err = f.adb.exec(program, &strings(&["x", "id"]), &[], ExecLimits::default()).map(|_| ());
            assert_eq!(err, Err(format!("`{program}` can't be run: a program can't start with `-`")));
        }
        assert_eq!(f.calls.calls(), Vec::<String>::new());
        std::fs::remove_dir_all(&f.root).ok();
    }

    /// Whether `pid` is gone (or a zombie, which has stopped), within 5 s; one that isn't is
    /// stopped here, so a failing test leaves nothing running.
    fn gone(pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ps = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().expect("ps");
            let stat = String::from_utf8_lossy(&ps.stdout).trim().to_owned();
            if stat.is_empty() || stat.starts_with('Z') {
                return true;
            }
            if Instant::now() >= deadline {
                let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The pid a program wrote to `file`.
    fn written_pid(file: &Path) -> i32 {
        std::fs::read_to_string(file).expect("pid file").trim().parse().expect("pid")
    }

    #[test]
    fn exec_stops_a_program_past_its_timeout() {
        let f = fixture("exectimeout");
        let file = f.root.join("program.pid");
        let limits = ExecLimits { timeout: Duration::from_millis(400), ..ExecLimits::default() };
        let script = format!("echo $$ > '{}'; exec sleep 30", file.display());
        let err = f.adb.exec("sh", &strings(&["-c", &script]), &[], limits).expect_err("timeout");
        assert!(err.starts_with("`sh` timed out after 400ms; stopped it"), "{err}");
        let pid = written_pid(&file);
        let calls = f.calls.calls();
        assert!(calls.last().is_some_and(|c| c.contains(&format!("simee_start {pid})"))), "the stop names the program's pid: {calls:?}");
        assert!(gone(pid), "the program still runs in the guest");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_at_its_cap_stops_the_program_and_every_process_it_started() {
        let f = fixture("execcap");
        let file = f.root.join("child.pid");
        let limits = ExecLimits { max_bytes: 1000, ..ExecLimits::default() };
        let script = format!("sleep 30 & echo $! > '{}'; yes", file.display());
        let out = f.adb.exec("sh", &strings(&["-c", &script]), &[], limits).expect("capped");
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.stdout.starts_with("y\ny\n"));
        assert!(out.truncated);
        assert_eq!(out.exit_code, None);
        let pid = out.pid.expect("pid");
        let calls = f.calls.calls();
        assert!(calls.last().is_some_and(|c| c.contains(&format!("simee_start {pid})"))), "the stop names the program's pid: {calls:?}");
        let child = written_pid(&file);
        assert!(gone(child), "the program's child {child} survived");
        assert!(gone(pid), "the program survived");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_flags_a_stream_past_its_cap_that_ended_under_the_pid_lines_room() {
        let f = fixture("execslack");
        let limits = ExecLimits { max_bytes: 1000, ..ExecLimits::default() };
        let out = f.adb.exec("sh", &strings(&["-c", "printf %1003s x >&2"]), &[], limits).expect("exec");
        assert_eq!(out.stderr.len(), 1000);
        assert!(out.truncated, "3 bytes past the cap were dropped silently");
        // The program had exited by itself: its status stands, and nothing is signalled.
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(f.calls.calls().len(), 1, "{:?}", f.calls.calls());
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_past_its_timeout_before_the_program_started_says_so() {
        let f = fixture("execslow");
        std::fs::write(f.root.join("sdk/platform-tools/exec.slow"), "").expect("slow");
        let limits = ExecLimits { timeout: Duration::from_millis(200), ..ExecLimits::default() };
        let err = f.adb.exec("true", &[], &[], limits).expect_err("timeout");
        assert!(err.starts_with("`true` timed out after 200ms before the device said it had started it"), "{err}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    /// `pid`'s start time as the guest's shell reads it, here run by the host's.
    fn host_start(pid: i32) -> String {
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(format!("{START_FN}; simee_start {pid}")).output().expect("sh");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[test]
    fn exec_stop_signals_only_the_process_it_started() {
        let f = fixture("execreuse");
        let mut other = std::process::Command::new("sleep").arg("30").process_group(0).spawn().expect("sleep");
        let pid = i32::try_from(other.id()).expect("pid");
        let start = host_start(pid);
        assert!(!start.is_empty(), "no start time for {pid}");
        // The program exited and another process now has its pid: a later start time.
        f.adb.run(&["shell", &stop_line(pid, "1")], Duration::from_secs(5)).expect("stop");
        assert!(other.try_wait().expect("try_wait").is_none(), "a process reusing the pid was signalled");
        f.adb.run(&["shell", &stop_line(pid, &start)], Duration::from_secs(5)).expect("stop");
        let killed = gone(pid);
        let status = other.wait().expect("wait");
        assert!(killed && status.signal() == Some(9), "the program itself is stopped: {status:?}");
        std::fs::remove_dir_all(&f.root).ok();
    }
}
