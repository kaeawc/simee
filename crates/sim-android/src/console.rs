//! A client for the emulator console (`adb emu`'s text protocol on the console port),
//! authenticating with `~/.emulator_console_auth_token` when it exists.
//!
//! [`send`] runs a batch of commands in one session, pipelined (one write, no round trip per
//! command). [`Session`] keeps a session open for sequences that need time between commands
//! (a button held down, a swipe's samples): the same connection serves the whole sequence
//! instead of one connect, banner and auth per command. The token is read from disk once and
//! kept; a console that rejects it (the file changed) gets the file read again, once.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

/// The token as last read from disk. Only a token that exists is kept: a missing or empty file
/// is looked for again, because a console creates it on first start.
static TOKEN: Mutex<Option<String>> = Mutex::new(None);

fn read_token_file() -> Option<String> {
    // `HOME` on unix, `USERPROFILE` on Windows: where the stock console keeps it too.
    let token = std::fs::read_to_string(std::env::home_dir()?.join(".emulator_console_auth_token")).ok()?;
    Some(token.trim().to_owned()).filter(|t| !t.is_empty())
}

/// The console's auth token, if the user has one.
fn auth_token() -> Option<String> {
    let mut cached = TOKEN.lock().unwrap_or_else(|e| e.into_inner());
    if cached.is_none() {
        *cached = read_token_file();
    }
    cached.clone()
}

/// Forgets the cached token and reads the file again; the new token, if it differs from `stale`.
fn refresh_token(stale: &str) -> Option<String> {
    let mut cached = TOKEN.lock().unwrap_or_else(|e| e.into_inner());
    *cached = read_token_file();
    cached.clone().filter(|t| t != stale)
}

/// Whether an error is the console refusing our token.
fn is_auth_failure(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains("authentication")
}

fn connect(port: u16) -> Result<TcpStream, String> {
    let addr = (Ipv4Addr::LOCALHOST, port).into();
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| format!("console {port}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    // Commands are small lines the guest acts on: don't let Nagle hold one back.
    let _ = s.set_nodelay(true);
    Ok(s)
}

/// Sends `commands` in one session and returns the reply lines, without the banner, the auth
/// acknowledgement and the `OK`s. A `KO:` reply fails the call with its reason.
pub fn send(port: u16, commands: &[String]) -> Result<Vec<String>, String> {
    let token = auth_token();
    match send_with(port, commands, token.as_deref()) {
        Err(reason) if is_auth_failure(&reason) => match token.as_deref().and_then(refresh_token) {
            Some(fresh) => send_with(port, commands, Some(&fresh)),
            None => Err(reason),
        },
        other => other,
    }
}

fn send_with(port: u16, commands: &[String], token: Option<&str>) -> Result<Vec<String>, String> {
    let mut s = connect(port)?;
    let mut script = String::new();
    if let Some(t) = token {
        script.push_str(&format!("auth {t}\r\n"));
    }
    for c in commands {
        script.push_str(c);
        script.push_str("\r\n");
    }
    script.push_str("quit\r\n");
    s.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    let lines: Vec<String> = BufReader::new(s).lines().map_while(Result::ok).map(|l| l.trim_end_matches('\r').to_owned()).collect();
    parse_replies(lines)
}

/// Drops the banner (everything up to the last `Android Console: type 'help'` line and its `OK`)
/// and the `OK`s; turns the first `KO:` into an error.
fn parse_replies(lines: Vec<String>) -> Result<Vec<String>, String> {
    let skip = lines.iter().rposition(|l| l.starts_with("Android Console: type 'help'")).map_or(0, |i| i + 2);
    let mut out = Vec::new();
    for l in lines.into_iter().skip(skip) {
        if let Some(reason) = l.strip_prefix("KO:") {
            return Err(reason.trim().to_owned());
        }
        if l != "OK" {
            out.push(l);
        }
    }
    Ok(out)
}

/// One console session, kept open across commands: each [`command`](Session::command) writes a
/// line and reads its reply, so the caller can pace the commands (hold a key, space a swipe's
/// samples) over one connection. Dropping it hangs up; the console ends a session on `quit` or
/// EOF alike.
pub struct Session {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Session {
    /// Connects and authenticates (when the console asks for it).
    pub fn open(port: u16) -> Result<Session, String> {
        let token = auth_token();
        match Self::open_with(port, token.as_deref()) {
            Err(reason) if is_auth_failure(&reason) => match token.as_deref().and_then(refresh_token) {
                Some(fresh) => Self::open_with(port, Some(&fresh)),
                None => Err(reason),
            },
            other => other,
        }
    }

    fn open_with(port: u16, token: Option<&str>) -> Result<Session, String> {
        let writer = connect(port)?;
        let reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
        let mut session = Session { reader, writer };
        let banner = session.read_reply()?;
        let wants_auth = banner.iter().any(|l| l.contains("Authentication required"));
        match (wants_auth, token) {
            (true, Some(token)) => drop(session.command(&format!("auth {token}"))?),
            (true, None) => return Err("console authentication is required but there is no ~/.emulator_console_auth_token".into()),
            (false, _) => {}
        }
        Ok(session)
    }

    /// Reads up to and including the next `OK`, or fails at a `KO:` line.
    fn read_reply(&mut self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Err("console closed the session".into()),
                Err(e) => return Err(e.to_string()),
                Ok(_) => {}
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if let Some(reason) = l.strip_prefix("KO:") {
                return Err(reason.trim().to_owned());
            }
            if l == "OK" {
                return Ok(out);
            }
            out.push(l.to_owned());
        }
    }

    /// Sends one command and returns its reply lines (without the `OK`).
    pub fn command(&mut self, command: &str) -> Result<Vec<String>, String> {
        self.writer.write_all(format!("{command}\r\n").as_bytes()).map_err(|e| e.to_string())?;
        self.read_reply()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A console that greets like simee, answers `ping` and `avd name`, and fails anything else.
    fn fake_console() -> u16 {
        fake_console_counting(None).0
    }

    /// The same, requiring `token` when given; also counts the sessions (accepted connections).
    fn fake_console_counting(token: Option<&'static str>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let sessions = Arc::new(AtomicUsize::new(0));
        let count = sessions.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                count.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut out = stream.try_clone().expect("clone");
                    let mut authed = token.is_none();
                    let banner: &[u8] = if authed {
                        b"Android Console: type 'help' for a list of commands\r\nOK\r\n"
                    } else {
                        b"Android Console: Authentication required\r\nAndroid Console: type 'auth <auth_token>' to authenticate\r\nOK\r\n"
                    };
                    out.write_all(banner).expect("banner");
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        let line = line.trim().to_owned();
                        let reply = match line.as_str() {
                            "quit" => break,
                            l if l.starts_with("auth") => {
                                // Without a required token any `auth` is accepted (the host may have one).
                                authed = token.is_none_or(|t| l == format!("auth {t}"));
                                if authed {
                                    "Android Console: type 'help' for a list of commands\r\nOK"
                                } else {
                                    "KO: authentication token does not match ~/.emulator_console_auth_token"
                                }
                            }
                            _ if !authed => "KO: unknown command, try 'help'",
                            "ping" => "I am alive!\r\nOK",
                            "avd name" => "pixel\r\nOK",
                            l if l.starts_with("event send") => "OK",
                            _ => "KO: unknown command",
                        };
                        if out.write_all(format!("{reply}\r\n").as_bytes()).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (port, sessions)
    }

    #[test]
    fn sessions_return_replies_without_banner_or_oks() {
        let port = fake_console();
        assert_eq!(send(port, &["ping".into(), "avd name".into()]), Ok(vec!["I am alive!".to_owned(), "pixel".to_owned()]));
        assert_eq!(send(port, &["dance".into()]), Err("unknown command".into()));
    }

    #[test]
    fn auth_banners_are_skipped() {
        let lines =
            ["Android Console: Authentication required", "OK", "Android Console: type 'help' for a list of commands", "OK", "x", "OK"];
        assert_eq!(parse_replies(lines.iter().map(|l| (*l).to_owned()).collect()), Ok(vec!["x".to_owned()]));
    }

    #[test]
    fn a_kept_session_runs_commands_one_at_a_time_on_one_connection() {
        let (port, sessions) = fake_console_counting(None);
        let mut s = Session::open(port).expect("open");
        assert_eq!(s.command("ping"), Ok(vec!["I am alive!".to_owned()]));
        assert_eq!(s.command("event send EV_KEY:1:1"), Ok(vec![]));
        assert_eq!(s.command("dance"), Err("unknown command".into()), "a KO fails that command, not the session");
        assert_eq!(s.command("avd name"), Ok(vec!["pixel".to_owned()]));
        assert_eq!(sessions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_kept_session_authenticates_when_asked() {
        let (port, _) = fake_console_counting(Some("secret"));
        assert!(Session::open_with(port, None).err().expect("no token").contains("authentication is required"));
        let mut s = Session::open_with(port, Some("secret")).expect("open");
        assert_eq!(s.command("ping"), Ok(vec!["I am alive!".to_owned()]));
        let err = Session::open_with(port, Some("wrong")).err().expect("wrong token");
        assert!(is_auth_failure(&err), "{err}");
        assert_eq!(send_with(port, &["ping".into()], Some("secret")), Ok(vec!["I am alive!".to_owned()]));
        assert!(send_with(port, &["ping".into()], Some("wrong")).is_err());
    }

    #[test]
    fn a_closed_console_fails_the_command() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                s.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n").expect("banner");
                // then hang up
            }
        });
        let mut s = Session::open(port).expect("open");
        // A clean close or a reset, depending on timing: either way an error, not a hang.
        assert!(s.command("ping").is_err());
    }

    /// `cargo test --release -p simee-android console_bench -- --ignored --nocapture`
    ///
    /// Sessions and time per call against the fake console: the old shape of a button press
    /// (two `send` sessions around the hold), a 10-step swipe (12 sessions), against one kept
    /// [`Session`]; and the token lookup each `send` paid.
    #[test]
    #[ignore = "benchmark"]
    fn console_bench() {
        use std::time::Instant;
        let (port, sessions) = fake_console_counting(None);
        let down = "event send EV_KEY:KEY_HOME:1".to_owned();
        let up = "event send EV_KEY:KEY_HOME:0".to_owned();
        let median = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.total_cmp(b));
            v[v.len() / 2]
        };
        let measure = |name: &str, calls_per_run: usize, f: &dyn Fn()| {
            let before = sessions.load(Ordering::SeqCst);
            let runs: Vec<f64> = (0..21)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..calls_per_run {
                        f();
                    }
                    t.elapsed().as_secs_f64() * 1e6 / calls_per_run as f64
                })
                .collect();
            let n = (sessions.load(Ordering::SeqCst) - before) as f64 / (21 * calls_per_run) as f64;
            println!("{name:<28} sessions/call {n:>5.1}   median {:>8.1} us/call", median(runs));
        };
        measure("button: 2 sends (old)", 20, &|| {
            send(port, std::slice::from_ref(&down)).expect("down");
            send(port, std::slice::from_ref(&up)).expect("up");
        });
        measure("button: one Session", 20, &|| {
            let mut s = Session::open(port).expect("open");
            s.command(&down).expect("down");
            s.command(&up).expect("up");
        });
        measure("swipe: 12 sends (old)", 10, &|| {
            for _ in 0..12 {
                send(port, std::slice::from_ref(&down)).expect("step");
            }
        });
        measure("swipe: one Session", 10, &|| {
            let mut s = Session::open(port).expect("open");
            for _ in 0..12 {
                s.command(&down).expect("step");
            }
        });
        let t = Instant::now();
        for _ in 0..2000 {
            std::hint::black_box(read_token_file());
        }
        let file = t.elapsed().as_secs_f64() * 1e6 / 2000.0;
        let t = Instant::now();
        for _ in 0..2000 {
            std::hint::black_box(auth_token());
        }
        let cached = t.elapsed().as_secs_f64() * 1e6 / 2000.0;
        println!("token: file read {file:.2} us, lookup {cached:.2} us (the cache holds only an existing token)");
    }
}
