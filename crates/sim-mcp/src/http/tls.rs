//! TLS for the HTTP transport (`simee mcp --http --tls-cert/--tls-key`, `--tls-self-signed`): rustls
//! with the ring provider, TLS 1.3 only, ALPN `http/1.1`.
//!
//! The handshake runs on the connection's own thread and holds one of its slots, under the same
//! deadline as a request head, so a client that stalls or trickles a handshake is cut off like one
//! that trickles a request, and TLS does not raise the connection limits. Without the `tls` cargo
//! feature this module is the same API with no server: asking for TLS is an error that says how to
//! build it.

#[cfg(feature = "tls")]
pub(super) use real::{Server, Session};

#[cfg(all(test, feature = "tls"))]
pub(super) use real::fingerprint;

#[cfg(not(feature = "tls"))]
pub(super) use absent::{Server, Session};

#[cfg(not(feature = "tls"))]
mod absent {
    use std::io;
    use std::net::{IpAddr, TcpStream};
    use std::time::Instant;

    use crate::http::conn::Conn;
    use crate::http::{Peer, Tls};

    /// There is no TLS in this build, so there is never a server.
    pub(in crate::http) enum Server {}

    pub(in crate::http) enum Session {}

    impl Server {
        pub(in crate::http) fn new(_: &Tls, _: IpAddr) -> io::Result<Server> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this build of simee has no TLS: build it with `--features tls` (cargo build -p simee --features tls), or put a TLS proxy in front",
            ))
        }

        pub(in crate::http) fn fingerprint(&self) -> &str {
            match *self {}
        }

        pub(in crate::http) fn certificate(&self) -> &std::path::Path {
            match *self {}
        }

        pub(in crate::http) fn accept(&self, _: TcpStream, _: Instant) -> io::Result<Conn> {
            match *self {}
        }
    }

    impl Session {
        pub(in crate::http) fn read(&self, _: &TcpStream, _: Instant, _: &mut [u8]) -> io::Result<usize> {
            match *self {}
        }

        pub(in crate::http) fn write(&self, _: &TcpStream, _: &[u8]) -> io::Result<usize> {
            match *self {}
        }

        pub(in crate::http) fn flush(&self, _: &TcpStream) -> io::Result<()> {
            match *self {}
        }

        pub(in crate::http) fn peer_state(&self, _: &TcpStream) -> Peer {
            match *self {}
        }

        pub(in crate::http) fn close_notify(&self, _: &TcpStream) {
            match *self {}
        }
    }
}

#[cfg(feature = "tls")]
mod real {
    use std::cell::{Cell, RefCell};
    use std::io::{self, Read, Write};
    use std::net::{IpAddr, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls::{ServerConfig, ServerConnection};

    use crate::http::conn::{Conn, linger};
    use crate::http::{Peer, Tls, ensure_private_dir, error_body, lock, open_token_file, random_hex, remaining, state_dir, write_response};
    use crate::rpc::INVALID_REQUEST;

    /// The self-signed certificate and its key, in one file so that replacing it is one rename.
    pub(in crate::http) const SELF_SIGNED_FILE: &str = "mcp-tls.pem";
    /// The same certificate alone: what a client is given to trust (the file above holds the key).
    pub(in crate::http) const CERTIFICATE_FILE: &str = "mcp-tls-cert.pem";
    /// How long a generated certificate is valid.
    const VALID_DAYS: i64 = 730;
    /// A cached certificate that expires within this is made anew.
    const RENEW_DAYS: i64 = 30;
    const MAX_PEM_FILE: u64 = 1 << 20;
    /// A TLS record of a handshake starts with this byte; an HTTP request does not.
    const HANDSHAKE_RECORD: u8 = 0x16;

    pub(in crate::http) struct Server {
        config: Arc<ServerConfig>,
        /// The certificate file to give clients.
        certificate: PathBuf,
        fingerprint: String,
        /// When a failed handshake was last reported: scanners would otherwise fill the log.
        last_report: Mutex<Option<Instant>>,
    }

    impl Server {
        /// The certificate chain and key `tls` names (made, for `SelfSigned`, if there are none yet),
        /// as a server configuration. `bind` is where the server listens: a self-signed
        /// certificate names it.
        pub(in crate::http) fn new(tls: &Tls, bind: IpAddr) -> io::Result<Server> {
            let mut certificate = PathBuf::new();
            let (chain, key) = match tls {
                Tls::Files { cert, key } => {
                    warn_if_readable(key);
                    cert.clone_into(&mut certificate);
                    load_files(cert, key)?
                }
                Tls::SelfSigned { dir, names } => {
                    let dir = match dir {
                        Some(dir) => dir.clone(),
                        None => state_dir().ok_or_else(|| {
                            io::Error::new(io::ErrorKind::NotFound, "no state directory for the certificate: set HOME or SIMEE_STATE_DIR")
                        })?,
                    };
                    certificate = dir.join(CERTIFICATE_FILE);
                    self_signed(&dir, &self_signed_names(bind, names), SystemTime::now())?
                }
            };
            let fingerprint = fingerprint(chain.first().ok_or_else(|| invalid("the certificate file holds no certificate"))?);
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = ServerConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(io::Error::other)?
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .map_err(|e| invalid(format!("the certificate and key do not make a server identity: {e}")))?;
            // The transport speaks HTTP/1.1 only: a client that offers `h2` is told to use this.
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(Server { config: Arc::new(config), certificate, fingerprint, last_report: Mutex::new(None) })
        }

        /// The certificate file clients can be given (never the key).
        pub(in crate::http) fn certificate(&self) -> &Path {
            &self.certificate
        }

        /// The leaf certificate's SHA-256, as `openssl x509 -fingerprint -sha256` prints it.
        pub(in crate::http) fn fingerprint(&self) -> &str {
            &self.fingerprint
        }

        /// Takes the TLS handshake of a new connection, which must be done by `deadline`. A client
        /// that speaks HTTP in the clear to this port is answered (in the clear) that this is a TLS
        /// port and closed.
        pub(in crate::http) fn accept(&self, tcp: TcpStream, deadline: Instant) -> io::Result<Conn> {
            match self.handshake(&tcp, deadline) {
                Ok(session) => Ok(Conn::secure(tcp, session)),
                Err(e) => {
                    self.report(&tcp, &e);
                    linger(&tcp);
                    Err(e)
                }
            }
        }

        fn handshake(&self, tcp: &TcpStream, deadline: Instant) -> io::Result<Session> {
            if first_byte(tcp, deadline)? != HANDSHAKE_RECORD {
                let _ = tcp.set_write_timeout(Some(Duration::from_secs(1)));
                let body = error_body(INVALID_REQUEST, "this port serves HTTPS (TLS 1.3): use an https:// URL");
                let _ = write_response(tcp, 400, &[], body.as_bytes(), true);
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "plaintext on a TLS port"));
            }
            let session = Session {
                conn: RefCell::new(ServerConnection::new(self.config.clone()).map_err(io::Error::other)?),
                client_done: Cell::new(false),
            };
            session.handshake(tcp, deadline)?;
            Ok(session)
        }

        /// Says on stderr why a handshake failed, at most once a second, and not for the ordinary
        /// ends (a client that left, or was too slow, or spoke plaintext).
        fn report(&self, tcp: &TcpStream, error: &io::Error) {
            use io::ErrorKind::*;
            if matches!(
                error.kind(),
                UnexpectedEof | TimedOut | WouldBlock | ConnectionReset | ConnectionAborted | BrokenPipe | InvalidInput
            ) {
                return;
            }
            let mut last = lock(&self.last_report);
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
                return;
            }
            *last = Some(Instant::now());
            let peer = tcp.peer_addr().map_or_else(|_| "a client".to_owned(), |a| a.to_string());
            eprintln!("simee mcp: TLS handshake with {peer} failed: {error}");
        }
    }

    /// The first byte a client sends, without taking it from the socket.
    fn first_byte(tcp: &TcpStream, deadline: Instant) -> io::Result<u8> {
        let mut byte = [0u8; 1];
        loop {
            tcp.set_read_timeout(Some(remaining(deadline)?))?;
            match tcp.peek(&mut byte) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => return Ok(byte[0]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// A TLS session over a connection's socket. Reads and writes happen on the connection's thread,
    /// one at a time, so the session needs no lock.
    pub(in crate::http) struct Session {
        conn: RefCell<ServerConnection>,
        /// The client has sent `close_notify`: it is done sending. (rustls has it too, but tells
        /// only the call that took it in, which may have been a read for a request.)
        client_done: Cell<bool>,
    }

    impl Session {
        /// Sends what the session has queued (records, alerts).
        fn send(conn: &mut ServerConnection, mut tcp: &TcpStream) -> io::Result<()> {
            while conn.wants_write() {
                conn.write_tls(&mut tcp)?;
            }
            Ok(())
        }

        /// Takes in one read's worth of records from the socket, which must arrive by `deadline`.
        /// `true` when the socket is at its end. A protocol error is answered with its alert.
        fn receive(&self, conn: &mut ServerConnection, mut tcp: &TcpStream, deadline: Instant) -> io::Result<bool> {
            loop {
                tcp.set_read_timeout(Some(remaining(deadline)?))?;
                match conn.read_tls(&mut tcp) {
                    Ok(0) => return Ok(true),
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            match conn.process_new_packets() {
                Ok(state) => {
                    self.client_done.set(self.client_done.get() || state.peer_has_closed());
                    Ok(false)
                }
                Err(e) => {
                    let _ = Session::send(conn, tcp);
                    Err(io::Error::new(io::ErrorKind::InvalidData, e))
                }
            }
        }

        fn handshake(&self, tcp: &TcpStream, deadline: Instant) -> io::Result<()> {
            let mut conn = self.conn.borrow_mut();
            while conn.is_handshaking() {
                Session::send(&mut conn, tcp)?;
                if self.receive(&mut conn, tcp, deadline)? {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
            }
            Session::send(&mut conn, tcp)
        }

        /// Plaintext from the client: what is already decrypted, else after taking in records
        /// until some is, all before `deadline` (one deadline for the whole read, however many
        /// socket reads it takes). 0 at the client's end.
        pub(in crate::http) fn read(&self, tcp: &TcpStream, deadline: Instant, buf: &mut [u8]) -> io::Result<usize> {
            let mut conn = self.conn.borrow_mut();
            loop {
                match conn.reader().read(buf) {
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    // The socket ended without a `close_notify`: the HTTP framing says what was cut.
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                    Err(e) => return Err(e),
                }
                Session::send(&mut conn, tcp)?;
                if self.receive(&mut conn, tcp, deadline)? {
                    return Ok(0);
                }
            }
        }

        pub(in crate::http) fn write(&self, tcp: &TcpStream, buf: &[u8]) -> io::Result<usize> {
            let mut conn = self.conn.borrow_mut();
            let taken = conn.writer().write(buf)?;
            Session::send(&mut conn, tcp)?;
            Ok(taken)
        }

        pub(in crate::http) fn flush(&self, tcp: &TcpStream) -> io::Result<()> {
            Session::send(&mut self.conn.borrow_mut(), tcp)
        }

        pub(in crate::http) fn peer_state(&self, tcp: &TcpStream) -> Peer {
            if self.client_done.get() {
                return Peer::Finished;
            }
            let mut conn = self.conn.borrow_mut();
            if tcp.set_nonblocking(true).is_err() {
                return Peer::Open;
            }
            let mut socket = tcp;
            let state = loop {
                if !conn.wants_read() {
                    break Peer::Open;
                }
                match conn.read_tls(&mut socket) {
                    Ok(0) => break Peer::Finished,
                    Ok(_) => match conn.process_new_packets() {
                        Ok(state) if state.peer_has_closed() => {
                            self.client_done.set(true);
                            break Peer::Finished;
                        }
                        Ok(_) => {}
                        Err(_) => break Peer::Broken,
                    },
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => break Peer::Open,
                    Err(_) => break Peer::Broken,
                }
            };
            let _ = tcp.set_nonblocking(false);
            state
        }

        /// Tells the client the session is over (best effort).
        pub(in crate::http) fn close_notify(&self, tcp: &TcpStream) {
            let mut conn = self.conn.borrow_mut();
            conn.send_close_notify();
            let _ = Session::send(&mut conn, tcp);
        }
    }

    fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }

    /// `AB:CD:…`, the SHA-256 of a DER certificate.
    pub(in crate::http) fn fingerprint(cert: &CertificateDer<'_>) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
        let hex: Vec<String> = digest.as_ref().iter().map(|b| format!("{b:02X}")).collect();
        hex.join(":")
    }

    type Identity = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

    fn load_files(cert: &Path, key: &Path) -> io::Result<Identity> {
        let named = |path: &Path, e: &dyn std::fmt::Display| invalid(format!("{}: {e}", path.display()));
        let chain = CertificateDer::pem_file_iter(cert)
            .map_err(|e| named(cert, &e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| named(cert, &e))?;
        if chain.is_empty() {
            return Err(invalid(format!("{}: no certificate in it (a PEM file with `BEGIN CERTIFICATE`)", cert.display())));
        }
        let key = PrivateKeyDer::from_pem_file(key).map_err(|e| named(key, &e))?;
        Ok((chain, key))
    }

    /// A key file others can read is the owner's to fix, but a server that keeps running with one
    /// should say so.
    fn warn_if_readable(key: &Path) {
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(key) {
            let mode = std::os::unix::fs::MetadataExt::mode(&meta);
            if mode & 0o077 != 0 {
                eprintln!("simee mcp: warning: {} can be read by other users (mode {:o}); `chmod 600` it", key.display(), mode & 0o777);
            }
        }
        #[cfg(not(unix))]
        let _ = key;
    }

    /// The names a self-signed certificate carries: the loopback names, where the server listens
    /// when that is one address, and any the caller adds (a host name clients will use).
    pub(in crate::http) fn self_signed_names(bind: IpAddr, extra: &[String]) -> Vec<String> {
        let mut names: Vec<String> = ["localhost", "127.0.0.1", "::1"].map(str::to_owned).into();
        if !bind.is_unspecified() && !bind.is_loopback() {
            names.push(bind.to_string());
        }
        for name in extra {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }

    /// The self-signed identity cached in `dir`, made first if there is none, it was made for other
    /// names or it is about to expire. The file is simee's: a private file in a private directory,
    /// which a symbolic link or another owner's file is refused for, as a token file is.
    pub(in crate::http) fn self_signed(dir: &Path, names: &[String], now: SystemTime) -> io::Result<Identity> {
        ensure_private_dir(dir)?;
        let path = dir.join(SELF_SIGNED_FILE);
        let existing = match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                let text = read_private(&path)?;
                match parse_cached(&text, names, now) {
                    Some(found) => Some((text, found)),
                    None => {
                        eprintln!(
                            "simee mcp: {} is for other names, expired or damaged: making a new certificate (clients pin its new fingerprint)",
                            path.display()
                        );
                        None
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let (text, found) = match existing {
            Some(existing) => existing,
            None => {
                let pem = generate(names, now)?;
                write_private(dir, &path, &pem)?;
                let found = parse_cached(&pem, names, now).ok_or_else(|| invalid("a certificate just made is not usable"))?;
                (pem, found)
            }
        };
        // The certificate alone, for clients to trust.
        let public = certificate_blocks(&text);
        let public_path = dir.join(CERTIFICATE_FILE);
        if std::fs::read_to_string(&public_path).ok().as_deref() != Some(public.as_str()) {
            write_private(dir, &public_path, &public)?;
        }
        Ok(found)
    }

    /// The `CERTIFICATE` blocks of a PEM text, and nothing else (no key).
    fn certificate_blocks(text: &str) -> String {
        let mut out = String::new();
        let mut inside = false;
        for line in text.lines() {
            inside |= line.starts_with("-----BEGIN CERTIFICATE-----");
            if inside {
                out.push_str(line);
                out.push('\n');
            }
            inside &= !line.starts_with("-----END CERTIFICATE-----");
        }
        out
    }

    const HEADER: &str = "# simee self-signed certificate:";

    /// What the cached file's first line says it was made for and until when.
    fn header(names: &[String], not_after: i64) -> String {
        format!("{HEADER} names={} not_after={not_after}\n", names.join(","))
    }

    fn parse_cached(text: &str, names: &[String], now: SystemTime) -> Option<Identity> {
        let line = text.lines().next()?.strip_prefix(HEADER)?.trim();
        let (listed, not_after) = line.strip_prefix("names=")?.rsplit_once(" not_after=")?;
        let renew_by = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64 + RENEW_DAYS * 86_400;
        if listed != names.join(",") || not_after.parse::<i64>().ok()? <= renew_by {
            return None;
        }
        let chain = CertificateDer::pem_slice_iter(text.as_bytes()).collect::<Result<Vec<_>, _>>().ok().filter(|c| !c.is_empty())?;
        let key = PrivateKeyDer::from_pem_slice(text.as_bytes()).ok()?;
        Some((chain, key))
    }

    /// A new certificate (ECDSA P-256, valid from yesterday for [`VALID_DAYS`]) for `names`, with
    /// its key, as the cached file's text.
    fn generate(names: &[String], now: SystemTime) -> io::Result<String> {
        let other = |e: rcgen::Error| io::Error::other(format!("making the self-signed certificate: {e}"));
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(other)?;
        let mut params = rcgen::CertificateParams::new(names.to_vec()).map_err(other)?;
        let today = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64).div_euclid(86_400);
        let (y, m, d) = civil_from_days(today - 1);
        params.not_before = rcgen::date_time_ymd(y, m, d);
        let (y, m, d) = civil_from_days(today + VALID_DAYS);
        params.not_after = rcgen::date_time_ymd(y, m, d);
        params.distinguished_name.push(rcgen::DnType::CommonName, "simee mcp (self-signed)");
        let cert = params.self_signed(&key).map_err(other)?;
        Ok(format!("{}{}{}", header(names, (today + VALID_DAYS) * 86_400), cert.pem(), key.serialize_pem()))
    }

    /// The calendar date of a day number (days since 1970-01-01), proleptic Gregorian.
    pub(in crate::http) fn civil_from_days(days: i64) -> (i32, u8, u8) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        (year as i32, month as u8, day as u8)
    }

    /// The text of a file the caller owns and no one else can read, opened without following a link.
    fn read_private(path: &Path) -> io::Result<String> {
        let file = open_token_file(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid(format!("{} is not a regular file", path.display())));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != crate::http::euid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} belongs to another user (uid {}): remove it, or point --tls-cert and --tls-key at your own",
                        path.display(),
                        meta.uid()
                    ),
                ));
            }
            if meta.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} can be read by other users (mode {:o}): `chmod 600` it, or delete it to make a new certificate",
                        path.display(),
                        meta.mode() & 0o777
                    ),
                ));
            }
        }
        if meta.len() > MAX_PEM_FILE {
            return Err(invalid(format!("{} is larger than a certificate file", path.display())));
        }
        let mut text = String::new();
        file.take(MAX_PEM_FILE).read_to_string(&mut text).map_err(|_| invalid(format!("{} is not text", path.display())))?;
        Ok(text)
    }

    /// Writes `text` to `path`, mode 0600, by a temporary file renamed over it: a reader sees the
    /// old file or the whole new one.
    fn write_private(dir: &Path, path: &Path, text: &str) -> io::Result<()> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(SELF_SIGNED_FILE);
        let temp = dir.join(format!("{name}.{}.tmp", random_hex(8)?));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&temp).and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        });
        let moved = written.and_then(|()| std::fs::rename(&temp, path));
        if moved.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        moved
    }
}

#[cfg(all(test, feature = "tls"))]
mod tests;
