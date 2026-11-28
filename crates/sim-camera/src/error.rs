//! The crate's error type: every failure names what was asked for and, where there is one, the fix.

use crate::tcc::Authorization;

pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong. `Clone` so one capture failure reaches every subscriber of the capture.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A source id or parameter that doesn't parse or can't be drawn (an unknown kind, an empty
    /// QR text, a frame size out of range).
    #[error("{0}")]
    Invalid(String),
    /// No host camera matches the query; `available` lists the names that exist.
    #[error("no camera matches {query:?}; available: {}", list_or_none(available))]
    NotFound { query: String, available: Vec<String> },
    /// More than one host camera matches the query; `matches` lists them as `name (id)`.
    #[error("{query:?} matches more than one camera: {}; use more of the name or the id", matches.join(", "))]
    Ambiguous { query: String, matches: Vec<String> },
    /// The host's camera permission is missing: never answered with black frames.
    #[error("camera permission is {}: {fix}", status.as_str())]
    Permission { status: Authorization, fix: String },
    /// A source kind this host or this build can't serve yet.
    #[error("{0}")]
    Unsupported(String),
    /// A host API or framework this needs isn't there (an older macOS, a missing symbol).
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Reading a file or a device node failed.
    #[error("{0}")]
    Io(String),
    /// A running capture failed (frames stopped, a frame didn't decode).
    #[error("capture failed: {0}")]
    Capture(String),
    /// The camera is in use by another program (V4L2 `EBUSY`); retry once it lets go.
    #[error("{0}")]
    Busy(String),
    /// The source went away: its camera was unplugged before or during the capture (`ENODEV`, a
    /// disconnected AVCaptureDevice). The hub reports it as [`crate::Event::SourceMissing`].
    #[error("source missing: {0}")]
    Missing(String),
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() { "none".to_string() } else { items.join(", ") }
}
