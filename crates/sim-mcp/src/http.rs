//! MCP's Streamable HTTP transport (`simee mcp --http`): one endpoint, [`ENDPOINT`], on a small
//! blocking HTTP/1.1 server with a thread per connection, up to [`Limits::max_connections`].
//!
//! - `POST` carries one JSON-RPC message. A notification or response is accepted with 202. A
//!   request is answered with `application/json`, or, once the server has something to say about
//!   it before its response (progress, a subscription's notifications), with a `text/event-stream`
//!   that ends after the response.
//! - `GET` opens a session's stream for what belongs to no request (an older revision's resource
//!   updates). `DELETE` ends a session.
//! - A client of the current revision is stateless and needs no session. A client of an earlier one
//!   opens with `initialize`, which answers with an `Mcp-Session-Id`; every request after it names
//!   that session, and each session is its own [`Server`], as a stdio connection is.
//!
//! Every message goes through the same [`Server::handle_line`] that stdio uses. Request ids and
//! progress tokens are swapped for ones unique to this process on the way in and back on the way
//! out, so two clients that both send id 1 each get their own answer.
//!
//! Security: every bind requires a bearer token from a token file only its owner can read (created
//! in simee's state directory when none is named); [`Config::no_auth`] turns that off, on a
//! loopback bind only. A non-loopback bind needs TLS ([`Config::tls`], the `tls` cargo feature) or
//! [`Config::insecure_remote`] (plain HTTP: the token crosses the network in the clear). A browser's
//! `Origin` must be a loopback one, and on a loopback bind so must `Host` (DNS rebinding). A request
//! head has one deadline from its first byte, bodies are bounded in size and time, connections are
//! capped per peer address and in all, and an error closes the connection (so a body nobody read is
//! never taken for the next request). Over TLS the handshake is under the same deadline and slots,
//! and reads are bounded by one deadline however many socket reads a record takes (`conn.rs`). A
//! session belongs to the token that opened it. SSE streams carry no event ids, so a dropped stream
//! can't be resumed.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::rpc::{self, INVALID_REQUEST, Incoming, LEGACY, META_SUBSCRIPTION, MODERN, PARSE_ERROR};
use crate::server::{Server, Timing};
use crate::tasks::Sink;
use crate::tools::Toolsets;

mod conn;
mod tls;

use conn::Conn;

/// The one path the transport serves.
pub const ENDPOINT: &str = "/mcp";
/// Where `simee mcp --http` listens when given no address.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8377";
/// The token file's name in simee's state directory.
pub const TOKEN_FILE: &str = "mcp-token";

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
/// A token shorter than this is refused: it could be guessed.
const MIN_TOKEN: usize = 16;
/// CR and LF bytes allowed before a request line (RFC 9112 asks a server to skip at least one
/// empty line); more is refused, so blank lines can't stand in for a request.
const MAX_LEADING_BLANK: usize = 8;
/// The largest token file read.
const MAX_TOKEN_FILE: u64 = 1 << 20;

/// Bounds on what one client can make the server hold.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The largest request body.
    pub max_body: usize,
    /// The largest request line and headers together.
    pub max_head: usize,
    /// Connections served at once (each on its own thread); more are answered 503.
    pub max_connections: usize,
    /// Connections served at once from one peer address (503 past it). Behind a proxy every
    /// client shares the proxy's address.
    pub max_connections_per_peer: usize,
    /// Sessions open at once; past it, idle ones are closed first, then `initialize` gets 503.
    pub max_sessions: usize,
    /// How long a request's head may take to arrive: from the connection's opening for its first
    /// request, from the head's first byte for later ones; never extended. Its body has as long
    /// again from the end of the head.
    pub request_timeout: Duration,
    /// How long a kept-alive connection may sit idle between requests.
    pub keep_alive: Duration,
    /// How long one write to a client may block.
    pub write_timeout: Duration,
    /// How often a waiting response checks that its client is still there (and an event stream
    /// sends a comment to keep proxies from closing it).
    pub ping: Duration,
    /// A session with no request for this long is closed, its `GET` stream with it (the stream's
    /// pings are not activity).
    pub session_idle: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_body: 8 << 20,
            max_head: 16 << 10,
            max_connections: 64,
            max_connections_per_peer: 8,
            max_sessions: 32,
            request_timeout: Duration::from_secs(10),
            keep_alive: Duration::from_secs(15),
            write_timeout: Duration::from_secs(30),
            ping: Duration::from_secs(15),
            session_idle: Duration::from_secs(3600),
        }
    }
}

/// How to serve.
#[derive(Clone)]
pub struct Config {
    pub addr: SocketAddr,
    /// Bearer tokens, one per line (default: [`default_token_file`], created if missing). Every
    /// request must carry one unless [`Config::no_auth`].
    pub token_file: Option<PathBuf>,
    /// Serve without authentication: refused unless the bind is loopback. Any local user or
    /// process (and anything a reverse proxy on this host forwards) can then drive the devices.
    pub no_auth: bool,
    /// Acknowledges that a non-loopback bind serves plain HTTP: the token crosses the network in
    /// the clear. Not needed with [`Config::tls`].
    pub insecure_remote: bool,
    /// Serve HTTPS (TLS 1.3) instead of HTTP. Needs the `tls` cargo feature: without it
    /// [`start`] fails and says so.
    pub tls: Option<Tls>,
    pub toolsets: Toolsets,
    pub limits: Limits,
    pub timing: Timing,
}

impl Config {
    pub fn new(addr: SocketAddr) -> Config {
        Config {
            addr,
            token_file: None,
            no_auth: false,
            insecure_remote: false,
            tls: None,
            toolsets: Toolsets::default(),
            limits: Limits::default(),
            timing: Timing::default(),
        }
    }
}

/// Where a TLS server gets its certificate.
#[derive(Clone, Debug)]
pub enum Tls {
    /// A PEM certificate chain (leaf first) and its PEM private key.
    Files { cert: PathBuf, key: PathBuf },
    /// A self-signed certificate for development, made on first use and cached in `dir` (default:
    /// simee's state directory) in a file only its owner can read. It names `localhost`, the
    /// loopback addresses, the address the server binds when that is one address, and `names`. A
    /// client trusts it by pinning the SHA-256 fingerprint the server prints.
    SelfSigned { dir: Option<PathBuf>, names: Vec<String> },
}

/// simee's state directory: `$SIMEE_STATE_DIR`, else `~/Library/Application Support/simee` on
/// macOS and `$XDG_STATE_HOME/simee` (or `~/.local/state/simee`) elsewhere.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SIMEE_STATE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/simee"));
    }
    std::env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/state")))
        .map(|d| d.join("simee"))
}

/// The token file used when given none.
pub fn default_token_file() -> io::Result<PathBuf> {
    state_dir()
        .map(|d| d.join(TOKEN_FILE))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory: set HOME or SIMEE_STATE_DIR, or pass --token-file"))
}

/// Serves until the process ends.
pub fn serve(backend: Arc<dyn Backend>, config: Config) -> io::Result<()> {
    let running = start(backend, config)?;
    match running.tls_fingerprint() {
        Some(fingerprint) => {
            eprintln!("simee mcp: Streamable HTTP over TLS 1.3 on https://{}{ENDPOINT}", running.addr());
            if let Some(cert) = running.tls_certificate() {
                eprintln!("simee mcp: certificate {} (give clients this file, or pin the fingerprint below)", cert.display());
            }
            eprintln!("simee mcp: certificate SHA-256 fingerprint {fingerprint}");
        }
        None => eprintln!("simee mcp: Streamable HTTP on http://{}{ENDPOINT}", running.addr()),
    }
    match running.token_file() {
        Some(path) => eprintln!("simee mcp: bearer tokens from {} (send `Authorization: Bearer <token>`)", path.display()),
        None => eprintln!("simee mcp: no authentication (--no-auth): any local user or process can drive the devices"),
    }
    running.wait();
    Ok(())
}

/// Binds and starts serving on a thread of its own.
pub fn start(backend: Arc<dyn Backend>, config: Config) -> io::Result<Running> {
    let tokens = prepare_auth(&config, default_token_file)?;
    let tls = config.tls.as_ref().map(|tls| tls::Server::new(tls, config.addr.ip())).transpose()?;
    let listener = TcpListener::bind(config.addr)?;
    let addr = listener.local_addr()?;
    let router = Arc::new(Router::default());
    let stateless = Server::with_timing(backend.clone(), sink(router.clone(), None), config.timing, config.toolsets.clone());
    let shared = Arc::new(Shared {
        backend,
        loopback: addr.ip().is_loopback(),
        config,
        tokens,
        tls,
        router,
        stateless,
        sessions: Mutex::default(),
        connections: AtomicUsize::new(0),
        peers: Mutex::default(),
        stopping: AtomicBool::new(false),
    });
    let accepting = shared.clone();
    let accept = std::thread::Builder::new().name("simee-mcp-http".into()).spawn(move || accept_loop(&accepting, &listener))?;
    Ok(Running { addr, shared, accept: Some(accept) })
}

/// A server that is listening. Dropping it stops accepting connections (open ones finish).
pub struct Running {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl Running {
    /// The address bound (with the port the OS chose for port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Blocks while the server accepts connections.
    pub fn wait(mut self) {
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }

    /// How many sessions are open.
    pub fn sessions(&self) -> usize {
        lock(&self.shared.sessions).len()
    }

    /// The SHA-256 fingerprint of the certificate clients see, as `AB:CD:…` (`None` without TLS).
    pub fn tls_fingerprint(&self) -> Option<&str> {
        self.shared.tls.as_ref().map(tls::Server::fingerprint)
    }

    /// The PEM certificate file to give clients so they trust the server (`None` without TLS):
    /// the one named, or, for a self-signed certificate, a copy without its key.
    pub fn tls_certificate(&self) -> Option<&Path> {
        self.shared.tls.as_ref().map(tls::Server::certificate)
    }

    /// The token file requests are checked against (`None` with [`Config::no_auth`]).
    pub fn token_file(&self) -> Option<&Path> {
        self.shared.tokens.as_ref().map(|t| t.path.as_path())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(accept) = self.accept.take() else { return };
        self.shared.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop with a connection of our own.
        let wake = match self.addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.addr.port()),
            IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), self.addr.port()),
            _ => self.addr,
        };
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        let _ = accept.join();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- Authentication ----

/// Checks the bind against the policy and loads the token file (`default` when none is named).
fn prepare_auth(config: &Config, default: impl FnOnce() -> io::Result<PathBuf>) -> io::Result<Option<Tokens>> {
    let loopback = config.addr.ip().is_loopback();
    if !loopback && !config.insecure_remote && config.tls.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a loopback address: without TLS its bearer token would cross the network in the clear; \
                 serve HTTPS with --tls-cert and --tls-key (or --tls-self-signed for development), or pass \
                 --insecure-remote to serve plain HTTP anyway (on a trusted network or behind a TLS proxy)",
                config.addr
            ),
        ));
    }
    if config.no_auth {
        if !loopback {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--no-auth is accepted only on a loopback bind, and {} is not one", config.addr),
            ));
        }
        if config.token_file.is_some() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "--no-auth and --token-file contradict each other"));
        }
        return Ok(None);
    }
    let path = match &config.token_file {
        Some(path) => path.clone(),
        None => default()?,
    };
    ensure_token_file(&path)?;
    let tokens = Tokens::new(path);
    if lock(&tokens.state).tokens.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: no token in it", tokens.path.display())));
    }
    Ok(Some(tokens))
}

/// `n` random bytes from the OS, as hex.
fn random_hex(n: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The effective user, who must own the token file.
#[cfg(unix)]
fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Makes the token file's directory if it is missing (0700), and refuses one that another user
/// owns or that others can write: they could plant a token file of their own, or a link.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a directory", dir.display())));
        }
        if meta.uid() != euid() && meta.uid() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} belongs to another user (uid {}): keep the token file in a directory of your own", dir.display(), meta.uid()),
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "other users can write {} (mode {:o}): `chmod go-w` it, or keep the token file elsewhere",
                    dir.display(),
                    meta.mode() & 0o7777
                ),
            ));
        }
    }
    Ok(())
}

/// The directory a token file lives in: a bare relative name lives in the current directory.
fn token_dir(path: &Path) -> &Path {
    path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

/// Makes `path` hold a token, readable only by its owner: an existing file is checked (and
/// refused if others can read it, if it is a link, or if its directory is not private); a missing
/// one is written with a new random token.
pub fn ensure_token_file(path: &Path) -> io::Result<()> {
    // A bare relative name lives in the current directory, which is checked like any other.
    ensure_private_dir(token_dir(path))?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => return read_tokens(path).map(drop),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // `create_new` refuses anything already there, a link included.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(format!("{}\n", random_hex(32)?).as_bytes())?;
    eprintln!("simee mcp: wrote a new bearer token to {} (give it to clients as `Authorization: Bearer <token>`)", path.display());
    Ok(())
}

/// Opens a token file without following a link (and without waiting on a FIFO).
#[cfg(unix)]
fn open_token_file(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(std::fs::File::from(fd)),
        Err(_) if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symbolic link: name the token file itself", path.display()),
        )),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
fn open_token_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// The tokens in a file: one per line, blank lines and `#` comments skipped. The file must be a
/// regular file (not a link), owned by this user and readable by them only; those are checked on
/// the open file, so it can't be swapped between the check and the read.
fn read_tokens(path: &Path) -> io::Result<Vec<String>> {
    let file = open_token_file(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a regular file", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != euid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} belongs to another user (uid {}): a token file must be your own", path.display(), meta.uid()),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} can be read by other users (mode {:o}): `chmod 600` it", path.display(), meta.mode() & 0o777),
            ));
        }
    }
    if meta.len() > MAX_TOKEN_FILE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is larger than a token file", path.display())));
    }
    let mut text = String::new();
    file.take(MAX_TOKEN_FILE).read_to_string(&mut text)?;
    let tokens: Vec<String> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect();
    if let Some(short) = tokens.iter().find(|t| t.len() < MIN_TOKEN) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: a token is {} characters; use at least {MIN_TOKEN}", path.display(), short.len()),
        ));
    }
    Ok(tokens)
}

/// What a file looked like when it was read, to notice a rewrite.
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let inode = 0;
        Stamp { modified: meta.modified().ok(), len: meta.len(), inode }
    }
}

struct TokenState {
    stamp: Option<Stamp>,
    tokens: Vec<String>,
    /// The last error reading the file, said once.
    error: Option<String>,
}

/// The token file, re-read whenever it changes, so rotating a token is rewriting the file.
struct Tokens {
    path: PathBuf,
    state: Mutex<TokenState>,
}

impl Tokens {
    fn new(path: PathBuf) -> Tokens {
        let tokens = Tokens { path, state: Mutex::new(TokenState { stamp: None, tokens: Vec::new(), error: None }) };
        tokens.refresh(&mut lock(&tokens.state));
        tokens
    }

    fn refresh(&self, state: &mut TokenState) {
        let stamp = std::fs::symlink_metadata(&self.path).ok().map(|m| Stamp::of(&m));
        if stamp.is_some() && stamp == state.stamp {
            return;
        }
        state.tokens = match read_tokens(&self.path) {
            Ok(tokens) => {
                state.error = None;
                tokens
            }
            Err(e) => {
                let error = e.to_string();
                if state.error.as_ref() != Some(&error) {
                    eprintln!("simee mcp: {error}; refusing every request until it is fixed");
                    state.error = Some(error);
                }
                Vec::new()
            }
        };
        state.stamp = stamp;
    }

    /// The token an `Authorization` header carries, if it is one of the file's.
    fn check(&self, authorization: Option<&str>) -> Option<String> {
        let presented = authorization.and_then(bearer)?;
        let mut state = lock(&self.state);
        self.refresh(&mut state);
        // Every token is compared, each in time that depends only on its length.
        let found = state.tokens.iter().fold(false, |found, t| same(t.as_bytes(), presented.as_bytes()) | found);
        found.then(|| presented.to_owned())
    }
}

fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Origins and hosts ----

/// The host of an authority (`host`, `host:port`, `[v6]:port`).
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

fn loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether a browser's `Origin` may talk to this server: loopback pages only. (`null`, the origin
/// of sandboxed frames and files, isn't one.)
fn origin_allowed(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.trim().split_once("://") else { return false };
    let authority = rest.split('/').next().unwrap_or(rest);
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) && loopback_host(host_of(authority))
}

// ---- Routing what servers say back to the requests that asked ----

#[derive(Debug, PartialEq, Eq)]
enum Out {
    Response(String),
    Note(String),
}

/// A request waiting for its answer: its own id and progress token, and where its messages go.
struct Route {
    id: Value,
    token: Option<Value>,
    tx: Arc<NoteTx>,
}

/// How many notifications a stream's queue holds for a client that reads slowly: a request's
/// own (progress, what a `subscriptions/listen` announces) and a session's `GET` stream. Messages
/// for one resource are one (the latest), so this is how many different things can change before
/// the oldest is dropped: they are notifications that something changed, and the client reads the
/// state when it next looks.
const NOTE_QUEUE: usize = 256;

/// A queued notification.
struct Note {
    /// What it supersedes: a later one with the same key replaces it.
    key: Option<String>,
    line: String,
    /// Never dropped for room (a `subscriptions/listen`'s acknowledgement, which comes first).
    pinned: bool,
}

/// What a stream's queue holds: the notifications, oldest first, the response (a request's one
/// and last message, kept apart so that no notification can push it out), and whether its sender
/// is gone.
struct QueueState {
    notes: VecDeque<Note>,
    response: Option<String>,
    closed: bool,
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
    capacity: usize,
}

/// The sending end of a stream's queue. It never waits for the client, so a stalled client can't
/// hold up the server's watcher, and dropping it ends the stream.
struct NoteTx(Arc<Queue>);

/// The stream's end of the queue.
struct NoteRx(Arc<Queue>);

fn note_queue(capacity: usize) -> (NoteTx, NoteRx) {
    let queue = Arc::new(Queue {
        state: Mutex::new(QueueState { notes: VecDeque::new(), response: None, closed: false }),
        ready: Condvar::new(),
        capacity,
    });
    (NoteTx(queue.clone()), NoteRx(queue))
}

impl NoteTx {
    /// Queues `line`. A queued notification with the same `key` is dropped for it (only the latest
    /// state of a resource matters), and past the capacity the oldest unpinned one goes.
    #[cfg(test)]
    fn push(&self, key: Option<String>, line: String) {
        self.queue(Note { key, line, pinned: false });
    }

    /// Queues a notification as [`supersedes`] and [`pinned`] say.
    fn note(&self, message: &Value, line: String) {
        self.queue(Note { key: supersedes(message), line, pinned: pinned(message) });
    }

    fn queue(&self, note: Note) {
        let mut state = lock(&self.0.state);
        if let Some(key) = &note.key {
            state.notes.retain(|n| n.key.as_ref() != Some(key));
        }
        state.notes.push_back(note);
        while state.notes.len() > self.0.capacity {
            let Some(oldest) = state.notes.iter().position(|n| !n.pinned) else { break };
            state.notes.remove(oldest);
        }
        drop(state);
        self.0.ready.notify_one();
    }

    /// Queues a request's response, which is neither coalesced nor dropped, and comes after the
    /// notifications queued before it.
    fn respond(&self, line: String) {
        lock(&self.0.state).response = Some(line);
        self.0.ready.notify_one();
    }
}

impl Drop for NoteTx {
    fn drop(&mut self) {
        lock(&self.0.state).closed = true;
        self.0.ready.notify_one();
    }
}

impl NoteRx {
    /// The next message, waiting up to `timeout`; the queued ones are delivered before a close.
    fn recv_timeout(&self, timeout: Duration) -> Result<Out, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.0.state);
        loop {
            if let Some(note) = state.notes.pop_front() {
                return Ok(Out::Note(note.line));
            }
            if let Some(line) = state.response.take() {
                return Ok(Out::Response(line));
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            state = self.0.ready.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    #[cfg(test)]
    fn try_recv(&self) -> Option<String> {
        lock(&self.0.state).notes.pop_front().map(|n| n.line)
    }
}

/// What a notification supersedes in a stream's queue: an update of a resource replaces the one
/// before it, and so does a change of the list; the latest progress of a request is its progress,
/// and the latest state of a task its state.
fn supersedes(message: &Value) -> Option<String> {
    match message["method"].as_str()? {
        "notifications/resources/updated" => Some(format!("updated {}", message["params"]["uri"].as_str()?)),
        method @ ("notifications/resources/list_changed" | "notifications/progress") => Some(method.to_owned()),
        "notifications/tasks" => Some(format!("task {}", message["params"]["taskId"].as_str()?)),
        _ => None,
    }
}

/// Whether a notification must survive the queue's dropping the oldest: the acknowledgement that
/// opens a `subscriptions/listen` is what the client waits for first.
fn pinned(message: &Value) -> bool {
    message["method"] == "notifications/subscriptions/acknowledged"
}

/// A session's stream for messages that belong to no request, with the generation of the `GET`
/// that holds it.
type Standalone = Mutex<Option<(u64, NoteTx)>>;

#[derive(Default)]
struct Router {
    next: AtomicU64,
    /// By internal id (`h<n>`): the id, progress token and subscription id the server sees.
    routes: Mutex<HashMap<String, Route>>,
}

/// Where a response's id sits in its line, when it is one of ours: `(start, key, rest)`.
/// serde_json writes keys sorted (`id` first) or in insertion order (`jsonrpc`, then `id`).
fn fast_response(line: &str) -> Option<(usize, &str, &str)> {
    for prefix in ["{\"id\":\"", "{\"jsonrpc\":\"2.0\",\"id\":\""] {
        if let Some(after) = line.strip_prefix(prefix) {
            let end = after.find('"')?;
            let key = &after[..end];
            if key.len() > 1 && key.starts_with('h') && key[1..].bytes().all(|b| b.is_ascii_digit()) {
                return Some((prefix.len() - 1, key, &after[end + 1..]));
            }
        }
    }
    None
}

impl Router {
    fn key(&self) -> String {
        format!("h{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn insert(&self, key: String, route: Route) {
        lock(&self.routes).insert(key, route);
    }

    fn forget(&self, key: &str) {
        lock(&self.routes).remove(key);
    }

    /// Sends one line from a server to whoever it is for, with the client's own ids put back.
    fn deliver(&self, line: String, standalone: Option<&Standalone>) {
        // The common case, a result, without parsing what may be a large one.
        if let Some((start, key, rest)) = fast_response(&line) {
            let route = lock(&self.routes).remove(key);
            if let Some(route) = route {
                let id = route.id.to_string();
                let mut out = String::with_capacity(line.len() + id.len());
                out.push_str(&line[..start]);
                out.push_str(&id);
                out.push_str(rest);
                route.tx.respond(out);
            }
            return;
        }
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else { return };
        if message.get("method").is_none() {
            let key = message.get("id").and_then(Value::as_str).map(str::to_owned);
            let route = key.and_then(|k| lock(&self.routes).remove(&k));
            if let Some(route) = route {
                message["id"] = route.id;
                route.tx.respond(message.to_string());
            }
            return;
        }
        let params = message.get("params");
        let subscription =
            params.and_then(|p| p.get("_meta")).and_then(|m| m.get(META_SUBSCRIPTION)).and_then(Value::as_str).map(str::to_owned);
        if let Some(key) = subscription {
            let found = lock(&self.routes).get(&key).map(|r| (r.id.clone(), r.tx.clone()));
            if let Some((id, tx)) = found {
                message["params"]["_meta"][META_SUBSCRIPTION] = id;
                let line = message.to_string();
                tx.note(&message, line);
            }
            return;
        }
        if message["method"] == "notifications/progress" {
            let key = params.and_then(|p| p.get("progressToken")).and_then(Value::as_str).map(str::to_owned);
            let found = key.and_then(|k| lock(&self.routes).get(&k).and_then(|r| Some((r.token.clone()?, r.tx.clone()))));
            if let Some((token, tx)) = found {
                message["params"]["progressToken"] = token;
                let line = message.to_string();
                tx.note(&message, line);
            }
            return;
        }
        // Never wait for a client that has stopped reading: that would stall the server's
        // watcher, and its other subscriptions with it.
        if let Some(slot) = standalone
            && let Some((_, tx)) = lock(slot).as_ref()
        {
            tx.note(&message, line);
        }
    }
}

fn sink(router: Arc<Router>, standalone: Option<Arc<Standalone>>) -> Sink {
    Arc::new(move |line: String| router.deliver(line, standalone.as_deref()))
}

// ---- Sessions ----

struct Session {
    server: Server,
    standalone: Arc<Standalone>,
    /// When a request last named it. A `GET` stream's pings don't count.
    last_used: Mutex<Instant>,
    /// Requests running on it (not its `GET` stream).
    requests: AtomicUsize,
    /// The client's request ids in flight, to the internal ones, for `notifications/cancelled`.
    cancels: Mutex<HashMap<String, String>>,
    /// The token that opened it (`None` without authentication): only that token can use it.
    owner: Option<String>,
}

impl Session {
    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }

    /// No request running, and none for `idle`.
    fn expired(&self, idle: Duration) -> bool {
        self.requests.load(Ordering::SeqCst) == 0 && lock(&self.last_used).elapsed() >= idle
    }

    fn owned_by(&self, principal: Option<&str>) -> bool {
        match (self.owner.as_deref(), principal) {
            (None, None) => true,
            (Some(a), Some(b)) => same(a.as_bytes(), b.as_bytes()),
            _ => false,
        }
    }

    /// Ends its `GET` stream.
    fn end(&self) {
        *lock(&self.standalone) = None;
    }
}

/// Counts a request against a session while it runs.
struct InFlight(Arc<Session>);

impl InFlight {
    fn new(session: &Arc<Session>) -> InFlight {
        session.requests.fetch_add(1, Ordering::SeqCst);
        session.touch();
        InFlight(session.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.touch();
        self.0.requests.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Shared {
    backend: Arc<dyn Backend>,
    config: Config,
    /// Whether the bind is loopback, so `Host` must be too.
    loopback: bool,
    tokens: Option<Tokens>,
    /// The TLS configuration, when the server speaks TLS.
    tls: Option<tls::Server>,
    router: Arc<Router>,
    /// Serves the current revision's requests that name no session.
    stateless: Server,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    connections: AtomicUsize,
    /// Connections open per peer address.
    peers: Mutex<HashMap<IpAddr, usize>>,
    stopping: AtomicBool,
}

impl Shared {
    /// The session `id` names, if it is open, unexpired and `principal`'s (another token's session
    /// is as unknown as a closed one). Using it counts as activity.
    fn session(&self, id: &str, principal: Option<&str>) -> Option<Arc<Session>> {
        let mut sessions = lock(&self.sessions);
        let session = sessions.get(id)?.clone();
        if session.expired(self.config.limits.session_idle) {
            sessions.remove(id);
            session.end();
            return None;
        }
        if !session.owned_by(principal) {
            return None;
        }
        session.touch();
        Some(session)
    }

    /// Closes `session` if `id` still names it.
    fn close_session(&self, id: &str, session: &Arc<Session>) {
        let mut sessions = lock(&self.sessions);
        if sessions.get(id).is_some_and(|s| Arc::ptr_eq(s, session)) {
            sessions.remove(id);
        }
        session.end();
    }

    /// A new session for `principal`, if there is room: expired sessions are closed, then, if the
    /// table is still full, `principal`'s own least recently used one with no request running.
    /// Another token's sessions are never closed to make room.
    fn open_session(&self, principal: Option<&str>) -> io::Result<Option<(String, Arc<Session>)>> {
        let id = random_hex(16)?;
        let limits = &self.config.limits;
        let mut sessions = lock(&self.sessions);
        sessions.retain(|_, s| {
            let keep = !s.expired(limits.session_idle);
            if !keep {
                s.end();
            }
            keep
        });
        if sessions.len() >= limits.max_sessions {
            let idle = sessions
                .iter()
                .filter(|(_, s)| s.requests.load(Ordering::SeqCst) == 0 && s.owned_by(principal))
                .min_by_key(|(_, s)| *lock(&s.last_used))
                .map(|(k, _)| k.clone());
            match idle.and_then(|k| sessions.remove(&k)) {
                Some(evicted) => evicted.end(),
                None => return Ok(None),
            }
        }
        let standalone: Arc<Standalone> = Arc::default();
        let server = Server::with_timing(
            self.backend.clone(),
            sink(self.router.clone(), Some(standalone.clone())),
            self.config.timing,
            self.config.toolsets.clone(),
        );
        let session = Arc::new(Session {
            server,
            standalone,
            last_used: Mutex::new(Instant::now()),
            requests: AtomicUsize::new(0),
            cancels: Mutex::default(),
            owner: principal.map(str::to_owned),
        });
        sessions.insert(id.clone(), session.clone());
        Ok(Some((id, session)))
    }
}

// ---- Connections ----

/// Holds one of the connection slots, and one of its peer's.
struct Slot(Arc<Shared>, Option<IpAddr>);

impl Slot {
    /// Gives back the peer's share (the global slot stays held): the address cap bounds only
    /// connections that have not authenticated.
    fn release_peer(&mut self) {
        if let Some(peer) = self.1.take() {
            let mut peers = lock(&self.0.peers);
            if let Some(open) = peers.get_mut(&peer) {
                *open -= 1;
                if *open == 0 {
                    peers.remove(&peer);
                }
            }
        }
    }

    /// A slot, or why there is none.
    fn take(shared: &Arc<Shared>, peer: IpAddr) -> Result<Slot, &'static str> {
        let limits = shared.config.limits;
        let mut peers = lock(&shared.peers);
        let open = peers.get(&peer).copied().unwrap_or(0);
        if open >= limits.max_connections_per_peer {
            return Err("too many connections from this address; retry shortly");
        }
        let max = limits.max_connections;
        shared
            .connections
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < max).then_some(n + 1))
            .map_err(|_| "too many connections; retry shortly")?;
        peers.insert(peer, open + 1);
        let mut slot = Slot(shared.clone(), Some(peer));
        if shared.tokens.is_none() {
            // No authentication (loopback only): nothing to wait for.
            drop(peers);
            slot.release_peer();
        }
        Ok(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.release_peer();
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &TcpListener) {
    for stream in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                // Out of descriptors, say: don't spin.
                eprintln!("simee mcp: accept: {e}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let accepted = Instant::now();
        let peer = stream.peer_addr().map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |a| a.ip().to_canonical());
        let slot = match Slot::take(shared, peer) {
            Ok(slot) => slot,
            Err(why) => {
                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                let body = error_body(INVALID_REQUEST, why);
                let _ = write_response(&stream, 503, &[("Retry-After", "1".into())], body.as_bytes(), true);
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }
        };
        let spawned = std::thread::Builder::new().name("simee-mcp-conn".into()).spawn(move || {
            let mut slot = slot;
            let shared = slot.0.clone();
            connection(&shared, &mut slot, stream, accepted);
            drop(slot);
        });
        if let Err(e) = spawned {
            eprintln!("simee mcp: cannot start a connection thread: {e}");
        }
    }
}

/// A request's line and headers.
struct Head {
    method: String,
    path: String,
    http11: bool,
    /// Names lowercased, in order.
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Whether the client may take the connection back for another request.
    fn keep_alive(&self) -> bool {
        let connection = self.header("connection").map(str::to_ascii_lowercase).unwrap_or_default();
        if self.http11 {
            !connection.split(',').any(|t| t.trim() == "close")
        } else {
            connection.split(',').any(|t| t.trim() == "keep-alive")
        }
    }

    /// Whether the `Accept` header admits `mime` (no header admits everything).
    fn accepts(&self, mime: &str) -> bool {
        let Some(accept) = self.header("accept") else { return true };
        let (kind, _) = mime.split_once('/').unwrap_or((mime, ""));
        accept
            .split(',')
            .map(|a| a.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
            .any(|a| a == mime || a == "*/*" || a.strip_suffix("/*").is_some_and(|k| k == kind))
    }
}

enum HeadError {
    /// The client went away or stayed silent: nothing to answer.
    Gone,
    TooLarge,
    Malformed(&'static str),
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "the request took too long"))
}

/// When a request head must be in by.
#[derive(Clone, Copy)]
enum HeadDeadline {
    /// This instant, whenever its first byte comes (a connection's first request: from the accept).
    Fixed(Instant),
    /// `request_timeout` after its first byte, which must come by this instant (a kept-alive
    /// connection waiting for its next request).
    FirstByteBy(Instant),
}

fn read_head(reader: &mut BufReader<Conn>, when: HeadDeadline, limits: &Limits) -> Result<Head, HeadError> {
    let mut raw: Vec<u8> = Vec::with_capacity(512);
    let (mut deadline, mut started) = match when {
        HeadDeadline::Fixed(at) => (at, true),
        HeadDeadline::FirstByteBy(at) => (at, false),
    };
    let mut blank = 0;
    loop {
        reader.get_ref().set_deadline(deadline).map_err(|_| HeadError::Gone)?;
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(HeadError::Gone),
        };
        if buf.is_empty() {
            return Err(HeadError::Gone);
        }
        if !started {
            // The request has started, blank lines included: one deadline for all of its head,
            // never moved again.
            started = true;
            deadline = Instant::now() + limits.request_timeout;
        }
        let mut used = 0;
        let mut done = false;
        for &b in buf {
            used += 1;
            // A few blank lines before a request line are allowed and skipped.
            if raw.is_empty() && (b == b'\r' || b == b'\n') {
                blank += 1;
                if blank > MAX_LEADING_BLANK {
                    reader.consume(used);
                    return Err(HeadError::Malformed("too many blank lines before the request line"));
                }
                continue;
            }
            raw.push(b);
            if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
                done = true;
                break;
            }
        }
        reader.consume(used);
        if raw.len() > limits.max_head {
            return Err(HeadError::TooLarge);
        }
        if done {
            break;
        }
    }
    let text = std::str::from_utf8(&raw).map_err(|_| HeadError::Malformed("the request head is not UTF-8"))?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty());
    let line = lines.next().ok_or(HeadError::Malformed("no request line"))?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(HeadError::Malformed("a request line is `METHOD target HTTP/1.1`"));
    };
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(HeadError::Malformed("only HTTP/1.0 and HTTP/1.1 are served")),
    };
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { return Err(HeadError::Malformed("a header is `Name: value`")) };
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(HeadError::Malformed("a header name has no spaces"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        if headers.len() > 100 {
            return Err(HeadError::TooLarge);
        }
    }
    let path = target.split('?').next().unwrap_or(target).to_owned();
    Ok(Head { method: method.to_owned(), path, http11, headers })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// Writes a whole response in one write.
fn write_response(mut stream: impl Write, status: u16, headers: &[(&str, String)], body: &[u8], close: bool) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    if !body.is_empty() {
        out.push_str("Content-Type: application/json\r\n");
    }
    if status != 204 {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    if close {
        out.push_str("Connection: close\r\n");
    }
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    stream.write_all(&bytes)?;
    stream.flush()
}

fn error_body(code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}}).to_string()
}

/// An event stream on a response: chunked on HTTP/1.1 (so the connection survives it), to the
/// connection's end on 1.0.
struct Sse<'a> {
    stream: &'a Conn,
    chunked: bool,
}

impl<'a> Sse<'a> {
    fn start(stream: &'a Conn, http11: bool, headers: &[(&str, String)]) -> io::Result<Sse<'a>> {
        let mut out = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n");
        out.push_str(if http11 { "Transfer-Encoding: chunked\r\n" } else { "Connection: close\r\n" });
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let mut w = stream;
        w.write_all(out.as_bytes())?;
        w.flush()?;
        Ok(Sse { stream, chunked: http11 })
    }

    fn write(&self, text: &str) -> io::Result<()> {
        let mut w = self.stream;
        if self.chunked {
            let mut out = format!("{:x}\r\n", text.len()).into_bytes();
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\r\n");
            w.write_all(&out)?;
        } else {
            w.write_all(text.as_bytes())?;
        }
        w.flush()
    }

    /// One message. A JSON line has no newlines, so it is one `data` line.
    fn event(&self, line: &str) -> io::Result<()> {
        self.write(&format!("event: message\ndata: {line}\n\n"))
    }

    fn ping(&self) -> io::Result<()> {
        self.write(": ping\n\n")
    }

    fn end(&self) -> io::Result<()> {
        if self.chunked {
            let mut w = self.stream;
            w.write_all(b"0\r\n\r\n")?;
            w.flush()?;
        }
        Ok(())
    }
}

/// What the client's side of a connection looks like while its response is pending.
#[derive(PartialEq, Eq)]
enum Peer {
    Open,
    /// It sent FIN: done sending, maybe still reading (a half-close), maybe gone. Only a write
    /// can tell which.
    Finished,
    /// The connection is broken (reset).
    Broken,
}

fn connection(shared: &Arc<Shared>, slot: &mut Slot, stream: TcpStream, accepted: Instant) {
    let limits = shared.config.limits;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(limits.write_timeout));
    // The first request's head is due `request_timeout` after the accept, a TLS handshake included.
    let first = accepted + limits.request_timeout;
    let conn = match &shared.tls {
        None => Conn::plain(stream),
        Some(tls) => match tls.accept(stream, first) {
            Ok(conn) => conn,
            // The handshake failed and its connection is closed.
            Err(_) => return,
        },
    };
    let mut reader = BufReader::new(conn);
    let mut when = HeadDeadline::Fixed(first);
    loop {
        let head = match read_head(&mut reader, when, &limits) {
            Ok(head) => head,
            Err(HeadError::Gone) => break,
            Err(HeadError::TooLarge) => {
                let _ = write_response(
                    reader.get_ref(),
                    431,
                    &[],
                    error_body(INVALID_REQUEST, "the request head is too large").as_bytes(),
                    true,
                );
                break;
            }
            Err(HeadError::Malformed(why)) => {
                let _ = write_response(reader.get_ref(), 400, &[], error_body(INVALID_REQUEST, why).as_bytes(), true);
                break;
            }
        };
        let keep = Exchange { shared, slot, reader: &mut reader, head: &head, principal: None }.serve() && head.keep_alive();
        if !keep || shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        when = HeadDeadline::FirstByteBy(Instant::now() + limits.keep_alive);
    }
    reader.get_ref().close();
}

/// One request on a connection. Each method returns whether the connection can carry another.
struct Exchange<'a> {
    shared: &'a Arc<Shared>,
    slot: &'a mut Slot,
    reader: &'a mut BufReader<Conn>,
    head: &'a Head,
    /// The token the request carried (`None` without authentication).
    principal: Option<String>,
}

/// How a request says its body is framed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framing {
    /// No `Content-Length` or `Transfer-Encoding`.
    Unframed,
    Length(usize),
    Chunked,
}

/// A request's framing, strictly: `Content-Length` is ASCII digits only (repeats must agree),
/// `Transfer-Encoding` appears at most once and is exactly `chunked`, and never both. Anything
/// looser is how a request is smuggled past a proxy that reads it differently.
fn framing(head: &Head) -> Result<Framing, &'static str> {
    let mut codings = head.headers.iter().filter(|(n, _)| n == "transfer-encoding").map(|(_, v)| v.as_str());
    let chunked = match (codings.next(), codings.next()) {
        (None, _) => false,
        (Some(_), Some(_)) => return Err("more than one Transfer-Encoding header"),
        (Some(te), None) if te.eq_ignore_ascii_case("chunked") => true,
        (Some(_), None) => return Err("the only transfer coding understood is `chunked`, alone"),
    };
    let mut lengths = head.headers.iter().filter(|(n, _)| n == "content-length").map(|(_, v)| v.as_str());
    let Some(first) = lengths.next() else {
        return Ok(if chunked { Framing::Chunked } else { Framing::Unframed });
    };
    if chunked {
        return Err("conflicting Content-Length and Transfer-Encoding");
    }
    if lengths.any(|v| v != first) {
        return Err("conflicting Content-Length headers");
    }
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) {
        return Err("a Content-Length is ASCII digits");
    }
    first.parse().map(Framing::Length).map_err(|_| "the Content-Length is too large")
}

impl Exchange<'_> {
    fn stream(&self) -> &Conn {
        self.reader.get_ref()
    }

    /// Answers with an error and closes the connection: after an error nothing that follows on it
    /// is trusted to start a request (an unread or half-read body, a framing a proxy may read
    /// differently).
    fn fail(&self, status: u16, code: i64, message: &str, headers: &[(&str, String)]) -> bool {
        let _ = write_response(self.stream(), status, headers, error_body(code, message).as_bytes(), true);
        false
    }

    fn serve(mut self) -> bool {
        let head = self.head;
        let framing = match framing(head) {
            Ok(framing) => framing,
            Err(why) => return self.fail(400, INVALID_REQUEST, why, &[]),
        };
        if head.method != "POST" && !matches!(framing, Framing::Unframed | Framing::Length(0)) {
            // A body no method here reads: never let it be taken for the next request.
            return self.fail(400, INVALID_REQUEST, &format!("a {} request carries no body here", head.method), &[]);
        }
        if head.path != ENDPOINT {
            return self.fail(404, INVALID_REQUEST, &format!("the MCP endpoint is {ENDPOINT}"), &[]);
        }
        if self.shared.loopback && head.header("host").is_some_and(|h| !loopback_host(host_of(h))) {
            return self.fail(403, INVALID_REQUEST, "this server is bound to loopback and answers only loopback host names", &[]);
        }
        if head.header("origin").is_some_and(|o| !origin_allowed(o)) {
            return self.fail(403, INVALID_REQUEST, "requests from this origin are refused (only loopback origins are served)", &[]);
        }
        if let Some(tokens) = &self.shared.tokens {
            match tokens.check(head.header("authorization")) {
                Some(token) => {
                    self.principal = Some(token);
                    self.slot.release_peer();
                }
                None => {
                    let challenge = [("WWW-Authenticate", "Bearer realm=\"simee\"".to_owned())];
                    return self.fail(401, INVALID_REQUEST, "a valid bearer token is required", &challenge);
                }
            }
        }
        match head.method.as_str() {
            "POST" => self.post(framing),
            "GET" => self.get(),
            "DELETE" => self.delete(),
            _ => self.fail(405, INVALID_REQUEST, "the MCP endpoint takes POST, GET and DELETE", &[("Allow", "GET, POST, DELETE".into())]),
        }
    }

    /// Reads the body, within the size and time limits. `Err` once the error is answered (the
    /// connection is then closed: an unread body can't be skipped safely).
    fn body(&mut self, framing: Framing) -> Result<Vec<u8>, ()> {
        let limits = self.shared.config.limits;
        let deadline = Instant::now() + limits.request_timeout;
        let too_large = format!("the body is larger than {} bytes", limits.max_body);
        let length = match framing {
            Framing::Unframed => {
                self.fail(411, INVALID_REQUEST, "a POST needs a Content-Length", &[]);
                return Err(());
            }
            Framing::Length(n) => Some(n),
            Framing::Chunked => None,
        };
        if length.is_some_and(|n| n > limits.max_body) {
            self.fail(413, INVALID_REQUEST, &too_large, &[]);
            return Err(());
        }
        if self.head.header("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
            let mut w = self.stream();
            if w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                return Err(());
            }
        }
        let read = match length {
            Some(n) => read_exact(self.reader, n, deadline),
            None => read_chunked(self.reader, limits.max_body, deadline),
        };
        match read {
            Ok(body) => Ok(body),
            Err(e) if e.kind() == io::ErrorKind::FileTooLarge => {
                self.fail(413, INVALID_REQUEST, &too_large, &[]);
                Err(())
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                self.fail(400, INVALID_REQUEST, &format!("a malformed chunked body: {e}"), &[]);
                Err(())
            }
            // Too slow, or gone.
            Err(_) => Err(()),
        }
    }

    fn post(mut self, framing: Framing) -> bool {
        let head = self.head;
        if !head.accepts("application/json") && !head.accepts("text/event-stream") {
            return self.fail(406, INVALID_REQUEST, "the client must accept application/json or text/event-stream", &[]);
        }
        if head.header("content-type").is_some_and(|t| !t.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json")) {
            return self.fail(415, INVALID_REQUEST, "the body is application/json", &[]);
        }
        let Ok(body) = self.body(framing) else { return false };
        let Ok(text) = std::str::from_utf8(&body) else {
            return self.fail(400, PARSE_ERROR, "the body is not UTF-8", &[]);
        };
        let principal = self.principal.as_deref();
        let incoming = match rpc::parse(text) {
            Ok(incoming) => incoming,
            Err(error) => {
                // A response to the server (it sends no requests, so nothing waits for it).
                let response = serde_json::from_str::<Value>(text).is_ok_and(|v| {
                    v.get("method").is_none() && v.get("id").is_some() && (v.get("result").is_some() || v.get("error").is_some())
                });
                if response {
                    return write_response(self.stream(), 202, &[], b"", false).is_ok();
                }
                let body = json!({"jsonrpc": "2.0", "id": null, "error": error.to_json()}).to_string();
                let _ = write_response(self.stream(), 400, &[], body.as_bytes(), true);
                return false;
            }
        };
        let (method, params) = match &incoming {
            Incoming::Request { method, params, .. } | Incoming::Notification { method, params } => (method.as_str(), params),
        };
        if let Some(version) = head.header(VERSION_HEADER) {
            if version != MODERN && !LEGACY.contains(&version) {
                let body = json!({"jsonrpc": "2.0", "id": null, "error": rpc::unsupported_version(version).to_json()}).to_string();
                let _ = write_response(self.stream(), 400, &[], body.as_bytes(), true);
                return false;
            }
            if let Some(named) = rpc::requested_version(params).filter(|v| *v != version) {
                let message = format!("MCP-Protocol-Version is {version} but the request's _meta names {named}");
                return self.fail(400, INVALID_REQUEST, &message, &[]);
            }
        }
        let is_request = matches!(incoming, Incoming::Request { .. });
        let mut created = None;
        let session = match head.header(SESSION_HEADER) {
            Some(id) => match self.shared.session(id, principal) {
                Some(session) => Some(session),
                None => {
                    return self.fail(404, INVALID_REQUEST, "no such session (it ended or expired): open a new one with initialize", &[]);
                }
            },
            None if is_request && method == "initialize" => match self.shared.open_session(principal) {
                Ok(Some((id, session))) => {
                    created = Some(id);
                    Some(session)
                }
                Ok(None) => return self.fail(503, INVALID_REQUEST, "too many sessions are open", &[("Retry-After", "5".into())]),
                Err(e) => return self.fail(500, rpc::INTERNAL_ERROR, &format!("cannot make a session id: {e}"), &[]),
            },
            None if !is_request || method == "server/discover" || method == "ping" || rpc::requested_version(params).is_some() => None,
            None => {
                let message = format!(
                    "`{method}` needs an Mcp-Session-Id header: open a session with initialize, or name the revision in \
                     _meta[\"{}\"] (\"{MODERN}\") to be served without one",
                    rpc::META_VERSION
                );
                return self.fail(400, INVALID_REQUEST, &message, &[]);
            }
        };
        let server = session.as_ref().map_or(&self.shared.stateless, |s| &s.server).clone();
        match incoming {
            Incoming::Notification { method, mut params } => {
                if method == "notifications/cancelled" {
                    // Only a session's own requests can be named; a stateless client cancels a
                    // request by closing its response.
                    let internal = session.as_ref().and_then(|s| {
                        let key = rpc::id_key(params.get("requestId")?);
                        lock(&s.cancels).get(&key).cloned()
                    });
                    let Some(internal) = internal else { return write_response(self.stream(), 202, &[], b"", false).is_ok() };
                    params["requestId"] = internal.into();
                }
                server.handle_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
                write_response(self.stream(), 202, &[], b"", false).is_ok()
            }
            Incoming::Request { id, method, mut params } => {
                let _running = session.as_ref().map(InFlight::new);
                let key = self.shared.router.key();
                let token =
                    params.get_mut("_meta").and_then(|m| m.get_mut("progressToken")).map(|t| std::mem::replace(t, key.clone().into()));
                let (tx, rx) = note_queue(NOTE_QUEUE);
                let client_key = rpc::id_key(&id);
                self.shared.router.insert(key.clone(), Route { id, token, tx: Arc::new(tx) });
                if let Some(s) = &session {
                    lock(&s.cancels).insert(client_key.clone(), key.clone());
                }
                server.handle_line(&json!({"jsonrpc": "2.0", "id": key, "method": method, "params": params}).to_string());
                let headers: Vec<(&str, String)> = created.into_iter().map(|id| ("Mcp-Session-Id", id)).collect();
                let keep = self.answer(&rx, &server, &key, &headers);
                self.shared.router.forget(&key);
                if let Some(s) = &session {
                    let mut cancels = lock(&s.cancels);
                    if cancels.get(&client_key) == Some(&key) {
                        cancels.remove(&client_key);
                    }
                }
                keep
            }
        }
    }

    /// Waits for a request's response and writes it: as JSON, or as an event stream once a
    /// notification for it comes first. A client that has gone (a write to it fails) cancels the
    /// request. One that only finished sending (a half-close) is still reading: if it takes an
    /// event stream, the response becomes one so its pings can tell; if not, it waits.
    fn answer(&self, rx: &NoteRx, server: &Server, key: &str, headers: &[(&str, String)]) -> bool {
        let stream = self.stream();
        let streams = self.head.accepts("text/event-stream");
        let cancel = || {
            server.handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": key}}).to_string())
        };
        let mut sse: Option<Sse<'_>> = None;
        loop {
            match rx.recv_timeout(self.shared.config.limits.ping) {
                Ok(Out::Response(line)) => {
                    return match &sse {
                        None => write_response(stream, 200, headers, line.as_bytes(), false).is_ok(),
                        Some(sse) => sse.event(&line).and_then(|()| sse.end()).is_ok() && sse.chunked,
                    };
                }
                Ok(Out::Note(line)) => {
                    if !streams {
                        continue;
                    }
                    if sse.is_none() {
                        match Sse::start(stream, self.head.http11, headers) {
                            Ok(started) => sse = Some(started),
                            Err(_) => {
                                cancel();
                                return false;
                            }
                        }
                    }
                    if sse.as_ref().is_some_and(|s| s.event(&line).is_err()) {
                        cancel();
                        return false;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if sse.is_none() {
                        match stream.peer_state() {
                            Peer::Open => continue,
                            Peer::Broken => {
                                cancel();
                                return false;
                            }
                            Peer::Finished if !streams => continue,
                            Peer::Finished => match Sse::start(stream, self.head.http11, headers) {
                                Ok(started) => sse = Some(started),
                                Err(_) => {
                                    cancel();
                                    return false;
                                }
                            },
                        }
                    }
                    if sse.as_ref().is_some_and(|s| s.ping().is_err()) {
                        cancel();
                        return false;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if sse.is_none() {
                        self.fail(500, rpc::INTERNAL_ERROR, "the request was dropped", &[]);
                    }
                    return false;
                }
            }
        }
    }

    /// A session's stream of what belongs to no request, until the client closes it, the session
    /// ends or expires (the stream's own pings are not activity), or another `GET` takes its place.
    fn get(self) -> bool {
        if !self.head.accepts("text/event-stream") {
            return self.fail(406, INVALID_REQUEST, "a GET stream is text/event-stream", &[]);
        }
        let Some(id) = self.head.header(SESSION_HEADER) else {
            let message = "a GET stream belongs to a session (open one with initialize); the current revision's \
                           notifications come on the POST of their subscriptions/listen";
            return self.fail(405, INVALID_REQUEST, message, &[("Allow", "POST, DELETE".into())]);
        };
        let Some(session) = self.shared.session(id, self.principal.as_deref()) else {
            return self.fail(404, INVALID_REQUEST, "no such session (it ended or expired)", &[]);
        };
        let idle = self.shared.config.limits.session_idle;
        let (tx, rx) = note_queue(NOTE_QUEUE);
        let generation = self.shared.router.next.fetch_add(1, Ordering::Relaxed);
        *lock(&session.standalone) = Some((generation, tx));
        let ended = match Sse::start(self.stream(), self.head.http11, &[]) {
            Ok(sse) => loop {
                match rx.recv_timeout(self.shared.config.limits.ping) {
                    Ok(Out::Note(line) | Out::Response(line)) => {
                        if sse.event(&line).is_err() {
                            break false;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if session.expired(idle) {
                            self.shared.close_session(id, &session);
                            break sse.end().is_ok() && sse.chunked;
                        }
                        if sse.ping().is_err() {
                            break false;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break sse.end().is_ok() && sse.chunked,
                }
            },
            Err(_) => false,
        };
        let mut slot = lock(&session.standalone);
        if slot.as_ref().is_some_and(|(g, _)| *g == generation) {
            *slot = None;
        }
        ended
    }

    fn delete(self) -> bool {
        let Some(id) = self.head.header(SESSION_HEADER) else {
            return self.fail(400, INVALID_REQUEST, "DELETE ends the session its Mcp-Session-Id names", &[]);
        };
        // Only the token that opened a session can end it.
        let Some(session) = self.shared.session(id, self.principal.as_deref()) else {
            return self.fail(404, INVALID_REQUEST, "no such session (it ended or expired)", &[]);
        };
        // Its GET stream ends; requests still running finish on their own.
        self.shared.close_session(id, &session);
        write_response(self.stream(), 204, &[], b"", false).is_ok()
    }
}

fn read_exact(reader: &mut BufReader<Conn>, n: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut body = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        reader.get_ref().set_deadline(deadline)?;
        match reader.read(&mut body[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(k) => filled += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(body)
}

/// One CRLF-terminated line of at most `max` bytes.
fn read_line(reader: &mut BufReader<Conn>, max: usize, deadline: Instant) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        reader.get_ref().set_deadline(deadline)?;
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        if line.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a chunk line is too long"));
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a chunk line is not UTF-8"))
}

/// A chunked body of at most `max` bytes (`FileTooLarge` past it).
fn read_chunked(reader: &mut BufReader<Conn>, max: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, 1024, deadline)?;
        // Hex digits only: no sign, no `0x`, no spaces (then any `;extension`).
        let digits = line.split(';').next().unwrap_or("");
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "a chunk size is hex digits");
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let size = usize::from_str_radix(digits, 16).map_err(|_| bad())?;
        if size == 0 {
            // Trailers, up to the blank line.
            while !read_line(reader, 8 << 10, deadline)?.is_empty() {}
            return Ok(body);
        }
        if body.len().saturating_add(size) > max {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        body.extend(read_exact(reader, size, deadline)?);
        if !read_line(reader, 1, deadline)?.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a chunk is longer than its size"));
        }
    }
}

#[cfg(test)]
mod tests;
