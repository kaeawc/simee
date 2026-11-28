//! The simee unix socket API.
//!
//! One request per line, one response per line, JSON:
//!
//! ```text
//! → {"id": 1, "method": "tap", "params": {"device": "booted", "x": 600, "y": 1300}}
//! ← {"id": 1, "ok": true, "result": null}
//! ← {"id": 2, "ok": false, "error": "no device matches `nope`"}
//! ```
//!
//! A request with `"binary": true` takes a result's bytes (`screenshot`'s PNG) raw instead of as
//! base64: the response line says `"binary": <len>` and exactly that many bytes follow it
//! ([`Client::call_binary`], docs/api.md).
//!
//! `{"method": "help"}` lists every method with its parameters. Each connection gets its own
//! thread (one a closed connection left waiting, when there is one) and its own service
//! connection. Device methods need the iOS backend, which exists on
//! macOS hosts; elsewhere they fail with a clear reason.

mod android;
mod android_car;
mod android_files;
mod android_net;
mod android_notify;
mod android_state;
mod camera;
#[cfg_attr(not(unix), allow(dead_code))]
mod camera_device;
pub mod catalog;
mod contrast;
#[cfg(target_os = "macos")]
mod duo;
mod forward;
#[cfg(target_os = "macos")]
mod ios;
#[cfg(not(target_os = "macos"))]
#[path = "ios_unavailable.rs"]
mod ios;
mod links;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod memtools;
mod methods;
mod outputs;
mod params;
pub mod prefs;
mod progress;
mod schema;
pub mod settle;
mod shrink;
pub mod shutdown;
mod stream;
mod usage;

use sim_core::net::{UnixListener, UnixStream};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use camera::source_device;
pub use forward::{HANDSHAKE_TIMEOUT, IN_CALLER, forwards, identity};
pub use ios::{flush_input, track_input};
pub use methods::{METHODS, Session, call as call_method, platforms};
pub use outputs::output_schema;
pub use progress::{CallHooks, Progress, call_cancelled, call_progress, call_step, with_call_hooks};
pub use schema::input_schema;
pub use shrink::{AGENT_MAX_SIDE, shrink_png_base64};
pub use sim_core::frame::encode_png;

/// The default socket path: `$SIMEE_SOCKET`, else `$TMPDIR/simee.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("SIMEE_SOCKET") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("simee.sock")
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
    /// The client takes a result's bytes raw, after the response line (docs/api.md, "Binary
    /// replies"); otherwise they are base64 in the line. Only `true` asks: any other value is
    /// ignored, as the key was before binary replies existed.
    #[serde(default)]
    binary: Value,
}

/// A response line, serialized straight from the method's result (no copy into a new `Value`).
/// Fields in the order `json!` wrote them (sorted), so the bytes on the wire are unchanged.
#[derive(Serialize)]
struct Response<'a> {
    /// How many raw bytes follow the line (a binary reply); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    binary: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    id: &'a Value,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
}

/// Appends one method's response line (with its newline) to `out`, saying that `binary` raw bytes
/// follow it when there are some.
fn respond(id: &Value, outcome: &Result<Value, String>, binary: Option<usize>, out: &mut Vec<u8>) {
    let response = match outcome {
        Ok(result) => Response { binary, error: None, id, ok: true, result: Some(result) },
        Err(error) => Response { binary: None, error: Some(error), id, ok: false, result: None },
    };
    // Serializing a `Value` and plain fields into memory cannot fail.
    serde_json::to_writer(&mut *out, &response).ok();
    out.push(b'\n');
}

/// Answers one request line with `call`, appending the response line to `out`; returns the raw
/// bytes to send after it, when the request asked for them (and `binary` allows it) and the
/// result has some.
fn reply(line: &str, binary: bool, out: &mut Vec<u8>, call: impl FnOnce(&str, &Value) -> Result<Value, String>) -> Option<Vec<u8>> {
    let req = match serde_json::from_str::<Request>(line) {
        Ok(req) => req,
        Err(e) => {
            respond(&Value::Null, &Err(format!("bad request: {e}")), None, out);
            return None;
        }
    };
    let (outcome, bytes) = match req.binary == Value::Bool(true) && binary {
        true => params::taking_bytes(|| call(&req.method, &req.params)),
        false => (call(&req.method, &req.params), None),
    };
    let bytes = bytes.filter(|_| outcome.is_ok());
    respond(&req.id, &outcome, bytes.as_ref().map(Vec::len), out);
    bytes
}

/// Handles one request line, appending its whole reply to `out`: the response line (newline
/// included) and, for a binary reply, its raw bytes.
pub fn handle_line_into(session: &mut methods::Session, line: &str, out: &mut Vec<u8>) {
    if let Some(bytes) = reply(line, true, out, |m, p| methods::call(session, m, p)) {
        out.extend_from_slice(&bytes);
    }
}

/// Handles one request line; always returns one response line (bytes as base64, whatever the
/// request asks).
pub fn handle_line(session: &mut methods::Session, line: &str) -> String {
    let mut out = Vec::new();
    reply(line, false, &mut out, |m, p| methods::call(session, m, p));
    out.pop();
    String::from_utf8(out).unwrap_or_default()
}

/// Calls `method` in this process taking a result's bytes raw: the result, with its bytes field
/// (`screenshot`'s `png`) holding `{"binary": <len>}`, and those bytes; `None` when it has none.
pub fn call_method_binary(session: &mut methods::Session, method: &str, params: &Value) -> Result<(Value, Option<Vec<u8>>), String> {
    let (outcome, bytes) = params::taking_bytes(|| methods::call(session, method, params));
    outcome.map(|result| (result, bytes))
}

/// Requests are read through a buffer this large, and responses read back through one too.
const IO_BUFFER: usize = 64 * 1024;

fn serve_connection(stream: UnixStream, make: &(dyn Fn() -> methods::Session + Send + Sync)) {
    let mut session = make();
    serve_stream(stream, |m, p| methods::call(&mut session, m, p));
}

/// Answers the requests on `stream` with `call`, one at a time, until it closes.
fn serve_stream(stream: UnixStream, mut call: impl FnMut(&str, &Value) -> Result<Value, String>) {
    let Ok(reader) = stream.try_clone() else { return };
    let mut writer = stream;
    let mut out = Vec::new();
    for line in BufReader::with_capacity(IO_BUFFER, reader).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        out.clear();
        // Counted while it runs and its reply goes out, so a shutdown waits for both.
        let Some(_running) = shutdown::enter() else {
            reply(&line, false, &mut out, |_, _| Err(shutdown::SHUTTING_DOWN.to_owned()));
            let _ = writer.write_all(&out);
            break;
        };
        let bytes = reply(&line, true, &mut out, |m, p| shutdown::outcome(call(m, p)));
        // The bytes go out from the result's own buffer, not copied after the line.
        if writer.write_all(&out).and_then(|()| bytes.map_or(Ok(()), |b| writer.write_all(&b))).is_err() {
            break;
        }
        // Don't hold a multi-megabyte screenshot's buffer between requests.
        if out.capacity() > IO_BUFFER {
            out = Vec::new();
        }
    }
}

/// Makes each connection's session.
type MakeSession = Arc<dyn Fn() -> methods::Session + Send + Sync>;

/// Connection threads whose connection closed, waiting a while for the next one: handing a new
/// connection to one costs less than starting a thread for it. Each connection still gets a
/// session of its own, made for it, and a thread of its own while it is open.
struct Threads {
    make: MakeSession,
    /// Idle threads, newest last, by id.
    idle: Mutex<Vec<(u64, Sender<UnixStream>)>>,
    next_id: AtomicU64,
    /// At most this many threads wait for connections; more than that end with theirs.
    max_idle: usize,
    /// How long an idle thread waits for a connection before it ends.
    idle_for: Duration,
}

impl Threads {
    fn new(make: MakeSession) -> Arc<Threads> {
        Threads::with_limits(make, 8, Duration::from_secs(30))
    }

    fn with_limits(make: MakeSession, max_idle: usize, idle_for: Duration) -> Arc<Threads> {
        Arc::new(Threads { make, idle: Mutex::default(), next_id: AtomicU64::new(0), max_idle, idle_for })
    }

    fn idle(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Sender<UnixStream>)>> {
        self.idle.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serves `stream` on an idle thread, or on a new one when none waits.
    fn serve(self: &Arc<Threads>, stream: UnixStream) -> std::io::Result<()> {
        let mut stream = stream;
        {
            // Sent under the lock, so a thread that finds itself no longer listed knows a
            // connection is in its channel ([`Threads::run`]).
            let mut idle = self.idle();
            while let Some((_, thread)) = idle.pop() {
                match thread.send(stream) {
                    Ok(()) => return Ok(()),
                    Err(std::sync::mpsc::SendError(back)) => stream = back,
                }
            }
        }
        let threads = self.clone();
        std::thread::Builder::new().name("simee-api".into()).spawn(move || threads.run(stream)).map(drop)
    }

    /// A connection thread: serves `stream`, then the connections handed to it while it waits.
    fn run(&self, stream: UnixStream) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        let mut stream = stream;
        loop {
            serve_connection(stream, &*self.make);
            {
                let mut idle = self.idle();
                if idle.len() >= self.max_idle {
                    return;
                }
                idle.push((id, tx.clone()));
            }
            stream = match rx.recv_timeout(self.idle_for) {
                Ok(next) => next,
                Err(_) => {
                    let mut idle = self.idle();
                    match idle.iter().position(|(other, _)| *other == id) {
                        Some(i) => {
                            idle.remove(i);
                            return;
                        }
                        // Taken off the list as the wait ended: its connection is in the channel.
                        None => match rx.try_recv() {
                            Ok(next) => next,
                            Err(_) => return,
                        },
                    }
                }
            };
        }
    }
}

/// Stops every running stream, each sending its WHIP DELETE: call it before the process exits.
pub fn stop_streams() {
    stream::stop_all();
}

/// How long a shutdown gives the calls in flight to finish, or to stop at a safe point, before it
/// stops the streams and exits anyway. On every host (callers outside this crate size their own
/// drain by it), though only unix has the signals that start one.
pub const DRAIN: Duration = Duration::from_secs(10);

/// Listens on `path` (replacing a stale socket) and serves connections until SIGINT or SIGTERM
/// (#388). Then it shuts down in order: calls in flight get up to 10 s (each stops at its next
/// safe point, answering `cancelled: server shutting down`), and every call from then on, on a
/// connection already open or one made during the shutdown, is answered so without running. It
/// stops every stream with its WHIP DELETE, removes its socket (listening on it until then, so no
/// other server takes the path meanwhile), and returns the exit code: 130 for SIGINT, 143 for
/// SIGTERM. A second signal exits at once ([`shutdown::watch`]).
///
/// Once a signal arrives, [`call_cancelled`] is true for every call in this process, as a watched
/// caller's cancel is for its own call.
#[cfg(unix)]
pub fn serve(path: &Path) -> std::io::Result<i32> {
    serve_until_signal(path, Arc::new(methods::Session::default), DRAIN, || ())
}

/// The API socket is a Unix socket, and Windows has none here yet ([`sim_core::net`]).
#[cfg(not(unix))]
pub fn serve(_path: &Path) -> std::io::Result<i32> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "the API socket isn't implemented on Windows yet"))
}

/// [`serve`]'s loop and shutdown, with each connection's session made by `make` and `drain` for
/// how long the calls in flight get; `ready` is called once the socket is bound and the signals
/// are watched.
#[cfg(unix)]
fn serve_until_signal(path: &Path, make: MakeSession, drain: Duration, ready: impl FnOnce()) -> std::io::Result<i32> {
    let listener = bind(path)?;
    // Held until the shutdown has removed the file: a socket still bound keeps its inode.
    shutdown::remove_on_exit(path);
    listener.set_nonblocking(true)?;
    // The shutdown runs on a thread of its own while this one goes on accepting, and says it is
    // done on a socket of the server's own: nothing depends on the socket file still being there.
    let (done, says_done) = UnixStream::pair()?;
    shutdown::watch(move |_| {
        shut_down(drain);
        let _ = (&says_done).write_all(&[1]);
    })?;
    let threads = Threads::new(make);
    ready();
    let served = accept_until_done(&listener, &done, &threads);
    if served.is_err() {
        shut_down(drain);
    }
    drop(listener);
    served.map(|()| shutdown::signal().map_or(0, shutdown::exit_code))
}

/// [`serve`]'s shutdown: the calls in flight get `drain`, then the streams stop and the sockets
/// are removed.
#[cfg(unix)]
fn shut_down(drain: Duration) {
    if !shutdown::drain(drain) {
        shutdown::say!("simee: calls still running after {} s: stopping anyway", drain.as_secs());
    }
    stream::stop_all();
    shutdown::remove_sockets();
}

/// Serves the connections `listener` (non-blocking) accepts until `done` is readable.
#[cfg(unix)]
fn accept_until_done(listener: &UnixListener, done: &UnixStream, threads: &Arc<Threads>) -> std::io::Result<()> {
    use rustix::event::{PollFd, PollFlags, poll};
    loop {
        let mut fds = [PollFd::new(listener, PollFlags::IN), PollFd::new(done, PollFlags::IN)];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        if !fds[1].revents().is_empty() {
            return Ok(());
        }
        match listener.accept() {
            // An accepted socket inherits the listener's O_NONBLOCK on macOS.
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_ok() {
                    threads.serve(stream)?;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {}
            // Out of descriptors, say: keep serving, as a blocking accept did, without spinning.
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Serves on a background thread; returns once the socket is bound.
pub fn serve_in_background(path: PathBuf) -> std::io::Result<()> {
    serve_in_background_with(path, methods::Session::default)
}

/// [`serve_in_background`] with each connection's session made by `make` (a test's own SDK and
/// AVD home, say).
pub fn serve_in_background_with(path: PathBuf, make: impl Fn() -> methods::Session + Send + Sync + 'static) -> std::io::Result<()> {
    let listener = bind(&path)?;
    let threads = Threads::new(Arc::new(make));
    std::thread::Builder::new().name("simee-api-accept".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = threads.serve(stream);
        }
    })?;
    Ok(())
}

/// Whether this process serves the API, and so outlives each call: state one call leaves for the
/// next (a pointer button held by `pointer`'s `down`) only means something then.
static SERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn serving() -> bool {
    SERVING.load(std::sync::atomic::Ordering::Relaxed)
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    // Fix this server's identity now, before its executable can be rebuilt under it.
    identity();
    SERVING.store(true, std::sync::atomic::Ordering::Relaxed);
    // The host sampler is not started here: it starts at the first read (`host`, `devices_usage`,
    // an admission check) and parks again a minute after the last (`sim_host::LINGER`).
    if path.exists() && UnixStream::connect(path).is_err() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Sends one request and waits for its response.
pub fn call(path: &Path, method: &str, params: Value) -> Result<Value, String> {
    Client::connect(path).map_err(|e| format!("{}: {e}", path.display()))?.call(method, &params)
}

/// A connection to a running server: one request at a time, each answered before the next.
pub struct Client {
    stream: BufReader<UnixStream>,
    next_id: u64,
    line: Vec<u8>,
}

#[derive(Serialize)]
struct Outgoing<'a> {
    id: u64,
    method: &'a str,
    params: &'a Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    binary: bool,
}

impl Client {
    /// Connects to the server on `path`. A missing or stale socket (nothing listening) fails at
    /// once rather than waiting.
    pub fn connect(path: &Path) -> std::io::Result<Client> {
        Ok(Client::from_stream(UnixStream::connect(path)?))
    }

    fn from_stream(stream: UnixStream) -> Client {
        Client { stream: BufReader::with_capacity(IO_BUFFER, stream), next_id: 1, line: Vec::new() }
    }

    /// Calls one method; its result, or the server's error. Bytes in the result are base64.
    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        self.exchange(method, params, false).map(|(result, _)| result)
    }

    /// Calls one method taking a result's bytes raw (a binary reply, docs/api.md): the result,
    /// with its bytes field (`screenshot`'s `png`) holding `{"binary": <len>}`, and those bytes.
    /// A method with no bytes field answers as [`Client::call`] does, with `None`; so does a server
    /// from before binary replies, which leaves the bytes base64 in the result.
    pub fn call_binary(&mut self, method: &str, params: &Value) -> Result<(Value, Option<Vec<u8>>), String> {
        self.exchange(method, params, true)
    }

    fn exchange(&mut self, method: &str, params: &Value, binary: bool) -> Result<(Value, Option<Vec<u8>>), String> {
        let mut request = serde_json::to_vec(&Outgoing { id: self.next_id, method, params, binary }).map_err(|e| e.to_string())?;
        self.next_id += 1;
        request.push(b'\n');
        self.stream.get_mut().write_all(&request).map_err(|e| e.to_string())?;
        self.line.clear();
        self.stream.read_until(b'\n', &mut self.line).map_err(|e| e.to_string())?;
        let mut value: Value = serde_json::from_slice(&self.line).map_err(|e| format!("bad response: {e}"))?;
        if self.line.capacity() > IO_BUFFER {
            self.line = Vec::new();
        }
        // Read a binary reply's bytes whatever the outcome, so the next reply starts in place.
        let bytes = match value.get("binary").and_then(Value::as_u64) {
            Some(len) => Some(self.read_bytes(len)?),
            None => None,
        };
        match value["ok"] == true {
            true => Ok((value["result"].take(), bytes)),
            false => Err(value["error"].as_str().unwrap_or("error").to_owned()),
        }
    }

    /// The `len` raw bytes after a binary reply's line; an error if the stream ends first.
    fn read_bytes(&mut self, len: u64) -> Result<Vec<u8>, String> {
        use std::io::Read as _;
        // Grown as the bytes arrive past 64 MB, rather than allocated to whatever a reply names.
        let mut bytes = Vec::with_capacity(len.min(64 << 20) as usize);
        (&mut self.stream).take(len).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if (bytes.len() as u64) < len {
            return Err(format!("the reply ended after {} of {len} bytes", bytes.len()));
        }
        Ok(bytes)
    }
}

/// A socket path for tests: under the temp dir when it fits a `sockaddr_un` (104 bytes on macOS),
/// else under `/tmp`, since build wrappers point `TMPDIR` at long private directories.
#[cfg(all(test, unix))]
pub(crate) fn test_socket(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    let path = if path.as_os_str().len() < 100 { path } else { PathBuf::from("/tmp").join(name) };
    std::fs::remove_file(&path).ok();
    path
}

#[cfg(all(test, unix))]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn help_and_errors_need_no_device() {
        let mut s = methods::Session::default();
        let help: Value = serde_json::from_str(&handle_line(&mut s, r#"{"id":7,"method":"help"}"#)).expect("json");
        assert_eq!(help["id"], 7);
        assert!(help["result"].as_array().is_some_and(|m| m.len() > 40));
        let bad: Value = serde_json::from_str(&handle_line(&mut s, "not json")).expect("json");
        assert_eq!(bad["ok"], false);
        let unknown: Value = serde_json::from_str(&handle_line(&mut s, r#"{"method":"nope"}"#)).expect("json");
        assert!(unknown["error"].as_str().is_some_and(|e| e.contains("unknown method")));
        let missing: Value = serde_json::from_str(&handle_line(&mut s, r#"{"method":"ca_debug","params":{"device":"x"}}"#)).expect("json");
        assert_eq!(missing["ok"], false);
    }

    #[test]
    fn serves_over_a_socket() {
        let path = crate::test_socket(&format!("simee-api-test-{}.sock", std::process::id()));
        serve_in_background(path.clone()).expect("bind");
        let result = call(&path, "help", Value::Null).expect("help");
        assert!(result.as_array().is_some_and(|m| m.iter().any(|x| x["method"] == "tap")));
        std::fs::remove_file(path).ok();
    }

    /// The response line as it was built before: a `json!` object, then a string.
    fn old_response(id: &Value, outcome: &Result<Value, String>) -> String {
        match outcome {
            Ok(result) => serde_json::json!({"id": id, "ok": true, "result": result}),
            Err(error) => serde_json::json!({"id": id, "ok": false, "error": error}),
        }
        .to_string()
    }

    #[test]
    fn responses_are_the_same_bytes_as_before() {
        let cases = [
            (json!(7), Ok(Value::Null)),
            (Value::Null, Ok(json!({"width": 1206, "height": 2622.5, "png": "iVBOR\"\\\u{1}é"}))),
            (json!("abc"), Ok(json!([1, "two", {"z": 1, "a": [true, null]}]))),
            (json!({"k": 1}), Err("no device matches `nope`\n\"quoted\"".to_owned())),
            (Value::Null, Err(String::new())),
        ];
        for (id, outcome) in &cases {
            let mut out = Vec::new();
            respond(id, outcome, None, &mut out);
            assert_eq!(String::from_utf8(out).expect("utf-8"), old_response(id, outcome) + "\n");
        }
        let mut s = methods::Session::default();
        assert_eq!(handle_line(&mut s, "not json"), old_response(&Value::Null, &Err(bad_request("not json"))));
    }

    fn bad_request(line: &str) -> String {
        format!("bad request: {}", serde_json::from_str::<Request>(line).err().expect("bad"))
    }

    /// A server answering every request with `result` (or `error` for method `fail`), with the
    /// old or the new response path.
    fn fixed_server(tag: &str, result: Value, old: bool) -> PathBuf {
        let path = crate::test_socket(&format!("simee-api-{tag}-{}.sock", std::process::id()));
        let listener = bind(&path).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let result = result.clone();
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    let mut out = Vec::new();
                    for line in BufReader::new(stream).lines() {
                        let req: Request = serde_json::from_str(&line.expect("line")).expect("request");
                        let outcome = if req.method == "fail" { Err("it failed".to_owned()) } else { Ok(result.clone()) };
                        if old {
                            let mut line = old_response(&req.id, &outcome);
                            line.push('\n');
                            writer.write_all(line.as_bytes()).expect("write");
                        } else {
                            out.clear();
                            respond(&req.id, &outcome, None, &mut out);
                            writer.write_all(&out).expect("write");
                        }
                    }
                });
            }
        });
        path
    }

    /// The client as it was: a new connection, `read_line`, then the result cloned out.
    fn old_call(path: &Path, method: &str, params: Value) -> Result<Value, String> {
        let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
        let mut line = json!({"id": 1, "method": method, "params": params}).to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&response).map_err(|e| format!("bad response: {e}"))?;
        if value["ok"] == true { Ok(value["result"].clone()) } else { Err(value["error"].as_str().unwrap_or("error").to_owned()) }
    }

    #[test]
    fn a_client_keeps_its_connection_across_calls() {
        let shot = json!({"width": 2, "height": 1, "png": "x".repeat(200_000)});
        let path = fixed_server("client", shot.clone(), false);
        let mut client = Client::connect(&path).expect("connect");
        for _ in 0..3 {
            assert_eq!(client.call("screenshot", &json!({"device": "booted"})), Ok(shot.clone()));
            assert_eq!(client.call("fail", &Value::Null), Err("it failed".to_owned()));
        }
        assert_eq!(call(&path, "screenshot", Value::Null), old_call(&path, "screenshot", Value::Null));
        std::fs::remove_file(&path).ok();
        let gone = Client::connect(&path).err().expect("nothing listening");
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_stale_socket_fails_at_once() {
        let path = crate::test_socket(&format!("simee-api-stale-{}.sock", std::process::id()));
        drop(UnixListener::bind(&path).expect("bind"));
        let start = std::time::Instant::now();
        assert!(Client::connect(&path).is_err(), "the listener is gone");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(&path).ok();
    }

    /// PNG-like bytes with every byte a JSON line could trip on: newlines, quotes, invalid UTF-8.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\"\\\xff\xfe binary \n bytes\n";

    /// A method table for tests: `screenshot` returns `PNG` through the real result builder,
    /// `screen` a result with no bytes field, anything else an error.
    fn fake_method(method: &str, p: &Value) -> Result<Value, String> {
        match method {
            "screenshot" => params::png_result(2, 1, PNG, p),
            "screen" => Ok(json!({"width": 2, "height": 1})),
            _ => Err("it failed".to_owned()),
        }
    }

    /// A client on one end of a socket pair whose other end the real serve loop answers with
    /// `fake_method`.
    fn fake_pair() -> Client {
        let (ours, theirs) = UnixStream::pair().expect("pair");
        std::thread::spawn(move || serve_stream(theirs, fake_method));
        Client::from_stream(ours)
    }

    #[test]
    fn a_binary_reply_reads_back_byte_identical() {
        use base64::Engine as _;
        let mut client = fake_pair();
        for _ in 0..2 {
            let (result, bytes) = client.call_binary("screenshot", &json!({"device": "booted"})).expect("binary");
            assert_eq!(result, json!({"width": 2, "height": 1, "png": {"binary": PNG.len()}}));
            assert_eq!(bytes.as_deref(), Some(PNG));
            // An old-style request on the same connection: base64 in the line, as before.
            let old = client.call("screenshot", &json!({})).expect("base64");
            assert_eq!(old["png"], base64::engine::general_purpose::STANDARD.encode(PNG));
            // Binary asked of a method with no bytes field, or one that fails: the normal reply.
            assert_eq!(client.call_binary("screen", &json!({})), Ok((json!({"width": 2, "height": 1}), None)));
            assert_eq!(client.call_binary("fail", &Value::Null), Err("it failed".to_owned()));
        }
    }

    #[test]
    fn a_binary_reply_is_the_json_line_then_exactly_its_bytes() {
        use base64::Engine as _;
        let (mut ours, theirs) = UnixStream::pair().expect("pair");
        std::thread::spawn(move || serve_stream(theirs, fake_method));
        ours.write_all(b"{\"id\":1,\"method\":\"screenshot\",\"binary\":true}\n{\"id\":2,\"method\":\"screenshot\"}\n").expect("write");
        ours.write_all(b"{\"id\":3,\"method\":\"screen\",\"binary\":true}\n").expect("write");
        ours.shutdown(std::net::Shutdown::Write).expect("shutdown");
        let mut wire = Vec::new();
        std::io::Read::read_to_end(&mut ours, &mut wire).expect("read");
        let n = PNG.len();
        let mut expected =
            format!(r#"{{"binary":{n},"id":1,"ok":true,"result":{{"height":1,"png":{{"binary":{n}}},"width":2}}}}"#).into_bytes();
        expected.push(b'\n');
        expected.extend(PNG);
        let base64 = json!({"width": 2, "height": 1, "png": base64::engine::general_purpose::STANDARD.encode(PNG)});
        expected.extend((old_response(&json!(2), &Ok(base64)) + "\n").as_bytes());
        expected.extend((old_response(&json!(3), &Ok(json!({"width": 2, "height": 1}))) + "\n").as_bytes());
        assert_eq!(String::from_utf8_lossy(&wire), String::from_utf8_lossy(&expected));
        assert_eq!(wire, expected);
    }

    /// Everything the serve loop sends back for `input`, once the client stops writing.
    fn served(input: &[u8]) -> Vec<u8> {
        let (mut ours, theirs) = UnixStream::pair().expect("pair");
        std::thread::spawn(move || serve_stream(theirs, |m, _| if m == "fail" { Err("it failed".into()) } else { Ok(json!(m)) }));
        ours.write_all(input).expect("write");
        ours.shutdown(std::net::Shutdown::Write).expect("shutdown");
        let mut wire = Vec::new();
        std::io::Read::read_to_end(&mut ours, &mut wire).expect("read");
        wire
    }

    #[test]
    fn the_serve_loop_reads_lines_as_it_always_has() {
        // `\n` and `\r\n` endings, blank and whitespace-only lines skipped, a bad line answered, and
        // a last line with no newline answered at the end of the stream.
        let wire =
            served(b"{\"id\":1,\"method\":\"a\"}\r\n\n \t\r\n{\"id\":2,\"method\":\"fail\"}\nnot json\r\n{\"id\":3,\"method\":\"c\"}");
        let expected = [
            old_response(&json!(1), &Ok(json!("a"))),
            old_response(&json!(2), &Err("it failed".into())),
            old_response(&Value::Null, &Err(bad_request("not json"))),
            old_response(&json!(3), &Ok(json!("c"))),
        ]
        .map(|line| line + "\n")
        .concat();
        assert_eq!(String::from_utf8_lossy(&wire), expected);
        // A line that isn't UTF-8 ends the connection: nothing after it is answered.
        let wire = served(b"{\"id\":1,\"method\":\"a\"}\n\xff\xfe\n{\"id\":2,\"method\":\"b\"}\n");
        assert_eq!(String::from_utf8_lossy(&wire), old_response(&json!(1), &Ok(json!("a"))) + "\n");
    }

    #[test]
    fn served_replies_are_the_bytes_handle_line_gives() {
        let path = crate::test_socket(&format!("simee-api-same-{}.sock", std::process::id()));
        serve_in_background(path.clone()).expect("bind");
        let mut s = methods::Session::default();
        for _ in 0..2 {
            let mut stream = BufReader::new(UnixStream::connect(&path).expect("connect"));
            for line in [
                r#"{"id":1,"method":"identity"}"#,
                r#"{"id":"two","method":"help","params":null}"#,
                r#"{"id":3,"method":"form_factors","params":{"platform":"android"}}"#,
                r#"{"id":4,"method":"form_factors","params":{"platform":"plan9"}}"#,
                r#"{"id":5,"method":"nope"}"#,
                r#"{"method":"identity","binary":true}"#,
                "{",
            ] {
                stream.get_mut().write_all(format!("{line}\n").as_bytes()).expect("write");
                let mut reply = String::new();
                stream.read_line(&mut reply).expect("read");
                assert_eq!(reply, handle_line(&mut s, line) + "\n", "{line}");
            }
        }
        std::fs::remove_file(path).ok();
    }

    /// A session maker that records which thread made each session.
    fn recording() -> (MakeSession, Arc<Mutex<Vec<std::thread::ThreadId>>>) {
        let made = Arc::new(Mutex::new(Vec::new()));
        let log = made.clone();
        let make: MakeSession = Arc::new(move || {
            log.lock().expect("log").push(std::thread::current().id());
            methods::Session::default()
        });
        (make, made)
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A client whose connection `threads` serves.
    fn connect_to(threads: &Arc<Threads>) -> Client {
        let (ours, theirs) = UnixStream::pair().expect("pair");
        threads.serve(theirs).expect("serve");
        Client::from_stream(ours)
    }

    #[test]
    fn a_connection_thread_serves_the_next_connection_with_a_new_session() {
        let (make, made) = recording();
        let threads = Threads::new(make);
        for n in 1..=3 {
            let mut client = connect_to(&threads);
            assert_eq!(client.call("identity", &Value::Null).as_ref(), Ok(identity()));
            drop(client);
            wait_until("the thread waits for the next connection", || threads.idle().len() == 1);
            assert_eq!(made.lock().expect("made").len(), n, "a session of its own for each connection");
        }
        let made = made.lock().expect("made");
        assert!(made.iter().all(|id| *id == made[0]), "one thread served all three in turn: {made:?}");
    }

    #[test]
    fn open_connections_have_threads_of_their_own_and_idle_threads_are_bounded() {
        let (make, made) = recording();
        let threads = Threads::with_limits(make, 2, Duration::from_millis(300));
        let mut clients: Vec<Client> = (0..4).map(|_| connect_to(&threads)).collect();
        for client in &mut clients {
            assert_eq!(client.call("identity", &Value::Null).as_ref(), Ok(identity()));
        }
        let ids = made.lock().expect("made").clone();
        assert_eq!(ids.len(), 4);
        assert!(ids.iter().enumerate().all(|(i, a)| !ids[..i].contains(a)), "four threads at once: {ids:?}");
        drop(clients);
        // Two wait for the next connection (the other two ended), then those end too.
        wait_until("two threads wait", || threads.idle().len() == 2);
        wait_until("idle threads end after a while", || threads.idle().is_empty());
        // A connection after that is served on a new thread.
        let mut client = connect_to(&threads);
        assert_eq!(client.call("identity", &Value::Null).as_ref(), Ok(identity()));
        assert!(!ids.contains(made.lock().expect("made").last().expect("made")));
    }

    #[test]
    fn a_truncated_binary_reply_is_an_error_not_a_hang() {
        let (ours, theirs) = UnixStream::pair().expect("pair");
        std::thread::spawn(move || {
            let mut writer = theirs.try_clone().expect("clone");
            let mut line = String::new();
            BufReader::new(theirs).read_line(&mut line).expect("request");
            // Ten bytes promised, four sent, then the connection closes.
            writer.write_all(b"{\"binary\":10,\"id\":1,\"ok\":true,\"result\":{\"png\":{\"binary\":10}}}\n\x89PNG").expect("write");
        });
        // A guard only: a hang fails the test with a timeout error instead of stalling the run.
        ours.set_read_timeout(Some(std::time::Duration::from_secs(10))).expect("timeout");
        let start = std::time::Instant::now();
        let err = Client::from_stream(ours).call_binary("screenshot", &Value::Null).expect_err("truncated");
        assert_eq!(err, "the reply ended after 4 of 10 bytes");
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
    }

    #[test]
    fn binary_is_asked_per_request_and_other_servers_answer_as_before() {
        // A server from before binary replies ignores the field: base64 comes back, no bytes.
        let shot = json!({"width": 2, "height": 1, "png": "iVBOR"});
        let path = fixed_server("binary-old", shot.clone(), true);
        let mut client = Client::connect(&path).expect("connect");
        assert_eq!(client.call_binary("screenshot", &Value::Null), Ok((shot, None)));
        std::fs::remove_file(&path).ok();
        // The real server, a method with no bytes: its JSON, then the next call on the connection.
        let path = crate::test_socket(&format!("simee-api-binary-{}.sock", std::process::id()));
        serve_in_background(path.clone()).expect("bind");
        let mut client = Client::connect(&path).expect("connect");
        let (help, bytes) = client.call_binary("help", &Value::Null).expect("help");
        assert!(help.as_array().is_some_and(|m| m.len() > 40) && bytes.is_none());
        assert_eq!(client.call("help", &Value::Null), Ok(help));
        std::fs::remove_file(path).ok();
        // In this process: a result's bytes come back beside it only when asked for.
        let mut s = methods::Session::default();
        assert!(call_method_binary(&mut s, "help", &Value::Null).is_ok_and(|(_, bytes)| bytes.is_none()));
        // `handle_line` gives one JSON line, so a binary request there gets base64.
        let line = handle_line(&mut s, r#"{"id":4,"method":"help","binary":true}"#);
        assert!(line.starts_with(r#"{"id":4,"ok":true,"result":[{"#), "{}", &line[..40]);
    }

    /// A 4.4 MB PNG through the socket on a kept connection, median of 50 each, interleaved: base64
    /// in the line against a binary reply, and the server writing `path` against a binary reply
    /// the client writes to the same file.
    ///     cargo test --release -p simee-api binary_reply_round_trip -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn binary_reply_round_trip() {
        const LEN: usize = 4_400_000;
        let png: std::sync::Arc<Vec<u8>> =
            std::sync::Arc::new((0..LEN as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect());
        let (ours, theirs) = UnixStream::pair().expect("pair");
        let served = png.clone();
        std::thread::spawn(move || serve_stream(theirs, move |_, p| params::png_result(1206, 2622, served.as_slice(), p)));
        let mut client = Client::from_stream(ours);
        let file = std::env::temp_dir().join(format!("simee-binary-bench-{}.png", std::process::id()));
        let to_file = json!({"path": file.to_str()});
        let mut runs: [Vec<f64>; 4] = Default::default();
        let mut time = |i: usize, f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            f();
            runs[i].push(t.elapsed().as_secs_f64() * 1000.0);
        };
        for _ in 0..50 {
            time(0, &mut || {
                assert_eq!(client.call("screenshot", &Value::Null).expect("base64")["png"].as_str().map(str::len), Some(5_866_668))
            });
            time(1, &mut || assert_eq!(client.call_binary("screenshot", &Value::Null).expect("binary").1.map(|b| b.len()), Some(LEN)));
            time(2, &mut || drop(client.call("screenshot", &to_file).expect("server writes")));
            time(3, &mut || {
                let bytes = client.call_binary("screenshot", &Value::Null).expect("binary").1.expect("bytes");
                std::fs::write(&file, bytes).expect("client writes");
            });
        }
        let [base64, binary, server_file, client_file] = runs.map(|mut v| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        });
        println!(
            "4.4 MB screenshot, median of 50: base64 {base64:.2} ms, binary {binary:.2} ms; to a file: server writes {server_file:.2} ms, \
             binary then client writes {client_file:.2} ms"
        );
        std::fs::remove_file(file).ok();
    }

    /// The median of 5 rounds of the mean microseconds per call of `f` (up to 20 000 calls or
    /// 300 ms a round).
    fn per_call_us(mut f: impl FnMut()) -> f64 {
        let mut rounds: Vec<f64> = (0..5)
            .map(|_| {
                let start = std::time::Instant::now();
                let mut n = 0;
                while n < 20_000 && (n < 20 || start.elapsed() < std::time::Duration::from_millis(300)) {
                    f();
                    n += 1;
                }
                start.elapsed().as_secs_f64() * 1e6 / n as f64
            })
            .collect();
        rounds.sort_by(f64::total_cmp);
        rounds[2]
    }

    /// Where a cheap call's time goes, in this process: parsing the request line, the method
    /// (dropping its result included), writing the response line; then the whole of
    /// `handle_line_into`, a round trip on a kept connection, and one on a new connection per call.
    ///     cargo test --release -p simee-api call_overhead -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn call_overhead() {
        let path = crate::test_socket(&format!("simee-api-ovh-{}.sock", std::process::id()));
        serve_in_background(path.clone()).expect("bind");
        let mut client = Client::connect(&path).expect("connect");
        let mut s = methods::Session::default();
        let mut out = Vec::new();
        // `method` includes dropping its result; `line` is all of `handle_line_into`.
        println!(
            "{:<13} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9}  µs per call",
            "method", "parse", "method", "write", "line", "kept", "per-conn"
        );
        for method in ["identity", "help", "form_factors"] {
            let line = format!(r#"{{"id":1,"method":"{method}"}}"#);
            let parse = per_call_us(|| drop(std::hint::black_box(serde_json::from_str::<Request>(&line).expect("request"))));
            let run = per_call_us(|| drop(std::hint::black_box(methods::call(&mut s, method, &Value::Null))));
            let result = methods::call(&mut s, method, &Value::Null);
            let write = per_call_us(|| {
                out.clear();
                respond(&json!(1), &result, None, &mut out);
            });
            let whole = per_call_us(|| {
                out.clear();
                handle_line_into(&mut s, &line, &mut out);
            });
            let kept = per_call_us(|| drop(client.call(method, &Value::Null).expect("kept")));
            let fresh = per_call_us(|| drop(call(&path, method, Value::Null).expect("per connection")));
            println!("{method:<13} {parse:>8.2} {run:>8.2} {write:>8.2} {whole:>8.2} {kept:>8.2} {fresh:>9.2}");
        }
        std::fs::remove_file(path).ok();
    }

    /// A 3 MB screenshot response's round trip over the socket, old path against new (server
    /// response building and client parsing), the median of 15 runs each, interleaved.
    ///     cargo test --release -p simee-api large_response_round_trip -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn large_response_round_trip() {
        let png: String = (0..3 << 20).map(|i: u32| char::from(b'A' + (i.wrapping_mul(2_654_435_761) >> 27) as u8)).collect();
        let shot = json!({"width": 1206, "height": 2622, "png": png});
        let (old_path, new_path) = (fixed_server("bench-old", shot.clone(), true), fixed_server("bench-new", shot.clone(), false));
        let mut client = Client::connect(&new_path).expect("connect");
        let (mut old, mut new, mut new_kept) = (Vec::new(), Vec::new(), Vec::new());
        let time = |f: &mut dyn FnMut() -> Value| {
            let t = std::time::Instant::now();
            assert_eq!(f()["png"].as_str().map(str::len), Some(3 << 20));
            t.elapsed().as_secs_f64() * 1000.0
        };
        for _ in 0..15 {
            old.push(time(&mut || old_call(&old_path, "screenshot", Value::Null).expect("old")));
            new.push(time(&mut || call(&new_path, "screenshot", Value::Null).expect("new")));
            new_kept.push(time(&mut || client.call("screenshot", &Value::Null).expect("kept")));
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "3 MB response: old {:.2} ms, new {:.2} ms, new on a kept connection {:.2} ms",
            median(&mut old),
            median(&mut new),
            median(&mut new_kept)
        );
        std::fs::remove_file(old_path).ok();
        std::fs::remove_file(new_path).ok();
    }
}
