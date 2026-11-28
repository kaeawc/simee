//! The audit log: one JSON line per tool call, with the client, the tool, its arguments with
//! secrets redacted, how it ended and how long it took.
//!
//! The file and the rotated ones next to it are mode 0600 in a directory made 0700, opened without
//! following a link. It is capped: past `max_bytes` the file becomes `<name>.1` (what was `.1` is
//! `.2`, and so on; the oldest of [`KEEP`] goes), so the log holds at most `max_bytes * (KEEP + 1)`
//! of history. A line is one `write` to a file opened for appending, so processes sharing a file
//! don't tear each other's lines (rotation can still race: give each its own file for a clean
//! history). Writing is best effort once the server runs: a failing disk is reported on stderr and
//! the call goes on; starting with a log that can't be opened is an error.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How many rotated files are kept next to the current one.
pub const KEEP: usize = 3;
/// The default size cap of the current file.
pub const DEFAULT_MAX_BYTES: u64 = 8 << 20;
/// The log's name in simee's state directory.
pub const FILE: &str = "mcp-audit.log";

/// What a call came to.
pub mod status {
    /// It ran and the method succeeded.
    pub const OK: &str = "ok";
    /// It ran and the method failed.
    pub const ERROR: &str = "error";
    /// It didn't run: policy, a read-only server, a missing confirmation or the host's admission.
    pub const REFUSED: &str = "refused";
    /// It didn't run yet: the client was asked to confirm (the current revision's retry follows).
    pub const INPUT_REQUIRED: &str = "input_required";
}

/// One call, as the log records it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub client: String,
    pub tool: String,
    /// The arguments as the client sent them (before the server added or removed any).
    pub args: Value,
    pub status: &'static str,
    pub duration_ms: u64,
    /// Why it was refused or failed, shortened.
    pub detail: Option<String>,
    /// The host-health verdict a boot came back with (`admit`, `hold`, `backoff`).
    pub admission: Option<String>,
}

/// How long a `detail` is kept.
const DETAIL_CHARS: usize = 300;

impl Entry {
    fn to_json(&self, at_ms: u64) -> Value {
        let mut line = json!({"ts_ms": at_ms, "client": self.client, "tool": self.tool, "args": redact(&self.tool, &self.args),
            "status": self.status, "duration_ms": self.duration_ms});
        if let Some(detail) = &self.detail {
            line["detail"] = scrub_string(&clip(detail, DETAIL_CHARS)).into();
        }
        if let Some(verdict) = &self.admission {
            line["admission"] = verdict.clone().into();
        }
        line
    }
}

/// Where and how large the log is.
#[derive(Clone, Debug)]
pub struct Config {
    pub path: PathBuf,
    pub max_bytes: u64,
}

impl Config {
    pub fn new(path: PathBuf) -> Config {
        Config { path, max_bytes: DEFAULT_MAX_BYTES }
    }
}

/// The log's default place: [`FILE`] in simee's state directory.
pub fn default_path() -> io::Result<PathBuf> {
    crate::http::state_dir()
        .map(|d| d.join(FILE))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory: set HOME or SIMEE_STATE_DIR, or pass --audit-log"))
}

/// The open log.
pub struct AuditLog {
    writer: Mutex<Writer>,
}

struct Writer {
    config: Config,
    file: File,
    size: u64,
    /// Write failures seen, so a broken disk is said once and then every hundredth time.
    failures: u64,
}

impl AuditLog {
    /// Opens (creating, and making its directory) the log.
    pub fn open(config: Config) -> io::Result<AuditLog> {
        let file = open_file(&config.path)?;
        let size = file.metadata()?.len();
        Ok(AuditLog { writer: Mutex::new(Writer { config, file, size, failures: 0 }) })
    }

    pub fn path(&self) -> PathBuf {
        lock(&self.writer).config.path.clone()
    }

    /// Appends one call. Never fails the call it records.
    pub fn record(&self, entry: &Entry) {
        let mut line = entry.to_json(now_ms()).to_string();
        line.push('\n');
        let mut writer = lock(&self.writer);
        if let Err(e) = writer.append(line.as_bytes()) {
            writer.failures += 1;
            if writer.failures % 100 == 1 {
                eprintln!("simee mcp: cannot write the audit log {}: {e}", writer.config.path.display());
            }
        }
    }
}

impl Writer {
    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size + line.len() as u64 > self.config.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }

    /// The current file becomes `.1`, each older one the next number, and the oldest goes.
    fn rotate(&mut self) -> io::Result<()> {
        let path = &self.config.path;
        let numbered = |n: usize| {
            let mut name = path.clone().into_os_string();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        };
        match std::fs::remove_file(numbered(KEEP)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        for n in (1..KEEP).rev() {
            match std::fs::rename(numbered(n), numbered(n + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        std::fs::rename(path, numbered(1))?;
        self.file = open_file(path)?;
        self.size = 0;
        Ok(())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The file for appending: made 0600 if new, made 0600 if it wasn't, and never a link.
fn open_file(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        make_dir(dir)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o177 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(file)
}

fn make_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

// ---- redaction -----------------------------------------------------------------------------

/// Words that make a key's value a secret (matched anywhere in the key, whatever its case).
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "passcode",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "authorization",
    "credential",
    "cookie",
    "private_key",
    "privatekey",
    "bearer",
];

/// Words that start a secret in text: `password=hunter2`, `Authorization: Bearer x`.
const SECRET_WORDS: &[&str] =
    &["password", "passwd", "secret", "token", "api_key", "apikey", "api-key", "authorization", "bearer", "access_key", "private_key"];

/// What stands for a removed value.
pub const REDACTED: &str = "[redacted]";

/// The longest string kept whole: a file's contents or a payload isn't an argument worth a megabyte
/// of log.
const MAX_STRING: usize = 256;

/// Arguments that are what the client typed, wrote or delivered: the log keeps their size, not
/// their text (a password typed into a device is still a password).
fn content_fields(tool: &str) -> &'static [&'static str] {
    match tool {
        "type" => &["text"],
        "write" => &["text", "base64"],
        "defaults_write" | "prefs_write" => &["value"],
        "prefs_batch" => &["ops"],
        "push" => &["payload"],
        "notify" => &["extras"],
        _ => &[],
    }
}

/// `args` as the log keeps them: secret keys and what a tool carries as content removed, secrets
/// inside text masked, long strings cut.
pub fn redact(tool: &str, args: &Value) -> Value {
    let mut out = args.clone();
    if let Some(fields) = out.as_object_mut() {
        for name in content_fields(tool) {
            if let Some(v) = fields.get_mut(*name) {
                *v = match v {
                    Value::String(s) => format!("{REDACTED} ({} bytes)", s.len()).into(),
                    Value::Null => Value::Null,
                    _ => REDACTED.into(),
                };
            }
        }
    }
    scrub(&mut out, 0);
    out
}

fn scrub(value: &mut Value, depth: usize) {
    // Nothing a client sends is nested this deep on purpose.
    if depth > 16 {
        *value = REDACTED.into();
        return;
    }
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_secret_key(key) {
                    *v = REDACTED.into();
                } else {
                    scrub(v, depth + 1);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| scrub(v, depth + 1)),
        Value::String(s) => *s = scrub_string(&clip(s, MAX_STRING)),
        _ => {}
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|w| key.contains(w))
}

/// `text` cut to `max` characters, saying how long it was.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…[{} bytes]", &text[..end], text.len()),
        None => text.to_owned(),
    }
}

/// `text` with the secrets in it masked: the value after `password=`, `token:`, `Bearer `, and
/// the user and password of a URL.
pub fn scrub_string(text: &str) -> String {
    mask_userinfo(&mask_words(text))
}

fn is_delimiter(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'&' | b',' | b';' | b'"' | b'\'' | b')' | b'}' | b']' | b'<' | b'>')
}

fn mask_words(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let blanks = |mut i: usize| {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        i
    };
    let word_end = |mut i: usize| {
        while i < bytes.len() && !is_delimiter(bytes[i]) {
            i += 1;
        }
        i
    };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some((start, len)) = SECRET_WORDS.iter().filter_map(|w| lower[at..].find(w).map(|i| (at + i, w.len()))).min() {
        let end_of_word = start + len;
        out.push_str(&text[at..end_of_word]);
        at = end_of_word;
        // The value follows after blanks and `=` or `:`; for `Bearer x` after the blanks alone.
        let mut i = blanks(end_of_word);
        let separated = bytes.get(i).is_some_and(|b| matches!(b, b'=' | b':'));
        if separated {
            i = blanks(i + 1);
            while bytes.get(i).is_some_and(|b| matches!(b, b'"' | b'\'')) {
                i += 1;
            }
        } else if !(lower[start..end_of_word] == *"bearer" && i > end_of_word) {
            // `tokens`, `token_count`: a word that only starts like one.
            continue;
        }
        out.push_str(&text[end_of_word..i]);
        let mut end = word_end(i);
        // A scheme in front of the credential (`Authorization: Bearer x`) takes the next word too.
        if matches!(&lower[i..end], "bearer" | "basic" | "token") && bytes.get(end) == Some(&b' ') {
            end = word_end(end + 1);
        }
        if end > i {
            out.push_str(REDACTED);
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

fn mask_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let authority_end = tail.find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace()).unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str(REDACTED);
                out.push_str(&tail[at..authority_end]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

// ---- querying ------------------------------------------------------------------------------

/// Which entries to read back.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// The most recent this many (default [`Query::DEFAULT_LIMIT`]).
    pub limit: Option<usize>,
    pub tool: Option<String>,
    pub client: Option<String>,
    pub status: Option<String>,
}

impl Query {
    pub const DEFAULT_LIMIT: usize = 50;

    fn matches(&self, entry: &Value) -> bool {
        let is = |want: &Option<String>, key: &str| want.as_deref().is_none_or(|w| entry[key].as_str() == Some(w));
        is(&self.tool, "tool") && is(&self.client, "client") && is(&self.status, "status")
    }
}

/// The matching entries of the log at `path` and its rotated files, oldest first, the most recent
/// `limit` of them. A line that isn't an entry (a torn write) is skipped.
pub fn query(path: &Path, query: &Query) -> io::Result<Vec<Value>> {
    let limit = query.limit.unwrap_or(Query::DEFAULT_LIMIT);
    let mut found = std::collections::VecDeque::new();
    let mut files: Vec<PathBuf> = (1..=KEEP)
        .rev()
        .map(|n| {
            let mut name = path.to_path_buf().into_os_string();
            name.push(format!(".{n}"));
            PathBuf::from(name)
        })
        .collect();
    files.push(path.to_path_buf());
    let mut opened = 0;
    for file in files {
        let file = match File::open(&file) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        opened += 1;
        for line in io::BufReader::new(file).lines() {
            let Ok(Ok(entry)) = line.map(|l| serde_json::from_str::<Value>(&l)) else { continue };
            if entry.is_object() && query.matches(&entry) {
                found.push_back(entry);
                if found.len() > limit {
                    found.pop_front();
                }
            }
        }
    }
    if opened == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("no audit log at {}", path.display())));
    }
    Ok(found.into())
}

/// An entry as one line for a person: time, client, tool, status, duration, and why.
pub fn describe(entry: &Value) -> String {
    let text = |key: &str| entry[key].as_str().unwrap_or("?").to_owned();
    let mut line = format!(
        "{} {} {} {} {} ms",
        entry["ts_ms"].as_u64().unwrap_or(0),
        text("client"),
        text("tool"),
        text("status"),
        entry["duration_ms"]
    );
    if let Some(detail) = entry["detail"].as_str() {
        line.push_str(&format!(": {detail}"));
    }
    line.push_str(&format!(" {}", entry["args"]));
    line
}

#[cfg(test)]
mod tests;
