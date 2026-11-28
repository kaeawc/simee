//! A fake WHIP server on `std::net`: it records every request and answers through a handler. The
//! WHIP flavour answers offers with a str0m receiver of its own on a loopback UDP socket, hands
//! the access units it receives to the test, and sends PLIs when asked.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use str0m::change::SdpOffer;
use str0m::media::{KeyframeRequestKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

pub const BASELINE: &[u8] = include_bytes!("../fixtures/testsrc-baseline.h264");
pub const MAIN: &[u8] = include_bytes!("../fixtures/testsrc-main.h264");

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn new(status: u16) -> Reply {
        Reply { status, ..Reply::default() }
    }

    pub fn header(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: &str) -> Reply {
        self.body = body.into();
        self
    }
}

type Handler = Box<dyn Fn(&Request) -> Reply + Send + Sync>;

pub struct FakeServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeServer {
    pub fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handler: Arc<Handler> = Arc::new(Box::new(handler));
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let recorded = recorded.clone();
                let handler = handler.clone();
                thread::spawn(move || serve(conn, &recorded, &handler));
            }
        });
        FakeServer { addr, requests }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock").clone()
    }

    /// Wait for a request with this method.
    pub fn wait_for(&self, method: &str, timeout: Duration) -> Option<Request> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(r) = self.requests().into_iter().find(|r| r.method == method) {
                return Some(r);
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

fn serve(mut conn: TcpStream, recorded: &Mutex<Vec<Request>>, handler: &Handler) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let path = first.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned())).collect();
    let len: usize = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let req = Request { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() };
    recorded.lock().expect("lock").push(req.clone());
    let reply = handler(&req);
    let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", reply.status, reply.body.len());
    for (n, v) in &reply.headers {
        out.push_str(&format!("{n}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&reply.body);
    let _ = conn.write_all(out.as_bytes());
}

/// What the fake WHIP server's receiver saw.
pub struct Viewer {
    /// Each received access unit, Annex B, as str0m's depacketizer assembled it.
    pub frames: Receiver<Vec<u8>>,
    /// Ask the receiver to send a PLI.
    pub pli: Sender<()>,
}

/// A WHIP server whose POST handler answers with a str0m receiver.
pub fn whip_server() -> (FakeServer, Receiver<Viewer>) {
    let (viewers_tx, viewers) = mpsc::channel();
    let viewers_tx = Mutex::new(viewers_tx);
    let sessions = AtomicUsize::new(0);
    let server = FakeServer::start(move |req| match req.method.as_str() {
        "POST" => {
            if req.header("content-type") != Some("application/sdp") {
                return Reply::new(415);
            }
            let Ok(offer) = SdpOffer::from_sdp_string(&req.body) else {
                return Reply::new(400).body("bad offer");
            };
            let (answer, viewer) = receiver(offer);
            let _ = viewers_tx.lock().expect("lock").send(viewer);
            let n = sessions.fetch_add(1, Ordering::SeqCst) + 1;
            Reply::new(201)
                .header("Content-Type", "application/sdp")
                .header("Location", &format!("/whip/session/{n}"))
                .header("ETag", "\"1\"")
                .body(&answer)
        }
        "PATCH" => Reply::new(204).header("ETag", "\"2\""),
        "DELETE" => Reply::new(200),
        _ => Reply::new(405),
    });
    (server, viewers)
}

pub fn receiver(offer: SdpOffer) -> (String, Viewer) {
    let provider = Arc::new(str0m::crypto::from_feature_flags());
    let mut rtc = RtcConfig::new().set_crypto_provider(provider).build(Instant::now());
    let socket = UdpSocket::bind("127.0.0.1:0").expect("udp");
    let local = socket.local_addr().expect("addr");
    rtc.add_local_candidate(Candidate::host(local, "udp").expect("candidate"));
    let answer = rtc.sdp_api().accept_offer(offer).expect("answer");
    let (frames_tx, frames) = mpsc::channel();
    let (pli, pli_rx) = mpsc::channel();
    thread::spawn(move || run_receiver(rtc, socket, local, &frames_tx, &pli_rx));
    (answer.to_sdp_string(), Viewer { frames, pli })
}

fn run_receiver(mut rtc: Rtc, socket: UdpSocket, local: SocketAddr, frames: &Sender<Vec<u8>>, pli: &Receiver<()>) {
    let mut buf = vec![0u8; 2048];
    let mut mid: Option<Mid> = None;
    let started = Instant::now();
    while rtc.is_alive() && started.elapsed() < Duration::from_secs(60) {
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(Event::MediaAdded(m))) => mid = Some(m.mid),
                Ok(Output::Event(Event::MediaData(d))) => {
                    if frames.send(d.data.to_vec()).is_err() {
                        return;
                    }
                }
                Ok(Output::Event(_)) => {}
                Err(_) => return,
            }
        };
        if pli.try_recv().is_ok()
            && let Some(mid) = mid
            && let Some(mut writer) = rtc.writer(mid)
        {
            let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
            continue;
        }
        let wait = timeout.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), Duration::from_millis(10));
        let _ = socket.set_read_timeout(Some(wait));
        let input = match socket.recv_from(&mut buf) {
            Ok((n, source)) => match buf[..n].try_into() {
                Ok(contents) => Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination: local, contents }),
                Err(_) => continue,
            },
            Err(_) => Input::Timeout(Instant::now()),
        };
        if rtc.handle_input(input).is_err() {
            return;
        }
    }
}

/// Print why a MediaMTX test is skipped, or fail it when `SIMEE_REQUIRE_MEDIAMTX` is set.
pub fn skip(reason: &str) {
    assert!(std::env::var_os("SIMEE_REQUIRE_MEDIAMTX").is_none(), "SIMEE_REQUIRE_MEDIAMTX is set, but: {reason}");
    eprintln!("SKIP: {reason}");
}

/// The MediaMTX test config; the harness starts the server on free ports through MTX_* overrides.
const MEDIAMTX_CONFIG: &str = include_str!("../../../../tests/mediamtx/mediamtx.yml");

/// A MediaMTX child process on free loopback ports with the test config. It is killed (by the PID
/// this struct recorded) and its temp dir removed on drop; when the test is panicking, the drop
/// prints the server's log first.
pub struct MediaMtx {
    pub webrtc_port: u16,
    pub api_port: u16,
    child: Child,
    dir: PathBuf,
}

static MEDIAMTX_RUNS: AtomicUsize = AtomicUsize::new(0);

impl MediaMtx {
    /// The binary to run: `$MEDIAMTX_BIN` (which must exist: a path that doesn't is a mistake,
    /// not a reason to skip), else `mediamtx` on PATH. Nothing is downloaded.
    pub fn find() -> Option<PathBuf> {
        if let Some(bin) = std::env::var_os("MEDIAMTX_BIN") {
            let bin = PathBuf::from(bin);
            assert!(bin.is_file(), "MEDIAMTX_BIN={} is not a file", bin.display());
            return Some(bin);
        }
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join("mediamtx")).find(|p| p.is_file())
    }

    /// `find`, or print why the test is skipped. With `SIMEE_REQUIRE_MEDIAMTX` set (CI sets it) a
    /// missing binary fails the test instead, so a broken download step can't go green.
    pub fn find_or_skip() -> Option<PathBuf> {
        let bin = Self::find();
        if bin.is_none() {
            skip("no MediaMTX (set MEDIAMTX_BIN or put mediamtx on PATH; see docs/streaming.md)");
        }
        bin
    }

    /// Start it and wait until its API answers and its WebRTC listener accepts; `Err` carries the
    /// log when it dies or stalls. The ports are picked free and released before MediaMTX binds
    /// them (nextest runs each test in its own process, so another test can take one in between):
    /// a start that dies on "address already in use" is tried again on new ports.
    pub fn start(bin: &Path) -> Result<MediaMtx, String> {
        let mut last = String::new();
        for _ in 0..3 {
            match Self::start_once(bin) {
                Err(e) if e.contains("address already in use") => last = e,
                other => return other,
            }
        }
        Err(last)
    }

    fn start_once(bin: &Path) -> Result<MediaMtx, String> {
        let dir =
            std::env::temp_dir().join(format!("simee-mediamtx-{}-{}", std::process::id(), MEDIAMTX_RUNS.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).map_err(|e| format!("temp dir: {e}"))?;
        let config = dir.join("mediamtx.yml");
        std::fs::write(&config, MEDIAMTX_CONFIG).map_err(|e| format!("config: {e}"))?;
        let log = std::fs::File::create(dir.join("mediamtx.log")).map_err(|e| format!("log: {e}"))?;
        let log2 = log.try_clone().map_err(|e| format!("log: {e}"))?;
        let [webrtc_port, api_port, media_port] = free_ports();
        let child = Command::new(bin)
            .arg(&config)
            .env("MTX_WEBRTCADDRESS", format!("127.0.0.1:{webrtc_port}"))
            .env("MTX_APIADDRESS", format!("127.0.0.1:{api_port}"))
            .env("MTX_WEBRTCLOCALUDPADDRESS", format!("127.0.0.1:{media_port}"))
            .env("MTX_WEBRTCLOCALTCPADDRESS", format!("127.0.0.1:{media_port}"))
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log2)
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        let mut mtx = MediaMtx { webrtc_port, api_port, child, dir };
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = mtx.child.try_wait() {
                return Err(format!("mediamtx exited during startup ({status}):\n{}", mtx.log()));
            }
            // MediaMTX starts its WebRTC server before its API, but the tests publish to the
            // former: check both rather than lean on that order.
            if matches!(mtx.api_get("/v3/config/global/get"), Some((200, _))) && mtx.webrtc_listening() {
                return Ok(mtx);
            }
            if Instant::now() > end {
                return Err(format!("mediamtx API not ready after 15s:\n{}", mtx.log()));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Whether the WebRTC (WHIP/WHEP) HTTP listener accepts connections.
    fn webrtc_listening(&self) -> bool {
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.webrtc_port)), Duration::from_secs(2)).is_ok()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The WHIP endpoint for a stream name.
    pub fn whip_url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/{name}/whip", self.webrtc_port)
    }

    /// The server's log so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("mediamtx.log")).unwrap_or_default()
    }

    /// GET on the control API: status and body, `None` when nothing answers.
    pub fn api_get(&self, path: &str) -> Option<(u16, String)> {
        let mut conn = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.api_port)), Duration::from_secs(2)).ok()?;
        conn.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        conn.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
        let mut raw = Vec::new();
        conn.read_to_end(&mut raw).ok()?;
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n")?;
        let status = head.split(' ').nth(1)?.parse().ok()?;
        Some((status, body.to_owned()))
    }

    /// The API's description of a path (`GET /v3/paths/get/<name>`, v1.19.2: `name`, `ready`,
    /// `source {type, id}`, `tracks`, `tracks2 [{codec, codecProps {width, height, profile,
    /// level}}]`, `inboundBytes`, ...), whitespace stripped so `"ready":true` matches, once the
    /// path exists and is ready (a publisher is connected and its tracks are known). An unknown
    /// path is a 404 with `{"status":"error","error":"path not found"}`.
    pub fn ready_path(&self, name: &str) -> Option<String> {
        let (status, body) = self.api_get(&format!("/v3/paths/get/{name}"))?;
        let body: String = body.split_whitespace().collect();
        (status == 200 && body.contains("\"ready\":true")).then_some(body)
    }

    /// Poll `ready_path` until `timeout`.
    pub fn wait_ready_path(&self, name: &str, timeout: Duration) -> Option<String> {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Some(body) = self.ready_path(name) {
                return Some(body);
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for MediaMtx {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("--- mediamtx log ({}) ---\n{}\n--- end mediamtx log ---", self.dir.display(), self.log());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Three ports nothing listens on right now (TCP, held until all three are chosen).
fn free_ports() -> [u16; 3] {
    let listeners: Vec<TcpListener> = (0..3).map(|_| TcpListener::bind("127.0.0.1:0").expect("bind")).collect();
    let port = |i: usize| listeners[i].local_addr().expect("addr").port();
    [port(0), port(1), port(2)]
}
