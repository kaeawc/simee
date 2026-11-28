#![cfg(unix)]
//! `simee serve` and `simee mcp` on SIGINT and SIGTERM (#388): the real binary as a subprocess,
//! with its own SDK, AVD home and `$HOME`, so no device is reached. It shuts down in order,
//! removes its socket and exits with 128 + the signal. (`sim-api`'s `shutdown` tests cover the
//! in-flight calls, the streams and a second signal.)

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// A deadline for a hang, never a measure of speed.
const DEADLINE: Duration = Duration::from_secs(60);

/// A short directory under `/tmp` (the socket path must fit `sockaddr_un`), removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Dir {
        let dir = PathBuf::from("/tmp").join(format!("ss-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).expect("dir");
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The subprocess, killed if a test fails before it exits.
struct Simee(Child);

impl Drop for Simee {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl Simee {
    fn command(dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_simee"));
        command
            .args(args)
            .env("HOME", dir.join("home"))
            .env("ANDROID_HOME", dir.join("sdk"))
            .env("ANDROID_AVD_HOME", dir.join("avd"))
            .env_remove("ANDROID_SDK_ROOT")
            .env_remove("SIMEE_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn start(dir: &Path, args: &[&str]) -> Simee {
        Simee(Simee::command(dir, args).spawn().expect("simee runs"))
    }

    /// With its stdin and stdout piped (an MCP client's end).
    fn start_piped(dir: &Path, args: &[&str]) -> Simee {
        Simee(Simee::command(dir, args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("simee runs"))
    }

    fn signal(&self, sig: &str) {
        let ok = Command::new("kill").args([sig, &self.0.id().to_string()]).status().expect("kill").success();
        assert!(ok, "kill {sig}");
    }

    fn exit(mut self) -> ExitStatus {
        let until = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.0.try_wait().expect("wait") {
                return status;
            }
            assert!(Instant::now() < until, "simee didn't exit within {DEADLINE:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Waits until a server answers on `socket`.
fn wait_for_server(socket: &Path) {
    let until = Instant::now() + DEADLINE;
    while sim_api::call(socket, "form_factors", serde_json::json!({})).is_err() {
        assert!(Instant::now() < until, "no server on {}", socket.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn serve_stops_on(sig: &str, code: i32) {
    let dir = Dir::new(&sig[1..]);
    let socket = dir.0.join("api.sock");
    let simee = Simee::start(&dir.0, &["serve", "--socket", socket.to_str().expect("path")]);
    wait_for_server(&socket);
    simee.signal(sig);
    let status = simee.exit();
    assert_eq!(status.code(), Some(code), "{status}");
    assert!(!socket.exists(), "the socket is removed");
}

#[test]
fn serve_on_sigint_removes_its_socket_and_exits_130() {
    serve_stops_on("-INT", 130);
}

#[test]
fn serve_on_sigterm_removes_its_socket_and_exits_143() {
    serve_stops_on("-TERM", 143);
}

#[test]
fn mcp_on_sigterm_exits_143_with_its_stdin_still_open() {
    use std::io::{BufRead as _, Write as _};
    let dir = Dir::new("mcp");
    let mut simee = Simee::start_piped(&dir.0, &["mcp"]);
    let mut stdin = simee.0.stdin.take().expect("stdin");
    let mut stdout = std::io::BufReader::new(simee.0.stdout.take().expect("stdout"));
    // Answered: the server is up, so its signals are watched.
    let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}});
    writeln!(stdin, "{init}").expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    assert!(line.contains("\"id\":1"), "{line}");
    simee.signal("-TERM");
    let status = simee.exit();
    assert_eq!(status.code(), Some(143), "{status}");
    drop(stdin);
}
