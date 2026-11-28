//! Shutting down on SIGINT and SIGTERM (#388): `simee serve`, `simee mcp`.
//!
//! [`watch`] puts `signal-hook`'s iterator on a thread of its own: the signal handler only writes
//! to signal-hook's self-pipe, and that thread does the rest. The first signal marks the process
//! shutting down, so [`crate::call_cancelled`] is true from then on and a call in flight stops at
//! its next safe point, and runs the server's own shutdown on another thread. A second signal
//! exits at once, removing the sockets the process serves on the way out. Either way the process
//! exits with 128 + the signal: 130 for SIGINT, 143 for SIGTERM.
//!
//! Nothing here shuts down a device the process didn't start, but a shutdown does reach devices
//! it did: a call cancelled at a safe point undoes what it made itself (a `create` or `clone` that
//! finishes during the drain is deleted; a `boot` waiting for admission boots nothing). And a
//! Ctrl-C at a terminal goes to the whole process group, so the Android emulators `serve` started
//! (its children) get the SIGINT too, and power off.

/// `eprintln!` without its panic on a closed stderr: a shutdown message must never stop the
/// shutdown it announces.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
pub(crate) use say;

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGINT, SIGTERM};

/// What a call that the shutdown cancelled, or never ran, answers (its own error follows, if any).
pub const SHUTTING_DOWN: &str = "cancelled: server shutting down";

/// The signal that started the shutdown; 0 while none has.
static SIGNAL: AtomicI32 = AtomicI32::new(0);

/// The sockets this process serves on (`serve`'s, the camera feeds'): removed however the process
/// exits, unless another server has replaced the file since.
static SOCKETS: Mutex<Vec<Bound>> = Mutex::new(Vec::new());

/// A socket file as this process bound it: its path, and the device and inode the path named
/// then. It is removed only while the path still names that same file, so a successor server's
/// socket at the path is left alone. Its owner keeps the listener open until the file is removed:
/// a socket still bound keeps its inode, so no new file can be given the same number meanwhile.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bound {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Bound {
    /// The file at `path` now.
    fn of(path: &Path) -> Option<Bound> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        Some(Bound { path: path.to_owned(), dev: meta.dev(), ino: meta.ino() })
    }

    /// Removes the file, if the path still names it.
    fn remove(&self) {
        if Bound::of(&self.path).as_ref() == Some(self) {
            std::fs::remove_file(&self.path).ok();
        }
    }
}

/// Calls running on the API server's connections; [`IDLE`] is told when one ends.
static IN_FLIGHT: Mutex<usize> = Mutex::new(0);
static IDLE: Condvar = Condvar::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The signal that started this process's shutdown, once one has.
pub fn signal() -> Option<i32> {
    match SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        sig => Some(sig),
    }
}

/// Whether this process is shutting down (a SIGINT or SIGTERM arrived).
pub fn shutting_down() -> bool {
    signal().is_some()
}

/// The exit code for a process a signal stopped: 128 + its number (130 SIGINT, 143 SIGTERM).
pub fn exit_code(sig: i32) -> i32 {
    128 + sig
}

/// The signal's name, for the lines a shutdown prints.
fn name(sig: i32) -> &'static str {
    match sig {
        SIGINT => "SIGINT",
        SIGTERM => "SIGTERM",
        _ => "signal",
    }
}

/// Watches SIGINT and SIGTERM from now on. The first marks the process shutting down and runs
/// `first` (the server's shutdown, which exits or ends the server) on a thread of its own, so
/// the watch goes on; a second exits at once, with [`exit_code`] of it, after removing the
/// sockets [`remove_on_exit`] names. Call it once per process.
pub fn watch(first: impl FnOnce(i32) + Send + 'static) -> std::io::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;
    std::thread::Builder::new().name("simee-signals".into()).spawn(move || {
        let mut first = Some(first);
        for sig in signals.forever() {
            let Some(shutdown) = first.take() else {
                say!("simee: a second {}: exiting now", name(sig));
                exit(sig);
            };
            SIGNAL.store(sig, Ordering::SeqCst);
            say!("simee: {}: shutting down (a second Ctrl-C or SIGTERM exits at once)", name(sig));
            if let Err(e) = std::thread::Builder::new().name("simee-shutdown".into()).spawn(move || shutdown(sig)) {
                say!("simee: cannot start the shutdown ({e}): exiting now");
                exit(sig);
            }
        }
    })?;
    Ok(())
}

/// Exits the process a signal stopped, with [`exit_code`] of it, removing its sockets first
/// (`simee mcp`'s shutdown, and a second signal's).
pub fn exit(sig: i32) -> ! {
    remove_sockets();
    std::process::exit(exit_code(sig))
}

/// Removes `path`, the socket this process just bound, when the process shuts down
/// ([`remove_sockets`]). The caller keeps its listener open until then.
pub(crate) fn remove_on_exit(path: &Path) {
    if let Some(bound) = Bound::of(path) {
        let mut sockets = lock(&SOCKETS);
        sockets.retain(|b| b.path != bound.path);
        sockets.push(bound);
    }
}

/// Forgets `path`: its socket was closed and removed by its owner (a camera feed that stopped).
pub(crate) fn forget(path: &Path) {
    lock(&SOCKETS).retain(|b| b.path != path);
}

/// Whether a shutdown would remove the socket at `path`.
#[cfg(test)]
pub(crate) fn removes(path: &Path) -> bool {
    lock(&SOCKETS).iter().any(|b| b.path == path)
}

/// Removes every socket [`remove_on_exit`] named that is still the file this process bound.
pub(crate) fn remove_sockets() {
    for bound in lock(&SOCKETS).drain(..) {
        bound.remove();
    }
}

/// A call running on a server connection: counted until it is dropped (after its reply is sent).
pub(crate) struct Call(());

impl Drop for Call {
    fn drop(&mut self) {
        let mut n = lock(&IN_FLIGHT);
        *n = n.saturating_sub(1);
        if *n == 0 {
            IDLE.notify_all();
        }
    }
}

/// Counts a call about to run; `None` once the process is shutting down: the call isn't run.
pub(crate) fn enter() -> Option<Call> {
    let mut n = lock(&IN_FLIGHT);
    if shutting_down() {
        return None;
    }
    *n += 1;
    Some(Call(()))
}

/// How many calls are running.
#[cfg(test)]
pub(crate) fn in_flight() -> usize {
    *lock(&IN_FLIGHT)
}

/// Waits up to `limit` for the calls running to end; whether they all did.
pub(crate) fn drain(limit: Duration) -> bool {
    let until = Instant::now() + limit;
    let mut n = lock(&IN_FLIGHT);
    while *n > 0 {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        n = IDLE.wait_timeout(n, left).unwrap_or_else(PoisonError::into_inner).0;
    }
    true
}

/// A call's outcome, saying so when the shutdown cancelled it: an error that ends a call once the
/// process is shutting down is the shutdown's.
pub(crate) fn outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Err(e) if shutting_down() => Err(format!("{SHUTTING_DOWN} ({e})")),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _};
    use std::path::Path;
    use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    /// The environment variable that makes [`server_child`] serve instead of passing at once.
    const CHILD: &str = "SIMEE_SHUTDOWN_CHILD";
    /// How long a test waits for the child to get anywhere: a deadline for a hang, never a
    /// measure of speed.
    const DEADLINE: Duration = Duration::from_secs(60);

    #[test]
    fn a_socket_is_removed_only_while_its_path_names_the_file_bound() {
        let dir = short_dir("bound");
        let path = dir.join("s.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let bound = Bound::of(&path).expect("stat");
        // The same inode number on another device, or another inode on this one: not this file.
        let other_dev = Bound { dev: bound.dev.wrapping_add(1), ..bound.clone() };
        let other_ino = Bound { ino: bound.ino.wrapping_add(1), ..bound.clone() };
        other_dev.remove();
        other_ino.remove();
        assert!(path.exists(), "a different file is left alone");
        bound.remove();
        assert!(!path.exists(), "the file bound is removed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_signal_exits_with_128_plus_its_number() {
        assert_eq!((exit_code(SIGINT), exit_code(SIGTERM)), (130, 143));
        assert_eq!((name(SIGINT), name(SIGTERM)), ("SIGINT", "SIGTERM"));
    }

    /// The child process's server (run only with [`CHILD`] set, by the tests below): `simee
    /// serve`'s shutdown on a session whose host holds every boot (a `boot` waits for admission
    /// until cancelled), with one stream running on fakes that write their WHIP DELETE to
    /// `<dir>/deletes` (or, `wedged`, never finish it). Says `ready` on stdout once it serves,
    /// and `in-flight` once a call runs.
    #[test]
    fn server_child() {
        let Some(dir) = std::env::var_os(CHILD).map(PathBuf::from) else { return };
        let wedged = std::env::var_os("SIMEE_SHUTDOWN_WEDGED").is_some();
        drop(crate::methods::tests::held_android_session(&dir));
        let stream = crate::stream::tests::start_global_fake_stream(&dir.join("deletes"), wedged);
        assert_eq!(stream, "stream-1");
        let root = dir.clone();
        let make = move || crate::methods::tests::held_android_session(&root);
        std::thread::spawn(|| {
            let until = Instant::now() + DEADLINE;
            while in_flight() == 0 && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            println!("in-flight");
        });
        let socket = dir.join("api.sock");
        let code = crate::serve_until_signal(&socket, std::sync::Arc::new(make), Duration::from_secs(30), || println!("ready"));
        std::process::exit(code.expect("serve"));
    }

    /// A short directory under `/tmp` (the socket path must fit `sockaddr_un`).
    fn short_dir(tag: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp").join(format!("sd-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    struct Server {
        child: Child,
        lines: Receiver<String>,
        stderr: Option<ChildStderr>,
    }

    impl Server {
        fn start(dir: &Path, wedged: bool) -> Server {
            let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
            cmd.args(["--exact", "shutdown::tests::server_child", "--nocapture", "--test-threads=1"])
                .env(CHILD, dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if wedged {
                cmd.env("SIMEE_SHUTDOWN_WEDGED", "1");
            }
            let mut child = cmd.spawn().expect("spawn the server child");
            let stdout = child.stdout.take().expect("stdout");
            let (tx, lines) = channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            });
            let stderr = child.stderr.take();
            Server { child, lines, stderr }
        }

        /// Waits for the child to print `want` on stdout (at the end of a line: the test harness
        /// starts the line with the test's name).
        fn expect_line(&self, want: &str) {
            let until = Instant::now() + DEADLINE;
            loop {
                let left = until.saturating_duration_since(Instant::now());
                match self.lines.recv_timeout(left) {
                    Ok(line) if line.trim_end().ends_with(want) => return,
                    Ok(_) => {}
                    Err(e) => panic!("no `{want}` from the child: {e}"),
                }
            }
        }

        fn signal(&self, sig: &str) {
            let ok = Command::new("kill").args([sig, &self.child.id().to_string()]).status().expect("kill").success();
            assert!(ok, "kill {sig}");
        }

        /// The child's exit status and its stderr, within the deadline (killed otherwise).
        fn exit(mut self) -> (ExitStatus, String) {
            let until = Instant::now() + DEADLINE;
            let status = loop {
                if let Some(status) = self.child.try_wait().expect("wait") {
                    break status;
                }
                if Instant::now() > until {
                    self.child.kill().ok();
                    panic!("the child didn't exit within {DEADLINE:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let mut err = String::new();
            if let Some(mut stderr) = self.stderr.take() {
                stderr.read_to_string(&mut err).ok();
            }
            (status, err)
        }
    }

    impl Drop for Server {
        /// A test that failed early leaves no server running.
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                self.child.kill().ok();
                self.child.wait().ok();
            }
        }
    }

    /// A call that waits for admission on the child's held host, sent on a connection of its
    /// own; its reply comes back on the receiver.
    fn boot_in_flight(socket: &Path) -> Receiver<Result<serde_json::Value, String>> {
        let mut client = crate::Client::connect(socket).expect("connect");
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(client.call("boot", &json!({"device": "pixel", "platform": "android", "admission": "wait"})));
        });
        rx
    }

    fn stopped_and_exited(sig: &str, code: i32) {
        let dir = short_dir(&sig[1..]);
        let socket = dir.join("api.sock");
        let server = Server::start(&dir, false);
        server.expect_line("ready");
        assert!(socket.exists(), "it serves on {}", socket.display());
        let streams = crate::call(&socket, "streams", json!({})).expect("streams");
        assert_eq!(streams.as_array().map(Vec::len), Some(1), "one stream runs: {streams}");
        let reply = boot_in_flight(&socket);
        server.expect_line("in-flight");
        server.signal(sig);
        let (status, err) = server.exit();
        assert_eq!(status.code(), Some(code), "{status}; stderr:\n{err}");
        let reply = reply.recv_timeout(DEADLINE).expect("a reply");
        let e = reply.expect_err("cancelled");
        assert!(e.starts_with("cancelled: server shutting down (admission: cancelled after waiting"), "{e}");
        assert!(e.ends_with("nothing was booted)"), "{e}");
        assert!(!socket.exists(), "the socket is removed");
        let deletes = std::fs::read_to_string(dir.join("deletes")).unwrap_or_default();
        assert_eq!(deletes, "DELETE http://127.0.0.1:8889/sim/whip/session-0\n", "the stream was stopped with its DELETE");
        assert!(err.contains("shutting down"), "{err}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sigint_cancels_calls_in_flight_stops_streams_removes_the_socket_and_exits_130() {
        stopped_and_exited("-INT", 130);
    }

    #[test]
    fn sigterm_cancels_calls_in_flight_stops_streams_removes_the_socket_and_exits_143() {
        stopped_and_exited("-TERM", 143);
    }

    #[test]
    fn the_first_signal_shuts_down_in_order_when_the_socket_file_is_gone() {
        let dir = short_dir("gone");
        let socket = dir.join("api.sock");
        let server = Server::start(&dir, false);
        server.expect_line("ready");
        // A tmp cleaner or an `rm`: nothing can connect to the server any more.
        std::fs::remove_file(&socket).expect("rm the socket");
        server.signal("-TERM");
        let (status, err) = server.exit();
        assert_eq!(status.code(), Some(143), "{status}; stderr:\n{err}");
        assert!(!err.contains("exiting now"), "one signal was enough: {err}");
        let deletes = std::fs::read_to_string(dir.join("deletes")).unwrap_or_default();
        assert_eq!(deletes, "DELETE http://127.0.0.1:8889/sim/whip/session-0\n", "the stream was stopped with its DELETE");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_connection_made_during_the_shutdown_is_answered_cancelled() {
        let dir = short_dir("late");
        let socket = dir.join("api.sock");
        // The stream's DELETE never finishes: the shutdown stays in progress until a second signal.
        let server = Server::start(&dir, true);
        server.expect_line("ready");
        server.signal("-INT");
        // Each call on a connection of its own: answered until the signal lands, then refused,
        // never dropped unanswered and never refused at connect.
        let until = Instant::now() + DEADLINE;
        let refused = loop {
            assert!(Instant::now() < until, "the server never said it was shutting down");
            match crate::call(&socket, "form_factors", json!({})) {
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => break e,
            }
        };
        assert_eq!(refused, SHUTTING_DOWN);
        // The socket is still this server's while it shuts down: a new server finds it live and
        // can't take its path (and so can't lose its own socket to this one's cleanup).
        assert!(socket.exists(), "the socket stays until the shutdown removes it");
        let again = crate::call(&socket, "form_factors", json!({})).expect_err("refused");
        assert_eq!(again, SHUTTING_DOWN);
        server.signal("-TERM");
        let (status, err) = server.exit();
        assert_eq!(status.code(), Some(143), "{status}; stderr:\n{err}");
        assert!(!socket.exists(), "the socket is removed on a forced exit");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_second_signal_exits_at_once_when_the_shutdown_is_stuck() {
        let dir = short_dir("twice");
        let socket = dir.join("api.sock");
        // The stream's DELETE never finishes: the first signal's shutdown never ends on its own.
        let server = Server::start(&dir, true);
        server.expect_line("ready");
        // Connected before the signal: the server stops accepting, not answering open connections.
        let mut client = crate::Client::connect(&socket).expect("connect");
        server.signal("-INT");
        // A call sent once the shutdown has begun isn't run.
        let until = Instant::now() + DEADLINE;
        let refused = loop {
            match client.call("form_factors", &json!({})) {
                Err(e) if e.starts_with(SHUTTING_DOWN) => break e,
                Err(e) => panic!("{e}"),
                Ok(_) if Instant::now() > until => panic!("the server never said it was shutting down"),
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        assert_eq!(refused, SHUTTING_DOWN);
        server.signal("-TERM");
        let (status, err) = server.exit();
        assert_eq!(status.code(), Some(143), "{status}; stderr:\n{err}");
        assert!(err.contains("a second SIGTERM: exiting now"), "{err}");
        assert!(!socket.exists(), "the socket is removed on a forced exit too");
        assert!(!dir.join("deletes").exists(), "the stuck DELETE never finished");
        std::fs::remove_dir_all(dir).ok();
    }
}
