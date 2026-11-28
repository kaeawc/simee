//! The transport over TLS, against real loopback sockets: a handshake in process, certificates that
//! are wrong, plaintext sent to a TLS port, and the authentication, slowloris and body bounds that
//! must hold on this path as they do on the plain one.
//!
//! Nothing here asserts how long something took: a connection that must be cut off is one whose
//! trickle of bytes ends in a failed write before it is all sent, and that cannot happen unless the
//! server closed it.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};

use super::*;

/// The server started with a self-signed certificate in `dir`, and what a client needs to trust it.
struct Secure {
    rig: Rig,
    config: Arc<ClientConfig>,
    dir: PathBuf,
    _scratch: Scratch,
}

fn loopback() -> ServerName<'static> {
    ServerName::IpAddress(IpAddr::from(Ipv4Addr::LOCALHOST).into())
}

fn client_config(trusted: &[CertificateDer<'static>]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for cert in trusted {
        roots.add(cert.clone()).expect("a root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

fn certs_in(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem).collect::<Result<_, _>>().expect("certificates")
}

impl Secure {
    fn new(tag: &str) -> Secure {
        Secure::with(tag, |_| {})
    }

    /// Requires the token `authed` returns.
    fn authed(tag: &str) -> (Secure, String) {
        let secure = Secure::with(tag, |c| {
            c.no_auth = false;
        });
        let token = std::fs::read_to_string(secure.dir.join("../state/mcp-token")).expect("token").trim().to_owned();
        (secure, token)
    }

    fn with(tag: &str, adjust: impl FnOnce(&mut Config)) -> Secure {
        let scratch = Scratch::new(tag);
        let dir = scratch.0.join("tls");
        let token_file = scratch.0.join("state/mcp-token");
        let rig = Rig::with(|c| {
            c.tls = Some(Tls::SelfSigned { dir: Some(dir.clone()), names: Vec::new() });
            c.token_file = Some(token_file.clone());
            adjust(c);
            // A token file is only for a server that checks tokens.
            if c.no_auth {
                c.token_file = None;
            }
        });
        let pem = std::fs::read(dir.join("mcp-tls.pem")).expect("the certificate file");
        Secure { rig, config: client_config(&certs_in(&pem)), dir, _scratch: scratch }
    }

    fn addr(&self) -> SocketAddr {
        self.rig.running.addr()
    }

    fn client(&self) -> Client {
        Client::connect_with(self.addr(), Some((self.config.clone(), loopback())))
    }

    fn wire(&self) -> Wire {
        Wire::open(self.addr(), &Some((self.config.clone(), loopback())))
    }
}

/// Polls until `done`, for as long as a test may take (it only gives up when the server is stuck).
fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn no_connections(rig: &Rig) {
    until("every connection to close", || rig.running.shared.connections.load(Ordering::SeqCst) == 0);
}

/// Writes `bytes` one at a time with a pause between, until the server closes the connection (a
/// write fails) or they are all sent. Returns how many went out.
fn trickle(mut out: impl Write, bytes: &[u8]) -> usize {
    for (sent, byte) in bytes.iter().enumerate() {
        if out.write_all(&[*byte]).and_then(|()| out.flush()).is_err() {
            return sent;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bytes.len()
}

fn start_error(adjust: impl FnOnce(&mut Config)) -> std::io::Error {
    let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
    config.no_auth = true;
    adjust(&mut config);
    match start(Arc::new(Fake::with_devices()), config) {
        Ok(_) => panic!("the server started"),
        Err(e) => e,
    }
}

#[test]
fn a_client_that_trusts_the_certificate_calls_over_tls_1_3_with_a_token() {
    let (secure, token) = Secure::authed("tls-call");
    let mut client = secure.client();
    client.token = Some(token);
    let discover = client.call("server/discover", json!({}));
    assert_eq!(discover["result"]["supportedVersions"][0], MODERN);
    // The negotiated session is TLS 1.3 and says `http/1.1`.
    let Wire::Tls(session) = client.reader.get_ref() else { panic!("a TLS client") };
    assert_eq!(session.conn.protocol_version(), Some(rustls::ProtocolVersion::TLSv1_3));
    assert_eq!(session.conn.alpn_protocol(), Some(&b"http/1.1"[..]));
    // An older client's session works on it too, and a call streams.
    let mut older = secure.client();
    older.token = client.token.clone();
    older.initialize("2025-11-25");
    assert_eq!(older.call("ping", json!({}))["result"], json!({}));
    // The certificate also answers to `localhost`.
    let mut by_name = Client::connect_with(secure.addr(), Some((secure.config.clone(), "localhost".try_into().expect("a name"))));
    by_name.token = client.token.clone();
    assert_eq!(by_name.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    // The fingerprint the server printed is the one of the certificate it serves.
    let pem = std::fs::read(secure.dir.join("mcp-tls.pem")).expect("pem");
    let served = tls::fingerprint(&certs_in(&pem)[0]);
    assert_eq!(secure.rig.running.tls_fingerprint(), Some(served.as_str()));
    assert_eq!(served.len(), 32 * 3 - 1, "{served}");
}

#[test]
fn a_certificate_and_key_from_files_are_served_and_bad_ones_are_refused_with_the_path() {
    let scratch = Scratch::new("tls-files");
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).expect("a certificate");
    let (cert, key) = (scratch.0.join("cert.pem"), scratch.0.join("key.pem"));
    std::fs::write(&cert, made.cert.pem()).expect("cert");
    std::fs::write(&key, made.signing_key.serialize_pem()).expect("key");
    let rig = Rig::with(|c| c.tls = Some(Tls::Files { cert: cert.clone(), key: key.clone() }));
    let mut client = Client::connect_with(rig.running.addr(), Some((client_config(&[made.cert.der().clone()]), loopback())));
    assert_eq!(client.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    assert_eq!(rig.running.tls_fingerprint(), Some(tls::fingerprint(made.cert.der()).as_str()));

    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("another");
    let other_key = scratch.0.join("other-key.pem");
    std::fs::write(&other_key, other.signing_key.serialize_pem()).expect("key");
    let e = start_error(|c| c.tls = Some(Tls::Files { cert: cert.clone(), key: other_key.clone() }));
    assert!(e.to_string().contains("server identity"), "a key that is not the certificate's: {e}");
    let missing = scratch.0.join("missing.pem");
    let e = start_error(|c| c.tls = Some(Tls::Files { cert: missing.clone(), key: key.clone() }));
    assert!(e.to_string().contains("missing.pem"), "{e}");
    let empty = scratch.0.join("empty.pem");
    std::fs::write(&empty, "not a certificate\n").expect("empty");
    let e = start_error(|c| c.tls = Some(Tls::Files { cert: empty.clone(), key: key.clone() }));
    assert!(e.to_string().contains("no certificate"), "{e}");
    let e = start_error(|c| c.tls = Some(Tls::Files { cert: cert.clone(), key: empty.clone() }));
    assert!(e.to_string().contains("empty.pem"), "a key file with no key: {e}");
}

#[test]
fn a_client_that_does_not_trust_the_certificate_is_refused_and_the_server_carries_on() {
    let secure = Secure::new("tls-untrusted");
    // Trusting some other certificate: the handshake fails on the client's side.
    let other = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("another");
    let mut wire = Wire::open(secure.addr(), &Some((client_config(&[other.cert.der().clone()]), loopback())));
    let e = wire.write_all(b"GET /mcp HTTP/1.1\r\n\r\n").and_then(|()| wire.flush()).expect_err("an untrusted certificate");
    assert!(e.to_string().contains("UnknownIssuer"), "{e}");
    // Trusting this one, but expecting another name.
    let name: ServerName<'static> = "not-this-host.example".try_into().expect("a name");
    let mut wire = Wire::open(secure.addr(), &Some((secure.config.clone(), name)));
    let e = wire.write_all(b"GET /mcp HTTP/1.1\r\n\r\n").and_then(|()| wire.flush()).expect_err("a certificate for other names");
    assert!(e.to_string().contains("not valid for name"), "{e}");
    // Neither took the server down, or held its connections.
    assert_eq!(secure.client().call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    no_connections(&secure.rig);
}

#[test]
fn plaintext_or_garbage_sent_to_a_tls_port_is_refused_cleanly() {
    let secure = Secure::with("tls-plain", |c| c.limits.request_timeout = Duration::from_secs(1));
    let addr = secure.addr();
    // HTTP in the clear: told, in the clear, that this is a TLS port, and closed.
    let request = format!("GET {ENDPOINT} HTTP/1.1\r\nHost: {addr}\r\n\r\n");
    let reply = raw_exchange(addr, request.as_bytes(), Duration::from_secs(2));
    assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    assert!(reply.contains("TLS") && reply.contains("https://"), "{reply}");
    let reply = raw_exchange(addr, discover_request(addr, "Content-Length: 2", "{}").as_bytes(), Duration::from_secs(2));
    assert!(reply.starts_with("HTTP/1.1 400") && !reply.contains("200 OK"), "a POST is not served either: {reply}");
    // Something that is not HTTP and not TLS; and a TLS record that is not a handshake. Neither is
    // answered with a response, and both end.
    for junk in [&b"\x80\x01\x02\x03 not tls"[..], &b"\x16\x03\x01\xff\xff junk junk junk"[..], &b"\x16\x03\x03\x00\x05hello"[..]] {
        let reply = raw_exchange(addr, junk, Duration::from_secs(2));
        assert!(!reply.contains("200 OK") && !reply.contains("result"), "{junk:?}: {reply:?}");
    }
    assert_eq!(secure.client().call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    no_connections(&secure.rig);
}

#[test]
fn authentication_is_enforced_over_tls() {
    let (secure, token) = Secure::authed("tls-auth");
    let mut client = secure.client();
    let (reply, _) = client.request("server/discover", json!({}));
    assert_eq!((reply.status, reply.header("www-authenticate")), (401, Some("Bearer realm=\"simee\"")), "{reply:?}");
    client.reconnect();
    client.token = Some("not-the-token-but-long-enough".into());
    let (reply, _) = client.request("server/discover", json!({}));
    assert_eq!(reply.status, 401, "{reply:?}");
    client.reconnect();
    // The token in a query string or another scheme is no token.
    client.headers.push(("Authorization".into(), format!("Basic {token}")));
    let (reply, _) = client.request("server/discover", json!({}));
    assert_eq!(reply.status, 401, "{reply:?}");
    client.reconnect();
    client.headers.clear();
    client.token = Some(token.clone());
    assert_eq!(client.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    // A session belongs to the token that opened it, here too.
    let mut older = secure.client();
    older.token = Some(token);
    older.initialize("2025-11-25");
    let session = older.session.clone();
    let mut thief = secure.client();
    thief.token = Some("another-token-that-is-long-enough".into());
    thief.session = session;
    let (reply, _) = thief.request("ping", json!({}));
    assert_eq!(reply.status, 401, "{reply:?}");
}

#[test]
fn the_connection_limits_hold_over_tls() {
    let (secure, token) = Secure::authed("tls-limits");
    let cap = secure.rig.running.shared.config.limits.max_connections_per_peer;
    // Connections that have said nothing yet count against the peer's share, handshake or none.
    let held: Vec<TcpStream> = (0..cap).map(|_| TcpStream::connect(secure.addr()).expect("connect")).collect();
    until("the held connections to be counted", || secure.rig.running.shared.connections.load(Ordering::SeqCst) >= cap);
    // One more is turned away in the clear (there is no handshake to carry a TLS answer).
    let reply = raw_exchange(secure.addr(), b"", Duration::from_secs(2));
    assert!(reply.starts_with("HTTP/1.1 503") && reply.contains("from this address"), "{reply}");
    drop(held);
    no_connections(&secure.rig);
    let mut client = secure.client();
    client.token = Some(token);
    assert_eq!(client.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
}

#[test]
fn a_silent_connection_and_a_trickled_handshake_are_cut_off_at_the_deadline() {
    let secure = Secure::with("tls-handshake", |c| c.limits.request_timeout = Duration::from_millis(300));
    // Says nothing: closed by the server (a read ends with EOF, not with our own timeout).
    let mut silent = TcpStream::connect(secure.addr()).expect("connect");
    silent.set_read_timeout(Some(WAIT)).expect("timeout");
    let mut byte = [0u8; 1];
    assert_eq!(silent.read(&mut byte).expect("the server closes it"), 0);
    // A real ClientHello a byte at a time: the deadline is the connection's, not each read's, so the
    // server closes it before the last byte goes out.
    let mut hello = ClientConnection::new(secure.config.clone(), loopback()).expect("client");
    let mut bytes = Vec::new();
    hello.write_tls(&mut bytes).expect("the ClientHello");
    assert!(bytes.len() > 100, "{}", bytes.len());
    let tcp = TcpStream::connect(secure.addr()).expect("connect");
    let sent = trickle(&tcp, &bytes);
    assert!(sent < bytes.len(), "the whole ClientHello went out in a trickle: the handshake has no deadline");
    drop((tcp, silent));
    no_connections(&secure.rig);
    assert_eq!(secure.client().call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
}

#[test]
fn a_request_trickled_over_tls_is_cut_off_whether_in_many_records_or_one() {
    let secure = Secure::with("tls-slowloris", |c| c.limits.request_timeout = Duration::from_millis(300));
    let addr = secure.addr();
    let request = discover_request(addr, &format!("Content-Length: {}\r\nConnection: close", discover_body().len()), &discover_body());
    // Many records: the head a byte per record.
    let mut wire = secure.wire();
    let sent = trickle(&mut wire, request.as_bytes());
    assert!(sent < request.len(), "a head trickled in records of one byte was served whole");
    // One record, its bytes a trickle: a timeout on each socket read would never see a pause, and
    // would serve the request when the record was complete. One deadline for the read closes it.
    let mut client = ClientConnection::new(secure.config.clone(), loopback()).expect("client");
    let mut tcp = TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(WAIT)).expect("timeout");
    while client.is_handshaking() {
        client.complete_io(&mut tcp).expect("the handshake");
    }
    client.writer().write_all(request.as_bytes()).expect("plaintext");
    let mut record = Vec::new();
    client.write_tls(&mut record).expect("the record");
    assert!(record.len() > request.len(), "one record, with its header and tag");
    let sent = trickle(&tcp, &record);
    assert!(sent < record.len(), "a record trickled a byte at a time was read whole");
    drop((wire, tcp));
    // The same request at once is served.
    assert_eq!(secure.client().call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    no_connections(&secure.rig);
}

#[test]
fn a_body_trickled_over_tls_is_cut_off() {
    let secure = Secure::with("tls-slow-body", |c| c.limits.request_timeout = Duration::from_millis(300));
    let mut wire = secure.wire();
    let head =
        format!("POST {ENDPOINT} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n", secure.addr());
    wire.write_all(head.as_bytes()).expect("head");
    wire.flush().expect("flush");
    let sent = trickle(&mut wire, &[b' '; 1000]);
    assert!(sent < 1000, "a body sent a byte at a time was read to its end");
    drop(wire);
    no_connections(&secure.rig);
}

#[test]
fn body_and_head_bounds_hold_over_tls() {
    let secure = Secure::new("tls-bounds");
    bounds(&secure);
    no_connections(&secure.rig);
}

fn bounds(secure: &Secure) {
    let max = limits().max_body;
    // Declared too large: refused before the body is read, and the connection closed.
    let mut client = secure.client();
    client.write_request("POST", &[("Content-Length", &(max + 1).to_string())], b"");
    let reply = client.read_reply();
    assert_eq!((reply.status, reply.header("connection")), (413, Some("close")), "{reply:?}");
    // Chunked past the limit.
    let mut client = secure.client();
    let head = format!(
        "POST {ENDPOINT} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
        secure.addr()
    );
    client.reader.get_mut().write_all(head.as_bytes()).expect("head");
    let chunk = vec![b' '; 16 << 10];
    for _ in 0..(max / chunk.len() + 1) {
        let wire = client.reader.get_mut();
        if wire
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .and_then(|()| wire.write_all(&chunk))
            .and_then(|()| wire.write_all(b"\r\n"))
            .is_err()
        {
            break;
        }
    }
    let reply = client.read_reply();
    assert_eq!((reply.status, reply.header("connection")), (413, Some("close")), "{reply:?}");
    // A chunked body within it is read.
    let mut client = secure.client();
    let body = discover_body();
    let (a, b) = body.split_at(10);
    client.reader.get_mut().write_all(head.as_bytes()).expect("head");
    client.reader.get_mut().write_all(format!("{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n", a.len(), b.len()).as_bytes()).expect("chunks");
    assert_eq!(client.read_reply().json()["id"], 1);
    // Smuggling shapes are refused and close the connection.
    let mut client = secure.client();
    client.write_request("POST", &[("Transfer-Encoding", "chunked"), ("Content-Length", "5")], b"0\r\n\r\n");
    let reply = client.read_reply();
    assert_eq!((reply.status, reply.header("connection")), (400, Some("close")), "{reply:?}");
    // A head past its limit.
    let mut client = secure.client();
    client.write_request("POST", &[("X-Big", &"x".repeat(limits().max_head + 1))], b"{}");
    assert_eq!(client.read_reply().status, 431);
    // A malformed body after a good request on the same connection: refused, then closed.
    let mut client = secure.client();
    assert_eq!(client.call("server/discover", json!({}))["result"]["supportedVersions"][0], MODERN);
    let reply = client.post_raw(b"{not json", &[]);
    assert_eq!((reply.status, reply.header("connection")), (400, Some("close")), "{reply:?}");
}

#[test]
fn a_client_that_ends_its_tls_session_still_gets_its_response() {
    let secure = Secure::new("tls-half-close");
    for accept in ["application/json", "application/json, text/event-stream"] {
        let gate = secure.rig.fake.gate("boot");
        secure.rig.fake.answer("boot", Ok(json!({"boot_s": 1})));
        let body = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": modern(json!({"name": "boot", "arguments": {"device": "phone"}}), false)})
            .to_string();
        let mut client = secure.client();
        client.write_request("POST", &[("Accept", accept)], body.as_bytes());
        // `close_notify` and a FIN: done sending, still reading (TLS 1.3 allows it).
        let Wire::Tls(session) = client.reader.get_mut() else { panic!("a TLS client") };
        session.conn.send_close_notify();
        session.conn.complete_io(&mut session.sock).expect("close_notify");
        session.sock.shutdown(Shutdown::Write).expect("half-close");
        // The server sees that the client has finished, which is not the client being gone: a
        // client that takes an event stream is pinged to find out.
        std::thread::sleep(limits().ping * 5);
        gate.release();
        let reply = client.read_reply();
        assert_eq!(reply.status, 200, "{accept}: {reply:?}");
        let streamed = reply.header("content-type") == Some("text/event-stream");
        assert_eq!(streamed, accept.contains("event-stream"), "{accept}: {reply:?}");
        let response = if streamed { events_of(&reply.body).pop().expect("the response ends the stream") } else { reply.json() };
        assert_eq!(response["result"]["structuredContent"]["boot_s"], 1, "{accept}: {response}");
        secure.rig.fake.gates.lock().expect("gates").clear();
    }
}

/// Per-call cost of an `identity` `tools/call` over TLS, to set beside `http_call_overhead`'s HTTP
/// rows: on a kept-alive connection with a bearer token, and on a new connection per call, with
/// the client resuming its last session (what a client keeping one configuration does) and with
/// a full handshake each time.
///     cargo test --release -p simee-mcp --features tls https_call_overhead -- --ignored --nocapture
#[test]
#[ignore = "benchmark"]
fn https_call_overhead() {
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
    let call = |id: u64| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": modern(json!({"name": "identity", "arguments": {}}), false)})
            .to_string()
    };
    let scratch = Scratch::new("bench-tls");
    let dir = scratch.0.join("tls");
    let token_file = scratch.0.join("t");
    let mut config = Config::new("127.0.0.1:0".parse().expect("addr"));
    config.toolsets = Toolsets::all();
    config.token_file = Some(token_file.clone());
    config.tls = Some(Tls::SelfSigned { dir: Some(dir.clone()), names: Vec::new() });
    let running = start(Arc::new(crate::ApiBackend::new()), config).expect("start");
    let token = std::fs::read_to_string(&token_file).expect("token").trim().to_owned();
    let trusted = certs_in(&std::fs::read(dir.join("mcp-tls.pem")).expect("pem"));
    let shared = client_config(&trusted);
    let addr = running.addr();
    let mut client = Client::connect_with(addr, Some((shared.clone(), loopback())));
    client.token = Some(token.clone());
    let kept_us = median_us(|| {
        let id = client.next;
        client.next += 1;
        assert_eq!(client.post_raw(call(id).as_bytes(), &[]).status, 200);
    });
    let per_call = |config: &dyn Fn() -> Arc<ClientConfig>, kind: rustls::HandshakeKind| {
        median_us(|| {
            let mut c = Client::connect_with(addr, Some((config(), loopback())));
            c.token = Some(token.clone());
            c.headers.push(("Connection".into(), "close".into()));
            assert_eq!(c.post_raw(call(1).as_bytes(), &[]).status, 200);
            let Wire::Tls(session) = c.reader.get_ref() else { panic!("a TLS client") };
            assert_eq!(session.conn.handshake_kind(), Some(kind));
        })
    };
    // The kept-alive client's first handshake gave `shared` a session to resume.
    let resumed_us = per_call(&|| shared.clone(), rustls::HandshakeKind::Resumed);
    let full_us = per_call(&|| client_config(&trusted), rustls::HandshakeKind::Full);
    println!("identity tools/call over TLS 1.3, µs per call (median of 5 rounds)");
    println!("HTTPS, kept-alive, bearer token        {kept_us:>8.1}");
    println!("HTTPS, new connection, resumed         {resumed_us:>8.1}");
    println!("HTTPS, new connection, full handshake  {full_us:>8.1}");
}
