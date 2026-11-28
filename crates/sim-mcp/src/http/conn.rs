//! What a connection reads and writes through: its TCP stream, or a TLS session over it.
//!
//! Reads are bounded by an absolute deadline ([`Conn::set_deadline`]), not by a timeout per socket
//! read. Over TCP the two are the same; over TLS one `read` may take many socket reads to complete a
//! record, and a client that trickles a record's bytes one at a time would otherwise never reach a
//! timeout that only bounds each of them (slowloris, through the encryption).

use std::cell::Cell;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

use super::{Peer, remaining, tls};

pub(super) struct Conn {
    tcp: TcpStream,
    /// The TLS session, when the server speaks TLS: `tcp` then carries only its records.
    session: Option<tls::Session>,
    /// When the read in progress, or the next one, must be done by.
    deadline: Cell<Option<Instant>>,
}

impl Conn {
    pub(super) fn plain(tcp: TcpStream) -> Conn {
        Conn { tcp, session: None, deadline: Cell::new(None) }
    }

    #[cfg(feature = "tls")]
    pub(super) fn secure(tcp: TcpStream, session: tls::Session) -> Conn {
        Conn { tcp, session: Some(session), deadline: Cell::new(None) }
    }

    /// Reads must finish by `deadline`; `Err` (`TimedOut`) if it has already passed.
    pub(super) fn set_deadline(&self, deadline: Instant) -> io::Result<()> {
        self.deadline.set(Some(deadline));
        self.tcp.set_read_timeout(Some(remaining(deadline)?))
    }

    /// Looks at the client's side without blocking or consuming a request: over TLS the records
    /// that arrived are taken in (their plaintext waits for the next read), because a client's
    /// `close_notify` is bytes in the socket and would otherwise look like a client still there.
    pub(super) fn peer_state(&self) -> Peer {
        if let Some(session) = &self.session {
            return session.peer_state(&self.tcp);
        }
        if self.tcp.set_nonblocking(true).is_err() {
            return Peer::Open;
        }
        let mut byte = [0u8; 1];
        let state = match self.tcp.peek(&mut byte) {
            Ok(0) => Peer::Finished,
            Ok(_) => Peer::Open,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => Peer::Open,
            Err(_) => Peer::Broken,
        };
        let _ = self.tcp.set_nonblocking(false);
        state
    }

    /// Closes the connection without losing the last response: a close with unread input in the
    /// socket resets it, and the client may never read what was written (a 413 sent before the
    /// body is read). So: end the TLS session, stop writing, read and drop what is still arriving
    /// for a moment, then close.
    pub(super) fn close(&self) {
        if let Some(session) = &self.session {
            let _ = self.tcp.set_write_timeout(Some(Duration::from_secs(1)));
            session.close_notify(&self.tcp);
        }
        linger(&self.tcp);
    }
}

/// Stops writing on a socket, then reads and drops what is still arriving for a moment.
pub(super) fn linger(mut tcp: &TcpStream) {
    let _ = tcp.shutdown(Shutdown::Write);
    let _ = tcp.set_read_timeout(Some(Duration::from_millis(200)));
    let start = Instant::now();
    let mut scratch = [0u8; 8192];
    let mut total = 0;
    while total < (1 << 20) && start.elapsed() < Duration::from_millis(500) {
        match tcp.read(&mut scratch) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &self.session {
            None => self.tcp.read(buf),
            Some(session) => {
                let deadline = self.deadline.get().unwrap_or_else(|| Instant::now() + Duration::from_secs(60));
                session.read(&self.tcp, deadline, buf)
            }
        }
    }
}

impl Write for &Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &self.session {
            None => (&self.tcp).write(buf),
            Some(session) => session.write(&self.tcp, buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &self.session {
            None => (&self.tcp).flush(),
            Some(session) => session.flush(&self.tcp),
        }
    }
}
