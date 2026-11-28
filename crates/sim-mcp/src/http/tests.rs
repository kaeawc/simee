//! The transport over real loopback sockets, against a fake backend: a small HTTP client sends
//! requests and reads JSON responses and event streams.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use super::*;
use crate::backend::fake::Fake;
use crate::rpc::{EXT_TASKS, META_CLIENT_CAPS, META_VERSION};

#[cfg(feature = "tls")]
mod https;

const WAIT: Duration = Duration::from_secs(10);

fn limits() -> Limits {
    Limits { max_body: 64 << 10, max_connections: 16, ping: Duration::from_millis(100), ..Limits::default() }
}

fn timing() -> Timing {
    Timing { watch: crate::watch::Pace::uniform(Duration::from_millis(20)), heartbeat: Duration::from_millis(40) }
}

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("simee-mcp-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    running: Running,
    fake: Arc<Fake>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(|_| {})
    }

    fn with(adjust: impl FnOnce(&mut Config)) -> Rig {
        let fake = Arc::new(Fake::with_devices());
        let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
        config.toolsets = Toolsets::all();
        config.limits = limits();
        // Authentication is on by default, from a file in the state directory: tests that want it
        // name a file of their own.
        config.no_auth = true;
        config.timing = timing();
        adjust(&mut config);
        let running = start(fake.clone(), config).expect("the server starts");
        Rig { running, fake }
    }

    fn client(&self) -> Client {
        Client::connect(self.running.addr())
    }
}

/// One response: status, headers (names lowercased) and the whole body.
#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("JSON body ({e}): {self:?}"))
    }
}

fn events_of(text: &str) -> Vec<Value> {
    text.split("\n\n")
        .filter_map(|event| event.lines().find_map(|l| l.strip_prefix("data: ")))
        .map(|data| serde_json::from_str(data).expect("an event's data is JSON"))
        .collect()
}

/// Sends raw bytes on a new connection and returns everything the server writes until it closes
/// the connection (or stays silent for `quiet`).
fn raw_exchange(addr: SocketAddr, bytes: &[u8], quiet: Duration) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(bytes).expect("write");
    stream.set_read_timeout(Some(quiet)).expect("timeout");
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How a client reaches the server over TLS: its configuration and the name it expects (none:
/// plain HTTP).
#[cfg(feature = "tls")]
type Trust = Option<(Arc<rustls::ClientConfig>, rustls::pki_types::ServerName<'static>)>;
#[cfg(not(feature = "tls"))]
type Trust = Option<std::convert::Infallible>;

/// A client's end of a connection: the socket, or a TLS session over it.
enum Wire {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Wire {
    fn open(addr: SocketAddr, trust: &Trust) -> Wire {
        let tcp = TcpStream::connect(addr).expect("connect");
        tcp.set_read_timeout(Some(WAIT)).expect("timeout");
        tcp.set_nodelay(true).expect("nodelay");
        #[cfg(feature = "tls")]
        if let Some((config, name)) = trust {
            let conn = rustls::ClientConnection::new(config.clone(), name.clone()).expect("a client session");
            return Wire::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)));
        }
        #[cfg(not(feature = "tls"))]
        let _ = trust;
        Wire::Plain(tcp)
    }

    /// The socket underneath (for TLS, the one the records travel on).
    fn tcp(&self) -> &TcpStream {
        match self {
            Wire::Plain(tcp) => tcp,
            #[cfg(feature = "tls")]
            Wire::Tls(tls) => tls.get_ref(),
        }
    }
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(tcp) => tcp.read(buf),
            #[cfg(feature = "tls")]
            Wire::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(tcp) => tcp.write(buf),
            #[cfg(feature = "tls")]
            Wire::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(tcp) => tcp.flush(),
            #[cfg(feature = "tls")]
            Wire::Tls(tls) => tls.flush(),
        }
    }
}

/// A client on one kept-alive connection, with the headers it sends on every request. A response
/// that closes the connection makes the next request open a new one.
struct Client {
    addr: SocketAddr,
    trust: Trust,
    reader: BufReader<Wire>,
    session: Option<String>,
    token: Option<String>,
    headers: Vec<(String, String)>,
    next: u64,
    closed: bool,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        Client::connect_with(addr, None)
    }

    fn connect_with(addr: SocketAddr, trust: Trust) -> Client {
        let reader = BufReader::new(Wire::open(addr, &trust));
        Client { addr, trust, reader, session: None, token: None, headers: Vec::new(), next: 1, closed: false }
    }

    fn reconnect(&mut self) {
        self.reader = BufReader::new(Wire::open(self.addr, &self.trust));
        self.closed = false;
    }

    fn write_request(&mut self, method: &str, extra: &[(&str, &str)], body: &[u8]) {
        if self.closed {
            self.reconnect();
        }
        let mut head = format!("{method} {ENDPOINT} HTTP/1.1\r\nHost: {}\r\n", self.addr);
        if !extra.iter().any(|(n, _)| n.eq_ignore_ascii_case("accept")) {
            head.push_str("Accept: application/json, text/event-stream\r\n");
        }
        if !body.is_empty() || method == "POST" {
            head.push_str("Content-Type: application/json\r\n");
            if !extra.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-length")) {
                head.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
        }
        if let Some(session) = &self.session {
            head.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
        }
        if let Some(token) = &self.token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        for (name, value) in self.headers.iter().map(|(n, v)| (n.as_str(), v.as_str())).chain(extra.iter().copied()) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut request = head.into_bytes();
        request.extend_from_slice(body);
        self.reader.get_mut().write_all(&request).expect("write");
    }

    fn read_head(&mut self) -> (u16, Vec<(String, String)>) {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("status line");
        let status = line.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("a status line: {line:?}"));
        let mut headers = Vec::new();
        loop {
            line.clear();
            self.reader.read_line(&mut line).expect("header");
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let (n, v) = l.split_once(':').expect("header");
            headers.push((n.to_ascii_lowercase(), v.trim().to_owned()));
        }
        (status, headers)
    }

    /// One chunk of a chunked body; `None` at its end.
    fn chunk(&mut self) -> Option<String> {
        self.try_chunk().expect("a chunk")
    }

    fn try_chunk(&mut self) -> std::io::Result<Option<String>> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        let size = usize::from_str_radix(line.trim(), 16).map_err(|_| std::io::Error::other(format!("chunk size {line:?}")))?;
        let mut data = vec![0; size + 2];
        self.reader.read_exact(&mut data)?;
        data.truncate(size);
        Ok((size > 0).then(|| String::from_utf8(data).expect("UTF-8")))
    }

    fn read_reply(&mut self) -> Reply {
        let (status, headers) = self.read_head();
        let find = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone());
        let body = if status == 204 {
            String::new()
        } else if find("transfer-encoding").as_deref() == Some("chunked") {
            let mut body = String::new();
            while let Some(chunk) = self.chunk() {
                body.push_str(&chunk);
            }
            body
        } else if let Some(length) = find("content-length") {
            let mut data = vec![0; length.parse().expect("length")];
            self.reader.read_exact(&mut data).expect("body");
            String::from_utf8(data).expect("UTF-8")
        } else {
            let mut body = String::new();
            self.reader.read_to_string(&mut body).expect("body");
            body
        };
        self.closed = find("connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
        Reply { status, headers, body }
    }

    fn post_raw(&mut self, body: &[u8], extra: &[(&str, &str)]) -> Reply {
        self.write_request("POST", extra, body);
        self.read_reply()
    }

    fn post(&mut self, message: &Value) -> Reply {
        self.post_raw(message.to_string().as_bytes(), &[])
    }

    /// Sends a request; returns the reply and the request's id.
    fn request(&mut self, method: &str, params: Value) -> (Reply, u64) {
        let id = self.next;
        self.next += 1;
        (self.post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})), id)
    }

    /// A request answered with JSON: its response.
    fn call(&mut self, method: &str, params: Value) -> Value {
        let (reply, id) = self.request(method, params);
        assert_eq!(reply.status, 200, "{reply:?}");
        assert_eq!(reply.header("content-type"), Some("application/json"), "{reply:?}");
        let response = reply.json();
        assert_eq!(response["id"], id, "{response}");
        response
    }

    /// Opens an older revision's session.
    fn initialize(&mut self, version: &str) -> Value {
        let (reply, _) = self.request("initialize", json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "t"}}));
        assert_eq!(reply.status, 200, "{reply:?}");
        self.session = Some(reply.header("mcp-session-id").expect("a session id").to_owned());
        let note = self.post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        assert_eq!(note.status, 202, "{note:?}");
        reply.json()
    }
}

fn modern(mut params: Value, tasks: bool) -> Value {
    let caps = if tasks { json!({"extensions": {EXT_TASKS: {}}}) } else { json!({}) };
    params["_meta"] = json!({META_VERSION: MODERN, META_CLIENT_CAPS: caps});
    params
}

#[test]
fn an_older_client_initializes_lists_and_calls_over_http() {
    let rig = Rig::new();
    let mut client = rig.client();
    let init = client.initialize("2025-11-25");
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    let session = client.session.clone().expect("session");
    assert!(session.len() >= 32 && session.bytes().all(|b| b.is_ascii_hexdigit()), "{session}");
    let tools = client.call("tools/list", json!({}));
    assert!(tools["result"]["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["name"] == "devices")), "{tools}");
    let devices = client.call("tools/call", json!({"name": "devices", "arguments": {}}));
    assert_eq!(devices["result"]["structuredContent"]["result"][0]["udid"], "phone", "{devices}");
    // The same connection carried every request, and the version header is accepted.
    client.headers.push(("MCP-Protocol-Version".into(), "2025-11-25".into()));
    assert_eq!(client.call("ping", json!({}))["result"], json!({}));
    assert_eq!(rig.running.sessions(), 1);
}

#[test]
fn the_current_revision_is_served_without_a_session() {
    let rig = Rig::new();
    let mut client = rig.client();
    let discover = client.call("server/discover", json!({}));
    assert!(discover["result"]["supportedVersions"].as_array().is_some_and(|v| v.contains(&json!(MODERN))), "{discover}");
    let (reply, id) = client.request("tools/call", modern(json!({"name": "devices", "arguments": {}}), false));
    assert_eq!((reply.status, reply.header("mcp-session-id")), (200, None), "{reply:?}");
    let response = reply.json();
    assert_eq!((response["id"].clone(), response["result"]["resultType"].clone()), (json!(id), json!("complete")), "{response}");
    // A string id comes back as the same string.
    let reply = client.post(&json!({"jsonrpc": "2.0", "id": "abc", "method": "tools/list", "params": modern(json!({}), false)}));
    assert_eq!(reply.json()["id"], "abc");
    assert_eq!(rig.running.sessions(), 0);
}

#[test]
fn a_session_is_required_after_initialize() {
    let rig = Rig::new();
    let mut client = rig.client();
    // An older revision's request with no session: refused, and told how to get one.
    let (reply, _) = client.request("tools/list", json!({}));
    assert_eq!(reply.status, 400, "{reply:?}");
    assert!(reply.json()["error"]["message"].as_str().is_some_and(|m| m.contains("Mcp-Session-Id")), "{reply:?}");
    client.initialize("2025-06-18");
    assert_eq!(client.call("tools/list", json!({}))["result"]["tools"].as_array().map(Vec::is_empty), Some(false));
    let session = client.session.take();
    let (reply, _) = client.request("tools/list", json!({}));
    assert_eq!(reply.status, 400, "the session id is needed on every request: {reply:?}");
    client.session = Some("0123456789abcdef0123456789abcdef".into());
    let (reply, _) = client.request("tools/list", json!({}));
    assert_eq!(reply.status, 404, "an unknown session: {reply:?}");
    // DELETE ends it; then it is unknown.
    client.session = session;
    client.write_request("DELETE", &[], b"");
    assert_eq!(client.read_reply().status, 204);
    let (reply, _) = client.request("tools/list", json!({}));
    assert_eq!(reply.status, 404, "{reply:?}");
    assert_eq!(rig.running.sessions(), 0);
}

#[test]
fn a_long_call_streams_its_progress_then_its_response() {
    let rig = Rig::new();
    let gate = rig.fake.gate("boot");
    rig.fake.answer("boot", Ok(json!({"boot_s": 3})));
    let mut params = modern(json!({"name": "boot", "arguments": {"device": "phone"}}), false);
    params["_meta"]["progressToken"] = json!("p-1");
    let mut client = rig.client();
    client.write_request("POST", &[], json!({"jsonrpc": "2.0", "id": 41, "method": "tools/call", "params": params}).to_string().as_bytes());
    let (status, headers) = client.read_head();
    assert_eq!(status, 200);
    assert!(headers.contains(&("content-type".into(), "text/event-stream".into())), "{headers:?}");
    // The first event is a progress under the client's own token; then the call is let finish.
    let first = events_of(&client.chunk().expect("an event"));
    assert_eq!(first[0]["method"], "notifications/progress", "{first:?}");
    assert_eq!(first[0]["params"]["progressToken"], "p-1");
    gate.release();
    let mut rest = Vec::new();
    while let Some(chunk) = client.chunk() {
        rest.extend(events_of(&chunk));
    }
    let (response, before) = rest.split_last().expect("the response ends the stream");
    assert_eq!((response["id"].clone(), response["result"]["structuredContent"]["boot_s"].clone()), (json!(41), json!(3)), "{response}");
    assert!(before.iter().all(|n| n["method"] == "notifications/progress" && n["params"]["progressToken"] == "p-1"), "{before:?}");
    // The stream ended cleanly: the connection carries the next request.
    assert_eq!(client.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
}

#[test]
fn a_call_without_notifications_is_plain_json_and_one_that_cannot_stream_drops_them() {
    let rig = Rig::new();
    let gate = rig.fake.gate("boot");
    let mut params = modern(json!({"name": "boot", "arguments": {"device": "phone"}}), false);
    params["_meta"]["progressToken"] = json!(7);
    let mut client = rig.client();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params}).to_string();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        gate.release();
    });
    // A client that takes only JSON gets the response alone.
    client.write_request("POST", &[("Accept", "application/json")], body.as_bytes());
    let reply = client.read_reply();
    release.join().expect("release");
    assert_eq!(reply.header("content-type"), Some("application/json"), "{reply:?}");
    assert_eq!(reply.json()["id"], 1);
}

#[test]
fn subscriptions_listen_streams_on_its_post_until_the_client_closes_it() {
    let rig = Rig::new();
    rig.fake.answer("info", Ok(json!({"state": "Shutdown"})));
    let mut client = rig.client();
    let params = modern(json!({"notifications": {"resourceSubscriptions": ["simee://devices/phone"]}}), false);
    client.write_request(
        "POST",
        &[],
        json!({"jsonrpc": "2.0", "id": "sub", "method": "subscriptions/listen", "params": params}).to_string().as_bytes(),
    );
    let (status, _) = client.read_head();
    assert_eq!(status, 200);
    let ack = events_of(&client.chunk().expect("the acknowledgement"));
    assert_eq!(ack[0]["method"], "notifications/subscriptions/acknowledged", "{ack:?}");
    assert_eq!(ack[0]["params"]["_meta"][META_SUBSCRIPTION], "sub", "the client's own id");
    let deadline = Instant::now() + WAIT;
    while rig.fake.called("info").is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    rig.fake.answer("info", Ok(json!({"state": "Booted"})));
    let updated = loop {
        let events = events_of(&client.chunk().expect("an update"));
        if let Some(e) = events.into_iter().find(|e| e["method"] == "notifications/resources/updated") {
            break e;
        }
    };
    assert_eq!(
        (updated["params"]["uri"].clone(), updated["params"]["_meta"][META_SUBSCRIPTION].clone()),
        (json!("simee://devices/phone"), json!("sub"))
    );
    // Closing the stream ends the subscription: its route goes.
    drop(client);
    let deadline = Instant::now() + WAIT;
    while !lock(&rig.running.shared.router.routes).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(lock(&rig.running.shared.router.routes).is_empty());
}

#[test]
fn ending_a_session_stops_what_it_watches() {
    let rig = Rig::new();
    rig.fake.answer("screenshot", Ok(json!({"png": "AAAA"})));
    let mut client = rig.client();
    client.initialize("2025-11-25");
    assert_eq!(client.call("resources/subscribe", json!({"uri": "simee://devices/phone/screen"}))["result"], json!({}));
    let deadline = Instant::now() + WAIT;
    while rig.fake.called("screenshot").is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    client.write_request("DELETE", &[], b"");
    assert_eq!(client.read_reply().status, 204);
    // A read under way may finish; then none follows.
    let mut reads = rig.fake.called("screenshot").len();
    loop {
        std::thread::sleep(Duration::from_millis(150));
        let now = rig.fake.called("screenshot").len();
        if now == reads {
            break;
        }
        reads = now;
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.fake.called("screenshot").len(), reads, "the ended session still reads");
}

#[test]
fn a_missing_or_wrong_token_is_refused_and_rotating_is_rewriting_the_file() {
    let scratch = Scratch::new("token");
    let path = scratch.0.join("state/mcp-token");
    let rig = Rig::with(|c| {
        c.no_auth = false;
        c.token_file = Some(path.clone());
    });
    // The file was made, for its owner only, with a long random token.
    let token = std::fs::read_to_string(&path).expect("token").trim().to_owned();
    assert_eq!(token.len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777, 0o600);
    }
    let mut client = rig.client();
    let (reply, _) = client.request("server/discover", json!({}));
    assert_eq!((reply.status, reply.header("www-authenticate")), (401, Some("Bearer realm=\"simee\"")), "{reply:?}");
    client.reconnect();
    client.token = Some("not-the-token-but-long-enough".into());
    let (reply, _) = client.request("server/discover", json!({}));
    assert_eq!(reply.status, 401, "{reply:?}");
    client.reconnect();
    client.token = Some(token.clone());
    assert_eq!(client.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    // Rotate: a new token in the file; the old one stops working at once.
    let fresh = "a-rotated-token-0123456789-abcdefghij";
    std::fs::write(&path, format!("# rotated\n{fresh}\n")).expect("rewrite");
    let (reply, _) = client.request("server/discover", json!({}));
    assert_eq!(reply.status, 401, "{reply:?}");
    client.reconnect();
    client.token = Some(fresh.into());
    assert_eq!(client.call("server/discover", json!({}))["id"], json!(client.next - 1));
}

#[test]
fn a_token_file_others_can_read_or_a_short_token_is_refused() {
    let scratch = Scratch::new("perms");
    let path = scratch.0.join("token");
    std::fs::write(&path, "0123456789abcdef0123\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let e = ensure_token_file(&path).expect_err("readable by others");
        assert!(e.to_string().contains("chmod 600"), "{e}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    ensure_token_file(&path).expect("private");
    std::fs::write(&path, "short\n").expect("write");
    assert!(ensure_token_file(&path).expect_err("short").to_string().contains("at least"));
}

#[test]
fn every_bind_requires_a_token_unless_no_auth_on_loopback() {
    let scratch = Scratch::new("remote");
    let default = scratch.0.join("state/mcp-token");
    let from_state_dir = || Ok(default.clone());
    let mut config = Config::new("192.0.2.1:0".parse().expect("addr"));
    let e = prepare_auth(&config, from_state_dir).err().expect("refused");
    assert!(e.to_string().contains("--insecure-remote"), "{e}");
    config.insecure_remote = true;
    config.token_file = Some(scratch.0.join("t"));
    let tokens = prepare_auth(&config, from_state_dir).expect("acknowledged").expect("a token is required off loopback");
    assert_eq!(lock(&tokens.state).tokens.len(), 1);
    // Off loopback, --no-auth is refused.
    config.token_file = None;
    config.no_auth = true;
    let e = prepare_auth(&config, from_state_dir).err().expect("--no-auth off loopback");
    assert!(e.to_string().contains("loopback"), "{e}");
    // Loopback too: with no file named, the state directory's is made (and reused next time).
    for addr in ["127.0.0.1:0", "[::1]:0"] {
        let tokens = prepare_auth(&Config::new(addr.parse().expect("addr")), from_state_dir).expect("loopback").expect("a token");
        assert_eq!(tokens.path, default);
        assert_eq!(lock(&tokens.state).tokens.len(), 1);
    }
    let token = std::fs::read_to_string(&default).expect("token").trim().to_owned();
    assert_eq!(token.len(), 64);
    let again = prepare_auth(&Config::new("127.0.0.1:0".parse().expect("addr")), from_state_dir).expect("reused").expect("a token");
    assert_eq!(lock(&again.state).tokens, [token]);
    // --no-auth on loopback turns it off; with a token file too it is a contradiction.
    let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
    config.no_auth = true;
    assert!(prepare_auth(&config, from_state_dir).expect("loopback").is_none());
    config.token_file = Some(scratch.0.join("t"));
    assert!(prepare_auth(&config, from_state_dir).is_err());
}

#[test]
fn a_non_loopback_bind_needs_tls_or_the_acknowledgement_and_still_a_token() {
    let scratch = Scratch::new("remote-tls");
    let token_file = scratch.0.join("state/mcp-token");
    let mut config = Config::new("192.0.2.1:0".parse().expect("addr"));
    let e = prepare_auth(&config, || unreachable!("no default token file here")).err().expect("plain HTTP off loopback");
    let message = e.to_string();
    assert!(message.contains("--tls-cert") && message.contains("--tls-self-signed") && message.contains("--insecure-remote"), "{message}");
    // TLS is what makes it safe to bind there: the token is still required, and --no-auth still isn't allowed.
    config.tls = Some(Tls::SelfSigned { dir: None, names: Vec::new() });
    config.token_file = Some(token_file);
    assert!(prepare_auth(&config, || unreachable!()).expect("TLS configured").is_some(), "a token is required over TLS too");
    config.token_file = None;
    config.no_auth = true;
    let e = prepare_auth(&config, || unreachable!()).err().expect("--no-auth off loopback");
    assert!(e.to_string().contains("loopback"), "{e}");
}

#[cfg(not(feature = "tls"))]
#[test]
fn without_the_tls_feature_asking_for_tls_is_an_error_that_says_how_to_build_it() {
    let scratch = Scratch::new("no-tls-feature");
    let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
    config.no_auth = true;
    config.tls = Some(Tls::SelfSigned { dir: Some(scratch.0.clone()), names: Vec::new() });
    let e = start(Arc::new(Fake::with_devices()), config).err().expect("no TLS in this build");
    assert_eq!(e.kind(), std::io::ErrorKind::Unsupported);
    assert!(e.to_string().contains("--features tls"), "{e}");
    assert_eq!(std::fs::read_dir(&scratch.0).expect("dir").count(), 0, "and nothing was written");
}

/// A rig that requires a token, and the token.
fn authed_rig(scratch: &Scratch) -> (Rig, String) {
    let path = scratch.0.join("state/mcp-token");
    let rig = Rig::with(|c| {
        c.no_auth = false;
        c.token_file = Some(path.clone());
    });
    let token = std::fs::read_to_string(&path).expect("token").trim().to_owned();
    (rig, token)
}

fn wait_for_connections(rig: &Rig, n: usize) {
    let deadline = Instant::now() + WAIT;
    while rig.running.shared.connections.load(Ordering::SeqCst) < n && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn unauthenticated_connections_past_the_per_peer_limit_get_503() {
    let scratch = Scratch::new("peer-unauth");
    let (rig, _) = authed_rig(&scratch);
    let cap = rig.running.shared.config.limits.max_connections_per_peer;
    assert_eq!(cap, 8, "the default cap");
    // Slow connections: open, and silent.
    let held: Vec<Client> = (0..cap).map(|_| rig.client()).collect();
    wait_for_connections(&rig, cap);
    let mut ninth = rig.client();
    let reply = ninth.read_reply();
    assert_eq!(reply.status, 503, "{reply:?}");
    assert!(reply.body.contains("from this address"), "{reply:?}");
    drop(held);
    let deadline = Instant::now() + WAIT;
    while !lock(&rig.running.shared.peers).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(lock(&rig.running.shared.peers).is_empty(), "every slot was given back");
}

#[test]
fn authenticated_connections_are_not_bounded_by_the_per_peer_limit() {
    let scratch = Scratch::new("peer-auth");
    let (rig, token) = authed_rig(&scratch);
    let cap = rig.running.shared.config.limits.max_connections_per_peer;
    assert_eq!(cap, 8, "the default cap, not a raised one");
    // More kept-alive connections than the cap, all from this address, all served and all open.
    // Each authenticates on its first request, before the next one connects.
    let mut clients: Vec<Client> = Vec::new();
    for _ in 0..cap + 4 {
        let mut c = rig.client();
        c.token = Some(token.clone());
        assert_eq!(c.call("server/discover", json!({}))["id"], 1);
        clients.push(c);
    }
    for c in &mut clients {
        assert_eq!(c.call("server/discover", json!({}))["id"], 2, "still served on its kept-alive connection");
    }
    assert!(lock(&rig.running.shared.peers).is_empty(), "authenticated connections hold no address slot");
}

#[test]
fn a_bare_relative_token_file_still_checks_its_directory() {
    assert_eq!(token_dir(Path::new("mcp-token")), Path::new("."));
    assert_eq!(token_dir(Path::new("./mcp-token")), Path::new("."));
    assert_eq!(token_dir(Path::new("a/b/mcp-token")), Path::new("a/b"));
}

/// Two tokens in one file: two principals.
fn two_tokens(scratch: &Scratch) -> (PathBuf, String, String) {
    let (a, b) = ("token-a-0123456789abcdef0123456789".to_owned(), "token-b-0123456789abcdef0123456789".to_owned());
    let path = scratch.0.join("tokens");
    std::fs::write(&path, format!("{a}\n{b}\n")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    (path, a, b)
}

#[test]
fn a_session_belongs_to_the_token_that_opened_it() {
    let scratch = Scratch::new("owners");
    let (path, a, b) = two_tokens(&scratch);
    let rig = Rig::with(|c| {
        c.no_auth = false;
        c.token_file = Some(path);
    });
    let mut alice = rig.client();
    alice.token = Some(a);
    alice.initialize("2025-11-25");
    // Bob, with a valid token of his own and Alice's session id: it is not his to use, watch or end.
    let mut bob = rig.client();
    bob.token = Some(b);
    bob.session = alice.session.clone();
    let (reply, _) = bob.request("ping", json!({}));
    assert_eq!(reply.status, 404, "{reply:?}");
    bob.write_request("GET", &[], b"");
    assert_eq!(bob.read_reply().status, 404);
    bob.write_request("DELETE", &[], b"");
    assert_eq!(bob.read_reply().status, 404);
    assert_eq!(rig.running.sessions(), 1);
    assert_eq!(alice.call("ping", json!({}))["result"], json!({}));
    alice.write_request("DELETE", &[], b"");
    assert_eq!(alice.read_reply().status, 204);
    assert_eq!(rig.running.sessions(), 0);
}

#[test]
fn a_full_session_table_closes_only_the_callers_own_sessions() {
    let scratch = Scratch::new("evict");
    let (path, a, b) = two_tokens(&scratch);
    let rig = Rig::with(|c| {
        c.no_auth = false;
        c.token_file = Some(path);
        c.limits.max_sessions = 2;
    });
    let mut alices: Vec<Client> = (0..2).map(|_| rig.client()).collect();
    for c in &mut alices {
        c.token = Some(a.clone());
        c.initialize("2025-11-25");
    }
    // Bob can't push Alice's idle sessions out.
    let mut bob = rig.client();
    bob.token = Some(b);
    let (reply, _) = bob.request("initialize", json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "t"}}));
    assert_eq!(reply.status, 503, "{reply:?}");
    for c in &mut alices {
        assert_eq!(c.call("ping", json!({}))["result"], json!({}));
    }
    // Alice can: her own idlest goes.
    let mut third = rig.client();
    third.token = Some(a);
    third.initialize("2025-11-25");
    let (reply, _) = alices[0].request("ping", json!({}));
    assert_eq!(reply.status, 404, "{reply:?}");
    assert_eq!(alices[1].call("ping", json!({}))["result"], json!({}));
}

#[test]
fn an_idle_session_expires_even_while_a_get_stream_holds_it() {
    let rig = Rig::with(|c| c.limits.session_idle = Duration::from_millis(400));
    let mut client = rig.client();
    client.initialize("2025-11-25");
    let mut stream = Client::connect(rig.running.addr());
    stream.session = client.session.clone();
    stream.write_request("GET", &[], b"");
    assert_eq!(stream.read_head().0, 200);
    // Only pings come; then, with no request for the idle time, the stream ends and the session goes.
    let start = Instant::now();
    while stream.chunk().is_some() {
        assert!(start.elapsed() < Duration::from_secs(5), "the stream outlived its session");
    }
    assert!(start.elapsed() >= Duration::from_millis(300), "{:?}", start.elapsed());
    assert_eq!(rig.running.sessions(), 0);
    let (reply, _) = client.request("ping", json!({}));
    assert_eq!(reply.status, 404, "{reply:?}");
    // A session in use stays: requests keep it alive.
    client.session = None;
    client.initialize("2025-11-25");
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(client.call("ping", json!({}))["result"], json!({}));
    }
    assert_eq!(rig.running.sessions(), 1);
}

#[test]
fn a_foreign_origin_or_host_is_refused() {
    let rig = Rig::new();
    let discover = json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}).to_string();
    for (origin, status) in
        [("https://evil.example", 403), ("null", 403), ("http://localhost:3000", 200), ("http://127.0.0.1", 200), ("http://[::1]:9", 200)]
    {
        let mut client = rig.client();
        let reply = client.post_raw(discover.as_bytes(), &[("Origin", origin)]);
        assert_eq!(reply.status, status, "{origin}: {reply:?}");
    }
    // A loopback server answers only loopback host names: a rebinding page's Host is refused.
    let mut stream = TcpStream::connect(rig.running.addr()).expect("connect");
    let request = format!(
        "POST {ENDPOINT} HTTP/1.1\r\nHost: evil.example:8377\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{discover}",
        discover.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
}

#[test]
fn oversized_and_malformed_requests_are_refused() {
    let rig = Rig::new();
    let mut client = rig.client();
    let max = limits().max_body;
    // Declared too large: refused before the body is read, and the connection closed.
    client.write_request("POST", &[("Content-Length", &(max + 1).to_string())], b"");
    let reply = client.read_reply();
    assert_eq!((reply.status, reply.header("connection")), (413, Some("close")), "{reply:?}");
    // Chunked past the limit.
    let mut client = rig.client();
    let mut stream = client.reader.get_ref().tcp().try_clone().expect("clone");
    let head = format!(
        "POST {ENDPOINT} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
        rig.running.addr()
    );
    stream.write_all(head.as_bytes()).expect("head");
    let chunk = vec![b' '; 16 << 10];
    for _ in 0..(max / chunk.len() + 1) {
        if stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .and_then(|()| stream.write_all(&chunk))
            .and_then(|()| stream.write_all(b"\r\n"))
            .is_err()
        {
            break;
        }
    }
    assert_eq!(client.read_reply().status, 413);
    // A chunked body within it is read.
    let mut client = rig.client();
    let body = json!({"jsonrpc": "2.0", "id": 9, "method": "server/discover", "params": {}}).to_string();
    let (a, b) = body.split_at(10);
    let mut stream = client.reader.get_ref().tcp().try_clone().expect("clone");
    stream.write_all(head.as_bytes()).expect("head");
    stream.write_all(format!("{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n", a.len(), b.len()).as_bytes()).expect("chunks");
    assert_eq!(client.read_reply().json()["id"], 9);
    // A head past its limit, a body that isn't JSON-RPC, a version nobody speaks, a wrong path.
    let mut client = rig.client();
    let big = "x".repeat(limits().max_head + 1);
    client.write_request("POST", &[("X-Big", &big)], b"{}");
    assert_eq!(client.read_reply().status, 431);
    let mut client = rig.client();
    let reply = client.post_raw(b"{not json", &[]);
    assert_eq!((reply.status, reply.json()["error"]["code"].clone()), (400, json!(PARSE_ERROR)));
    let reply = client.post_raw(b"[]", &[]);
    assert_eq!((reply.status, reply.json()["error"]["code"].clone()), (400, json!(INVALID_REQUEST)));
    let reply = client.post_raw(body.as_bytes(), &[("MCP-Protocol-Version", "1999-01-01")]);
    assert_eq!((reply.status, reply.json()["error"]["code"].clone()), (400, json!(rpc::UNSUPPORTED_VERSION)));
    let mismatched = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": modern(json!({}), false)}).to_string();
    let reply = client.post_raw(mismatched.as_bytes(), &[("MCP-Protocol-Version", "2025-06-18")]);
    assert_eq!(reply.status, 400, "{reply:?}");
    // A response from the client (this server asks nothing) is accepted and ignored.
    assert_eq!(client.post(&json!({"jsonrpc": "2.0", "id": 5, "result": {}})).status, 202);
    let mut stream = TcpStream::connect(rig.running.addr()).expect("connect");
    stream.write_all(format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", rig.running.addr()).as_bytes()).expect("write");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("read");
    assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
}

#[test]
fn connections_past_the_limit_get_503() {
    let rig = Rig::with(|c| c.limits.max_connections = 2);
    let held: Vec<Client> = (0..2).map(|_| rig.client()).collect();
    // Wait for both to be counted.
    let deadline = Instant::now() + WAIT;
    while rig.running.shared.connections.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut third = rig.client();
    assert_eq!(third.read_reply().status, 503);
    drop(held);
    let deadline = Instant::now() + WAIT;
    while rig.running.shared.connections.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.client().call("server/discover", json!({}))["id"], 1);
}

#[test]
fn concurrent_sessions_are_isolated() {
    let rig = Rig::new();
    let mut a = rig.client();
    let mut b = rig.client();
    // Different revisions: each session keeps its own.
    a.initialize("2025-03-26");
    b.initialize("2025-11-25");
    assert_ne!(a.session, b.session);
    let schema = |c: &mut Client| {
        let tools = c.call("tools/list", json!({}));
        tools["result"]["tools"].as_array().expect("tools").iter().any(|t| t.get("outputSchema").is_some())
    };
    assert!(!schema(&mut a), "2025-03-26 has no output schemas");
    assert!(schema(&mut b), "2025-11-25 has them");
    // Both send id 1 at once to a call that is held: each gets its own answer.
    let gate = rig.fake.gate("devices");
    let mut a2 = rig.client();
    let mut b2 = rig.client();
    a2.session = a.session.clone();
    b2.session = b.session.clone();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "devices", "arguments": {}}}).to_string();
    a2.write_request("POST", &[], call.as_bytes());
    b2.write_request("POST", &[], call.as_bytes());
    let deadline = Instant::now() + WAIT;
    while rig.fake.called("devices").len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    gate.release();
    let (ra, rb) = (a2.read_reply().json(), b2.read_reply().json());
    assert_eq!((ra["id"].clone(), rb["id"].clone()), (json!(1), json!(1)));
    // 2025-03-26 puts the listing under `result` too, but neither saw the other's answer twice.
    assert!(ra["result"]["content"].is_array() && rb["result"]["content"].is_array(), "{ra} {rb}");
    // A's resource subscription reaches A's GET stream, not B's.
    rig.fake.gates.lock().expect("gates").clear();
    rig.fake.answer("info", Ok(json!({"state": "Shutdown"})));
    let mut a_stream = Client::connect(rig.running.addr());
    a_stream.session = a.session.clone();
    a_stream.write_request("GET", &[], b"");
    assert_eq!(a_stream.read_head().0, 200);
    let mut b_stream = Client::connect(rig.running.addr());
    b_stream.session = b.session.clone();
    b_stream.write_request("GET", &[], b"");
    assert_eq!(b_stream.read_head().0, 200);
    assert_eq!(a.call("resources/subscribe", json!({"uri": "simee://devices/phone"}))["result"], json!({}));
    let deadline = Instant::now() + WAIT;
    while rig.fake.called("info").is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    rig.fake.answer("info", Ok(json!({"state": "Booted"})));
    let updated = loop {
        let events = events_of(&a_stream.chunk().expect("an update"));
        if let Some(e) = events.into_iter().find(|e| e["method"] == "notifications/resources/updated") {
            break e;
        }
    };
    assert_eq!(updated["params"]["uri"], "simee://devices/phone");
    // B's stream has had only pings meanwhile.
    b_stream.reader.get_ref().tcp().set_read_timeout(Some(Duration::from_millis(400))).expect("timeout");
    let mut seen = String::new();
    while let Ok(Some(chunk)) = b_stream.try_chunk() {
        seen.push_str(&chunk);
        if seen.len() > 200 {
            break;
        }
    }
    assert!(events_of(&seen).is_empty(), "{seen}");
    // Ending A leaves B working, and ends A's stream.
    a.write_request("DELETE", &[], b"");
    assert_eq!(a.read_reply().status, 204);
    a_stream.reader.get_ref().tcp().set_read_timeout(Some(WAIT)).expect("timeout");
    while a_stream.chunk().is_some() {}
    assert_eq!(b.call("ping", json!({}))["result"], json!({}));
    assert_eq!(rig.running.sessions(), 1);
}

#[test]
fn a_get_needs_a_session_and_a_cancel_names_a_sessions_own_request() {
    let rig = Rig::new();
    let mut client = rig.client();
    client.write_request("GET", &[], b"");
    assert_eq!(client.read_reply().status, 405);
    client.initialize("2025-11-25");
    // Cancelling a request this session has no such id for is accepted and changes nothing.
    assert_eq!(client.post(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 99}})).status, 202);
    // A held call, cancelled by its id: no answer comes, and the stream closes when the client does.
    let gate = rig.fake.gate("boot");
    let mut caller = Client::connect(rig.running.addr());
    caller.session = client.session.clone();
    caller.write_request(
        "POST",
        &[],
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "boot", "arguments": {"device": "phone"}}})
            .to_string()
            .as_bytes(),
    );
    let deadline = Instant::now() + WAIT;
    while rig.fake.called("boot").is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(client.post(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5}})).status, 202);
    let session = lock(&rig.running.shared.sessions).values().next().cloned().expect("session");
    let internal = lock(&session.cancels).get("5").cloned().expect("the call is in flight");
    gate.release();
    drop(caller);
    let deadline = Instant::now() + WAIT;
    while lock(&rig.running.shared.router.routes).contains_key(&internal) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!lock(&rig.running.shared.router.routes).contains_key(&internal));
}

#[test]
fn a_full_session_table_closes_the_idlest_session_first() {
    let rig = Rig::with(|c| c.limits.max_sessions = 2);
    let mut clients: Vec<Client> = (0..3).map(|_| rig.client()).collect();
    for c in &mut clients {
        c.initialize("2025-11-25");
    }
    assert_eq!(rig.running.sessions(), 2);
    let (reply, _) = clients[0].request("ping", json!({}));
    assert_eq!(reply.status, 404, "the oldest idle session went: {reply:?}");
    assert_eq!(clients[2].call("ping", json!({}))["result"], json!({}));
}

#[test]
fn the_router_puts_the_clients_ids_back() {
    let router = Router::default();
    let (tx, rx) = note_queue(NOTE_QUEUE);
    router.insert(
        "h7".into(),
        Route { id: json!({"weird": 1}).get("weird").cloned().expect("id"), token: Some(json!("tok")), tx: Arc::new(tx) },
    );
    router.deliver(r#"{"id":"h7","jsonrpc":"2.0","result":{"x":"{\"id\":\"h7\"}"}}"#.into(), None);
    let Ok(Out::Response(line)) = rx.recv_timeout(Duration::ZERO) else { panic!("a response") };
    assert_eq!(serde_json::from_str::<Value>(&line).expect("json"), json!({"id": 1, "jsonrpc": "2.0", "result": {"x": "{\"id\":\"h7\"}"}}));
    assert!(lock(&router.routes).is_empty(), "a response ends its route");
    // Insertion order (`jsonrpc` first) and an error (`error` first) are found too.
    for line in [r#"{"jsonrpc":"2.0","id":"h8","result":{}}"#, r#"{"error":{"code":1,"message":"m"},"id":"h8","jsonrpc":"2.0"}"#] {
        let (tx, rx) = note_queue(NOTE_QUEUE);
        router.insert("h8".into(), Route { id: json!("mine"), token: None, tx: Arc::new(tx) });
        router.deliver(line.into(), None);
        let Ok(Out::Response(out)) = rx.recv_timeout(Duration::ZERO) else { panic!("a response for {line}") };
        assert_eq!(serde_json::from_str::<Value>(&out).expect("json")["id"], "mine");
    }
    // Progress for no request, and messages with no route, go nowhere; the rest go to the session.
    let standalone: Standalone = Mutex::new(None);
    let (tx, rx) = note_queue(NOTE_QUEUE);
    *lock(&standalone) = Some((0, tx));
    router.deliver(
        r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"h99","progress":1}}"#.into(),
        Some(&standalone),
    );
    router.deliver(r#"{"id":"h99","jsonrpc":"2.0","result":{}}"#.into(), Some(&standalone));
    router.deliver(r#"{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"u"}}"#.into(), Some(&standalone));
    let note = rx.try_recv().expect("the update");
    assert!(note.contains("resources/updated"));
    assert!(rx.try_recv().is_none());
}

fn updated(uri: &str) -> String {
    json!({"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": uri}}).to_string()
}

fn queued(rx: &NoteRx) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv()).collect()
}

#[test]
fn a_get_stream_nobody_reads_queues_a_bounded_number_and_never_blocks_the_sender() {
    let router = Router::default();
    let standalone: Standalone = Mutex::new(None);
    let (tx, rx) = note_queue(NOTE_QUEUE);
    *lock(&standalone) = Some((0, tx));
    // Three times the queue's worth of different resources for a client that stopped reading:
    // every `deliver` returns at once.
    for n in 0..NOTE_QUEUE * 3 {
        router.deliver(updated(&format!("u{n}")), Some(&standalone));
    }
    let got = queued(&rx);
    assert_eq!(got.len(), NOTE_QUEUE);
    assert!(got[0].contains(&format!("\"u{}\"", NOTE_QUEUE * 2)), "the oldest are dropped, not the newest");
    assert!(got[NOTE_QUEUE - 1].contains(&format!("\"u{}\"", NOTE_QUEUE * 3 - 1)));
    // Once read, it takes more.
    router.deliver(updated("later"), Some(&standalone));
    assert!(rx.try_recv().is_some_and(|n| n.contains("later")));
}

#[test]
fn a_stalled_get_stream_keeps_the_latest_update_of_each_resource_behind_a_busy_screen() {
    let router = Router::default();
    let standalone: Standalone = Mutex::new(None);
    let (tx, rx) = note_queue(NOTE_QUEUE);
    *lock(&standalone) = Some((0, tx));
    router.deliver(updated("simee://devices/p/logs"), Some(&standalone));
    router.deliver(updated("simee://devices/p/prefs/app"), Some(&standalone));
    // A screen announced 30 times a second for ten seconds, and the list changing as often.
    let list = json!({"jsonrpc": "2.0", "method": "notifications/resources/list_changed", "params": {}}).to_string();
    for _ in 0..300 {
        router.deliver(updated("simee://devices/p/screen"), Some(&standalone));
        router.deliver(list.clone(), Some(&standalone));
    }
    router.deliver(updated("simee://devices/p/logs"), Some(&standalone));
    let got = queued(&rx);
    assert_eq!(got.len(), 4, "one message for each of the screen, the list, the prefs and the logs: {got:?}");
    // The logs' latest is the one kept, in the order the updates last happened.
    let uris: Vec<String> = got.iter().map(|n| serde_json::from_str::<Value>(n).expect("json")["params"]["uri"].to_string()).collect();
    assert_eq!(uris, [r#""simee://devices/p/prefs/app""#, r#""simee://devices/p/screen""#, "null", r#""simee://devices/p/logs""#]);
}

/// A route for a request the client sent as `id`, listening under the internal id `h1`.
fn listen_route(router: &Router, id: Value) -> NoteRx {
    let (tx, rx) = note_queue(NOTE_QUEUE);
    router.insert("h1".into(), Route { id, token: Some(json!("tok")), tx: Arc::new(tx) });
    rx
}

fn listened(method: &str, params: Value) -> String {
    let mut params = params;
    params["_meta"] = json!({META_SUBSCRIPTION: "h1"});
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

fn listened_update(uri: &str) -> String {
    listened("notifications/resources/updated", json!({"uri": uri}))
}

fn drained(rx: &NoteRx) -> Vec<Value> {
    std::iter::from_fn(|| rx.try_recv()).map(|n| serde_json::from_str(&n).expect("json")).collect()
}

#[test]
fn a_stalled_listen_queues_a_bounded_number_and_never_blocks_the_sender() {
    let router = Router::default();
    let rx = listen_route(&router, json!(7));
    router.deliver(listened("notifications/subscriptions/acknowledged", json!({})), None);
    // Three times the queue's worth of different resources for a client that stopped reading:
    // every `deliver` returns at once.
    for n in 0..NOTE_QUEUE * 3 {
        router.deliver(listened_update(&format!("u{n}")), None);
    }
    let got = drained(&rx);
    assert_eq!(got.len(), NOTE_QUEUE, "the acknowledgement and the newest updates");
    assert_eq!(got[0]["method"], "notifications/subscriptions/acknowledged", "the acknowledgement is never the one dropped");
    assert_eq!(got[0]["params"]["_meta"][META_SUBSCRIPTION], 7, "with the client's own id put back");
    assert_eq!(got[1]["params"]["uri"], format!("u{}", NOTE_QUEUE * 3 - (NOTE_QUEUE - 1)), "the oldest updates are dropped");
    assert_eq!(got[NOTE_QUEUE - 1]["params"]["uri"], format!("u{}", NOTE_QUEUE * 3 - 1));
    // Once read, it takes more.
    router.deliver(listened_update("later"), None);
    assert_eq!(drained(&rx)[0]["params"]["uri"], "later");
}

#[test]
fn a_stalled_listen_keeps_the_latest_update_of_each_resource_behind_a_busy_screen() {
    let router = Router::default();
    let rx = listen_route(&router, json!("L"));
    router.deliver(listened_update("simee://devices/p/logs"), None);
    router.deliver(listened_update("simee://devices/p/prefs/app"), None);
    // A screen announced 30 times a second for ten seconds, and the list changing as often.
    let list = listened("notifications/resources/list_changed", json!({}));
    for _ in 0..300 {
        router.deliver(listened_update("simee://devices/p/screen"), None);
        router.deliver(list.clone(), None);
    }
    router.deliver(listened_update("simee://devices/p/logs"), None);
    let got = drained(&rx);
    assert_eq!(got.len(), 4, "one message for each of the screen, the list, the prefs and the logs: {got:?}");
    // The logs' latest is the one kept, in the order the updates last happened.
    let uris: Vec<&Value> = got.iter().map(|n| &n["params"]["uri"]).collect();
    assert_eq!(
        uris,
        [&json!("simee://devices/p/prefs/app"), &json!("simee://devices/p/screen"), &Value::Null, &json!("simee://devices/p/logs")]
    );
}

#[test]
fn a_stalled_listens_response_follows_its_notifications_and_is_never_dropped() {
    let router = Router::default();
    let rx = listen_route(&router, json!(3));
    // Progress for the request: the latest is the one that counts.
    for progress in 1..=1000 {
        let line = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "h1", "progress": progress}});
        router.deliver(line.to_string(), None);
    }
    // A full queue of other things, then the response.
    for n in 0..NOTE_QUEUE * 2 {
        router.deliver(listened_update(&format!("u{n}")), None);
    }
    router.deliver(r#"{"jsonrpc":"2.0","id":"h1","result":{"done":true}}"#.into(), None);
    let mut got = Vec::new();
    while let Ok(out) = rx.recv_timeout(Duration::ZERO) {
        got.push(out);
    }
    let Some(Out::Response(last)) = got.pop() else { panic!("the response comes last: {got:?}") };
    assert_eq!(serde_json::from_str::<Value>(&last).expect("json"), json!({"jsonrpc": "2.0", "id": 3, "result": {"done": true}}));
    assert_eq!(got.len(), NOTE_QUEUE, "the notifications are bounded, the response is not among them");
    let progress: Vec<&Out> = got.iter().filter(|o| matches!(o, Out::Note(n) if n.contains("notifications/progress"))).collect();
    assert!(progress.len() <= 1, "progress is coalesced to its latest: {progress:?}");
    assert!(lock(&router.routes).is_empty(), "a response ends its route");
    assert_eq!(rx.recv_timeout(Duration::ZERO), Err(RecvTimeoutError::Disconnected), "and its stream");
}

#[test]
fn progress_and_task_updates_keep_only_the_latest_of_each() {
    let router = Router::default();
    let rx = listen_route(&router, json!(1));
    for step in 1..=50 {
        let progress = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "h1", "progress": step}});
        router.deliver(progress.to_string(), None);
        router.deliver(listened("notifications/tasks", json!({"taskId": "a", "status": format!("s{step}")})), None);
        router.deliver(listened("notifications/tasks", json!({"taskId": "b", "status": format!("s{step}")})), None);
    }
    let got = drained(&rx);
    assert_eq!(got.len(), 3, "progress and each task once: {got:?}");
    let progress = got.iter().find(|n| n["method"] == "notifications/progress").expect("progress");
    assert_eq!((progress["params"]["progress"].as_i64(), progress["params"]["progressToken"].as_str()), (Some(50), Some("tok")));
    for task in ["a", "b"] {
        let note = got.iter().find(|n| n["params"]["taskId"] == task).expect("the task's update");
        assert_eq!(note["params"]["status"], "s50");
    }
}

#[test]
fn a_get_streams_queue_ends_when_its_sender_goes_but_hands_over_what_it_holds_first() {
    let (tx, rx) = note_queue(4);
    tx.push(None, "one".into());
    drop(tx);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(Out::Note("one".into())));
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Err(RecvTimeoutError::Disconnected));
    let (_tx, rx) = note_queue(4);
    assert_eq!(rx.recv_timeout(Duration::from_millis(10)), Err(RecvTimeoutError::Timeout));
}

#[test]
fn hosts_origins_and_tokens_parse() {
    for (authority, host) in [("localhost:8377", "localhost"), ("[::1]:80", "::1"), ("::1", "::1"), ("example.com", "example.com")] {
        assert_eq!(host_of(authority), host);
    }
    assert!(loopback_host("LOCALHOST") && loopback_host("app.localhost") && loopback_host("127.0.0.2") && loopback_host("::1"));
    assert!(!loopback_host("localhost.example.com") && !loopback_host("10.0.0.1"));
    assert!(origin_allowed("http://localhost:5173") && origin_allowed("https://127.0.0.1/"));
    assert!(!origin_allowed("http://localhost.evil.example") && !origin_allowed("file://") && !origin_allowed("null"));
    assert_eq!(bearer("Bearer abc"), Some("abc"));
    assert_eq!(bearer("bearer  abc "), Some("abc"));
    assert_eq!(bearer("Basic abc"), None);
    assert!(same(b"abc", b"abc") && !same(b"abc", b"abx") && !same(b"abc", b"ab"));
}

fn discover_request(addr: SocketAddr, framing: &str, body: &str) -> String {
    format!("POST {ENDPOINT} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n{framing}\r\n\r\n{body}")
}

fn discover_body() -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}).to_string()
}

#[test]
fn blank_lines_cannot_hold_a_connection_open() {
    let rig = Rig::with(|c| c.limits.request_timeout = Duration::from_millis(500));
    let mut stream = TcpStream::connect(rig.running.addr()).expect("connect");
    stream.set_read_timeout(Some(Duration::from_millis(50))).expect("timeout");
    let start = Instant::now();
    // A CR every 50 ms, which is never a request line: the server ends it well within 3 s.
    let ended = loop {
        if start.elapsed() > Duration::from_secs(3) {
            break false;
        }
        if stream.write_all(b"\r").is_err() {
            break true;
        }
        let mut buf = [0u8; 256];
        match stream.read(&mut buf) {
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            _ => break true,
        }
    };
    assert!(ended, "a trickle of blank lines held the connection for {:?}", start.elapsed());
    // Many blank lines before a request at once: refused, not skipped.
    let addr = rig.running.addr();
    let flood =
        format!("{}{}", "\r\n".repeat(32), discover_request(addr, &format!("Content-Length: {}", discover_body().len()), &discover_body()));
    let reply = raw_exchange(addr, flood.as_bytes(), Duration::from_secs(2));
    assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    // One blank line is allowed (RFC 9112).
    let one = format!(
        "\r\n{}",
        discover_request(addr, &format!("Content-Length: {}\r\nConnection: close", discover_body().len()), &discover_body())
    );
    assert!(raw_exchange(addr, one.as_bytes(), Duration::from_secs(2)).starts_with("HTTP/1.1 200"));
}

#[test]
fn the_first_request_head_has_one_deadline_from_the_connection() {
    let rig = Rig::with(|c| c.limits.request_timeout = Duration::from_millis(600));
    let addr = rig.running.addr();
    let request = discover_request(addr, &format!("Content-Length: {}\r\nConnection: close", discover_body().len()), &discover_body());
    // Silent for 400 ms, then a head sent over 400 ms more: 800 ms after the connection opened.
    let mut stream = TcpStream::connect(addr).expect("connect");
    // Set before the server can close the socket: macOS refuses SO_RCVTIMEO (EINVAL) on a socket
    // its peer already shut.
    stream.set_read_timeout(Some(Duration::from_secs(2))).expect("timeout");
    std::thread::sleep(Duration::from_millis(400));
    let (a, b) = request.split_at(20);
    let _ = stream.write_all(a.as_bytes());
    std::thread::sleep(Duration::from_millis(400));
    let _ = stream.write_all(b.as_bytes());
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    assert!(!reply.contains("200 OK"), "a head that arrived after its deadline was served: {reply}");
    // The same head in time is served.
    assert!(raw_exchange(addr, request.as_bytes(), Duration::from_secs(2)).starts_with("HTTP/1.1 200"));
}

#[test]
fn a_body_on_a_get_or_delete_closes_the_connection_and_is_never_a_request() {
    let rig = Rig::new();
    let addr = rig.running.addr();
    let inner = discover_request(addr, &format!("Content-Length: {}", discover_body().len()), &discover_body());
    let session = "Mcp-Session-Id: 0123456789abcdef0123456789abcdef";
    for (method, framing, body) in [
        ("DELETE", format!("Content-Length: {}", inner.len()), inner.clone()),
        ("GET", "Transfer-Encoding: chunked".to_owned(), format!("{:x}\r\n{inner}\r\n0\r\n\r\n", inner.len())),
        ("OPTIONS", format!("Content-Length: {}", inner.len()), inner.clone()),
    ] {
        let request =
            format!("{method} {ENDPOINT} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n{session}\r\n{framing}\r\n\r\n{body}");
        let reply = raw_exchange(addr, request.as_bytes(), Duration::from_secs(2));
        assert!(reply.starts_with("HTTP/1.1 400"), "{method}: {reply}");
        assert!(reply.contains("Connection: close"), "{method}: {reply}");
        assert_eq!(reply.matches("HTTP/1.1 ").count(), 1, "{method}: the body was read as a request: {reply}");
    }
    // An empty body (`Content-Length: 0`) is no body: the DELETE is answered as one (no such
    // session), not refused for its framing.
    let request = format!(
        "DELETE {ENDPOINT} HTTP/1.1\r\nHost: {addr}\r\n{session}\r\nContent-Length: 0\r\n\r\n{}",
        discover_request(addr, &format!("Content-Length: {}\r\nConnection: close", discover_body().len()), &discover_body())
    );
    let reply = raw_exchange(addr, request.as_bytes(), Duration::from_secs(2));
    assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
}

#[test]
fn an_error_closes_the_connection() {
    let rig = Rig::new();
    let mut client = rig.client();
    for older in [false, true] {
        // A body that isn't JSON; an older revision's request with no session.
        let reply = if older { client.request("tools/list", json!({})).0 } else { client.post_raw(b"{not json", &[]) };
        assert_eq!((reply.status, reply.header("connection")), (400, Some("close")), "{reply:?}");
        let mut rest = Vec::new();
        let _ = client.reader.read_to_end(&mut rest);
        assert!(rest.is_empty(), "nothing follows: {rest:?}");
        client.reconnect();
    }
    // A success keeps it.
    let reply = client.post(&json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}));
    assert_eq!((reply.status, reply.header("connection")), (200, None));
}

#[test]
fn framing_is_parsed_strictly() {
    let rig = Rig::new();
    let addr = rig.running.addr();
    let body = discover_body();
    let chunked = |size: &str| format!("{size}\r\n{body}\r\n0\r\n\r\n");
    let hex = format!("{:x}", body.len());
    for (framing, sent) in [
        (format!("Content-Length: +{}", body.len()), body.clone()),
        ("Transfer-Encoding: chunked\r\nTransfer-Encoding: gzip".to_owned(), chunked(&hex)),
        ("Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked".to_owned(), chunked(&hex)),
        ("Transfer-Encoding: gzip, chunked".to_owned(), chunked(&hex)),
        ("Transfer-Encoding: chunked".to_owned(), chunked(&format!("+{hex}"))),
        ("Transfer-Encoding: chunked".to_owned(), chunked(&format!(" {hex}"))),
        ("Transfer-Encoding: chunked".to_owned(), chunked(&format!("0x{hex}"))),
    ] {
        let reply = raw_exchange(addr, discover_request(addr, &framing, &sent).as_bytes(), Duration::from_secs(2));
        assert!(reply.starts_with("HTTP/1.1 400"), "{framing:?} {sent:?}: {reply}");
        assert!(reply.contains("Connection: close"), "{framing:?}: {reply}");
    }
    // The strict forms are served.
    for (framing, sent) in [
        (format!("Content-Length: {}\r\nConnection: close", body.len()), body.clone()),
        ("Transfer-Encoding: Chunked\r\nConnection: close".to_owned(), chunked(&format!("{hex};ext=1"))),
    ] {
        let reply = raw_exchange(addr, discover_request(addr, &framing, &sent).as_bytes(), Duration::from_secs(2));
        assert!(reply.starts_with("HTTP/1.1 200"), "{framing:?}: {reply}");
    }
}

#[test]
#[cfg(unix)]
fn a_linked_token_file_or_a_directory_others_can_write_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new("files");
    let private = |path: &Path| {
        std::fs::write(path, "0123456789abcdef0123\n").expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    };
    let real = scratch.0.join("real");
    private(&real);
    ensure_token_file(&real).expect("a private file in a private directory");
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let e = ensure_token_file(&link).expect_err("a link is not followed");
    assert!(e.to_string().contains("symbolic link"), "{e}");
    assert!(read_tokens(&link).is_err());
    let dir = scratch.0.join("dir");
    std::fs::create_dir(&dir).expect("mkdir");
    assert!(ensure_token_file(&dir).expect_err("a directory").to_string().contains("regular file"));
    // A directory others can write: someone else could have planted the file.
    let shared = scratch.0.join("shared");
    std::fs::create_dir(&shared).expect("mkdir");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).expect("chmod");
    private(&shared.join("t"));
    let e = ensure_token_file(&shared.join("t")).expect_err("a shared directory");
    assert!(e.to_string().contains("other users can write"), "{e}");
    assert!(ensure_token_file(&shared.join("new")).is_err(), "nor is a new token made there");
    // A state directory that is made is its owner's alone.
    let made = scratch.0.join("made/state/t");
    ensure_token_file(&made).expect("made");
    let mode = std::fs::metadata(scratch.0.join("made/state")).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn a_client_that_half_closes_still_gets_its_response() {
    let rig = Rig::new();
    let addr = rig.running.addr();
    for accept in ["application/json", "application/json, text/event-stream"] {
        let gate = rig.fake.gate("boot");
        rig.fake.answer("boot", Ok(json!({"boot_s": 1})));
        let body = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": modern(json!({"name": "boot", "arguments": {"device": "phone"}}), false)})
            .to_string();
        let mut client = Client::connect(addr);
        client.write_request("POST", &[("Accept", accept)], body.as_bytes());
        // Done sending (`nc -N`, HTTP/1.0-style clients); still reading.
        client.reader.get_ref().tcp().shutdown(Shutdown::Write).expect("half-close");
        std::thread::sleep(limits().ping * 5);
        gate.release();
        let reply = client.read_reply();
        assert_eq!(reply.status, 200, "{accept}: {reply:?}");
        let response = if reply.header("content-type") == Some("text/event-stream") {
            events_of(&reply.body).pop().expect("the response ends the stream")
        } else {
            reply.json()
        };
        assert_eq!(response["result"]["structuredContent"]["boot_s"], 1, "{accept}: {response}");
        rig.fake.gates.lock().expect("gates").clear();
    }
}

/// Per-call cost of a `tools/call` of `identity` with `sim-api`'s real method: over stdio (the
/// server reading lines from one OS pipe and writing to another, as `simee mcp` does) and over
/// HTTP on loopback, on one kept-alive connection and on a new connection per call.
///     cargo test --release -p simee-mcp http_call_overhead -- --ignored --nocapture
#[test]
#[ignore = "benchmark"]
fn http_call_overhead() {
    fn median_us(mut f: impl FnMut()) -> f64 {
        for _ in 0..200 {
            f();
        }
        let mut rounds: Vec<f64> = (0..5)
            .map(|_| {
                let start = Instant::now();
                let mut n = 0;
                while n < 50 || start.elapsed() < Duration::from_millis(500) {
                    f();
                    n += 1;
                }
                start.elapsed().as_secs_f64() * 1e6 / f64::from(n)
            })
            .collect();
        rounds.sort_by(f64::total_cmp);
        rounds[2]
    }
    let backend: Arc<dyn Backend> = Arc::new(crate::ApiBackend::new());
    let call = |id: u64| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": modern(json!({"name": "identity", "arguments": {}}), false)})
            .to_string()
    };

    let (to_server, mut client_out) = {
        let (r, w) = std::io::pipe().expect("pipe");
        (r, w)
    };
    let (mut client_in, from_server) = {
        let (r, w) = std::io::pipe().expect("pipe");
        (BufReader::new(r), Mutex::new(w))
    };
    let stdio_backend = backend.clone();
    let stdio = std::thread::spawn(move || {
        let sink: Sink = Arc::new(move |line: String| {
            let mut w = from_server.lock().expect("w");
            let _ = w.write_all(line.as_bytes()).and_then(|()| w.write_all(b"\n"));
        });
        crate::serve(stdio_backend, BufReader::new(to_server), sink, Toolsets::all()).expect("serve");
    });
    let mut line = String::new();
    let mut n = 0;
    let stdio_us = median_us(|| {
        n += 1;
        client_out.write_all(format!("{}\n", call(n)).as_bytes()).expect("write");
        line.clear();
        client_in.read_line(&mut line).expect("read");
    });
    drop(client_out);
    stdio.join().expect("stdio server");

    let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
    config.toolsets = Toolsets::all();
    config.no_auth = true;
    let running = start(backend, config).expect("start");
    let mut client = Client::connect(running.addr());
    let kept_us = median_us(|| {
        let id = client.next;
        client.next += 1;
        let reply = client.post_raw(call(id).as_bytes(), &[]);
        assert_eq!(reply.status, 200);
    });
    let addr = running.addr();
    let fresh_us = median_us(|| {
        let mut c = Client::connect(addr);
        c.headers.push(("Connection".into(), "close".into()));
        assert_eq!(c.post_raw(call(1).as_bytes(), &[]).status, 200);
    });
    let tokened = Scratch::new("bench");
    let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
    config.toolsets = Toolsets::all();
    config.token_file = Some(tokened.0.join("t"));
    let authed = start(Arc::new(crate::ApiBackend::new()), config).expect("start");
    let mut client = Client::connect(authed.addr());
    client.token = Some(std::fs::read_to_string(tokened.0.join("t")).expect("token").trim().to_owned());
    let token_us = median_us(|| {
        let id = client.next;
        client.next += 1;
        assert_eq!(client.post_raw(call(id).as_bytes(), &[]).status, 200);
    });
    println!("identity tools/call, µs per call (median of 5 rounds)");
    println!("stdio (pipes)                    {stdio_us:>8.1}");
    println!("HTTP, kept-alive connection      {kept_us:>8.1}");
    println!("HTTP, kept-alive, bearer token   {token_us:>8.1}");
    println!("HTTP, new connection per call    {fresh_us:>8.1}");
}
