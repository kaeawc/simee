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
    /// Whether stdout or stderr reached the cap, at which the program was stopped.
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

/// What the guest's shell writes to stderr, with its pid, before `exec` replaces it with the
/// program (which keeps that pid).
const PID_MARK: &str = "simee-exec-pid:";

/// Processes the device can't run without: signalling one reboots the framework or the guest.
fn essential(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    base == "init" || base == "system_server" || base.starts_with("zygote")
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
    /// root (`su 0`, which google_apis images have) when the shell user may not. init, zygote and
    /// system_server are refused unless `force`.
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
    /// shell `exec`s the program. Past `limits.timeout` it is stopped and the call fails; a stream
    /// that reaches `limits.max_bytes` is cut there and the program stopped (`truncated`).
    pub fn exec(&self, program: &str, args: &[String], env: &[(String, String)], limits: ExecLimits) -> Result<Executed, String> {
        let line = exec_line(program, args, env)?;
        let what = format!("adb -s {} shell {program}", self.serial);
        let mut command = std::process::Command::new(&self.adb);
        command.args(["-s", &self.serial, "shell", &line]);
        // stderr's cap leaves room for the pid line, which isn't the program's.
        let caps = [limits.max_bytes, limits.max_bytes.saturating_add(PID_MARK.len() + 12)];
        let ran = crate::apps::capture(&mut command, limits.timeout, caps, &what)?;
        let stdout = String::from_utf8_lossy(&ran.stdout).into_owned();
        let (pid, mut stderr) = take_pid(&ran.stderr);
        stderr.truncate(limits.max_bytes);
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        let Some(pid) = pid else {
            // The shell never ran the line: adb itself failed (an offline device, no adbd).
            return Err(crate::apps::failure("adb", &format!("{stdout}{stderr}")));
        };
        if ran.timed_out || ran.truncated {
            // adb is stopped; the program may still run in the guest.
            let _ = self.run(&["shell", &format!("kill -9 {pid}")], self.deadlines.query);
        }
        if ran.timed_out {
            return Err(format!("`{program}` timed out after {:?}; stopped it", limits.timeout));
        }
        Ok(Executed { pid: Some(pid), stdout, stderr, exit_code: ran.code, truncated: ran.truncated })
    }
}

/// The guest shell line that runs `program` with exactly `args` and `env`: its pid on stderr,
/// then `exec [env K=V...] program args...`, every word quoted.
fn exec_line(program: &str, args: &[String], env: &[(String, String)]) -> Result<String, String> {
    use crate::apps::shell_quote;
    if program.is_empty() {
        return Err("`program` is empty".into());
    }
    let mut line = format!("echo {PID_MARK}$$ >&2; exec");
    if !env.is_empty() {
        line.push_str(" env");
    }
    for (key, value) in env {
        let name =
            key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name {
            return Err(format!("`{key}` isn't an environment variable name"));
        }
        line.push(' ');
        line.push_str(&shell_quote(&format!("{key}={value}")));
    }
    for word in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        line.push(' ');
        line.push_str(&shell_quote(word));
    }
    Ok(line)
}

/// The pid [`PID_MARK`] gave on stderr's first line, and stderr without that line.
fn take_pid(stderr: &[u8]) -> (Option<i32>, Vec<u8>) {
    let end = stderr.iter().position(|b| *b == b'\n').unwrap_or(stderr.len());
    let pid = std::str::from_utf8(&stderr[..end]).ok().and_then(|l| l.trim().strip_prefix(PID_MARK)?.parse().ok());
    match pid {
        Some(pid) => (Some(pid), stderr.get(end + 1..).unwrap_or_default().to_vec()),
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
            let name = cols.next()?.to_owned();
            let path = cols.next().unwrap_or(&name).to_owned();
            let base = name.split(':').next().unwrap_or(&name);
            let package = installed.iter().find(|p| *p == base).cloned();
            Some(Process { pid, ppid, cpu, rss_kb, elapsed, name, path, package })
        })
        .collect();
    rows.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.rss_kb.cmp(&a.rss_kb)).then(a.pid.cmp(&b.pid)));
    rows
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
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
        assert_eq!(rows.len(), 10);
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
        assert_eq!(f.adb.kill(520, 9, false), Ok(()));
        assert_eq!(f.calls.calls()[1..], ["shell kill -9 520", "shell su 0 kill -9 520"]);
        // An image without su (google_apis_playstore) says what failed.
        let err = f.adb.kill(520, 15, false).expect_err("no su");
        assert!(err.starts_with("kill 520 failed: ") && err.contains("Operation not permitted") && err.contains("su"), "{err}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn kill_refuses_init_zygote_and_system_server_without_force() {
        let f = fixture("protected");
        for (pid, name) in [(1, "init"), (310, "zygote64"), (311, "zygote"), (612, "system_server")] {
            let err = f.adb.kill(pid, 9, false).expect_err(name);
            assert_eq!(err, format!("pid {pid} is {name}, which the device can't run without; pass force to signal it anyway"));
        }
        assert!(f.calls.calls().iter().all(|c| c.starts_with("shell ps ")), "nothing was signalled: {:?}", f.calls.calls());
        assert_eq!(f.adb.kill(612, 9, true), Ok(()));
        assert_eq!(f.calls.calls().last().map(String::as_str), Some("shell kill -9 612"));
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_passes_the_argv_through_unchanged_and_returns_both_streams_and_the_status() {
        let f = fixture("exec");
        let args: Vec<String> = ["-c", "printf '%s|' \"$@\"; echo err >&2; exit 3", "sh", "a b", "it's", "$(id)", ";", "*"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let out = f.adb.exec("sh", &args, &[], ExecLimits::default()).expect("exec");
        assert_eq!(out.stdout, "a b|it's|$(id)|;|*|");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.exit_code, Some(3));
        assert!(out.pid.is_some_and(|p| p > 0), "{out:?}");
        assert!(!out.truncated);
        let env = [("SIMEE_X".to_owned(), "b c".to_owned())];
        let out = f.adb.exec("sh", &["-c".to_owned(), "echo \"$SIMEE_X\"".to_owned()], &env, ExecLimits::default()).expect("env");
        assert_eq!((out.stdout.as_str(), out.exit_code), ("b c\n", Some(0)));
        assert_eq!(
            f.adb.exec("sh", &[], &[("A;B".to_owned(), "x".to_owned())], ExecLimits::default()).map(|_| ()),
            Err("`A;B` isn't an environment variable name".into())
        );
        assert_eq!(f.adb.exec("", &[], &[], ExecLimits::default()).map(|_| ()), Err("`program` is empty".into()));
        assert_eq!(f.calls.calls().len(), 2, "refused calls never reached adb");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_stops_a_program_past_its_timeout() {
        let f = fixture("exectimeout");
        let started = Instant::now();
        let limits = ExecLimits { timeout: Duration::from_millis(400), ..ExecLimits::default() };
        let err = f.adb.exec("sleep", &["10".to_owned()], &[], limits).expect_err("timeout");
        assert!(started.elapsed() < Duration::from_secs(5), "it waited {:?}", started.elapsed());
        assert!(err.starts_with("`sleep` timed out after 400ms; stopped it"), "{err}");
        let calls = f.calls.calls();
        assert!(calls.last().is_some_and(|c| c.starts_with("shell kill -9 ")), "the guest's process is stopped too: {calls:?}");
        std::fs::remove_dir_all(&f.root).ok();
    }

    #[test]
    fn exec_caps_each_stream_and_stops_the_program() {
        let f = fixture("execcap");
        let started = Instant::now();
        let limits = ExecLimits { max_bytes: 1000, ..ExecLimits::default() };
        let out = f.adb.exec("yes", &[], &[], limits).expect("capped");
        assert!(started.elapsed() < Duration::from_secs(5), "it waited {:?}", started.elapsed());
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.stdout.starts_with("y\ny\n"));
        assert!(out.truncated);
        assert_eq!(out.exit_code, None);
        std::fs::remove_dir_all(&f.root).ok();
    }
}
