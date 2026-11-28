//! A WebRTC publisher for H.264 device video: it takes an Annex B elementary stream (access units
//! with presentation times) and publishes it over WHIP (RFC 9725) to a media server such as
//! MediaMTX, which viewers then play over WHEP.
//!
//! - [`whip`]: the WHIP client: POST the SDP offer, `201 Created` + `Location`, PATCH trickle ICE,
//!   DELETE to end, bearer tokens, and the refusal of plaintext WHIP to non-loopback hosts.
//! - [`http`] and [`url`]: the blocking HTTP/1.1 client over `std::net` that WHIP runs on.
//! - [`h264`]: Annex B splitting, access units, the SPS `profile-level-id` and the check that a
//!   stream's SPS matches the negotiated one, and SPS/PPS before every IDR.
//! - [`rtp`]: RFC 6184 packetization (single NAL units and FU-A at an MTU) and a depacketizer.
//! - [`sdp`]: what the publisher reads from the answer, and trickle ICE fragments.
//! - [`publisher`]: [`Publisher`], which drives str0m (sans-IO WebRTC: ICE, DTLS-SRTP, NACK/RTX,
//!   TWCC) on one thread with one UDP socket and reports keyframe requests and bandwidth
//!   estimates to the encoder through a callback.
//! - [`datachannel`]: the `simee-input` data channel protocol: touch, keys, text, buttons,
//!   rotation, fold posture and wheel from the viewer, validated, rate limited and coalesced, to
//!   an [`InputSink`], with acks that echo the viewer's timestamp for latency measurement.
//!
//! No async runtime: the WHIP requests block the caller, and the media path is one thread per
//! publisher that sleeps on its socket. Design notes and the plan: `docs/streaming.md`.

pub mod datachannel;
pub mod h264;
pub mod http;
pub mod publisher;
pub mod rtp;
pub mod sdp;
pub mod url;
pub mod whip;

pub use datachannel::{DisplayMap, InputConfig, InputEvent, InputSession, InputSink};
pub use h264::{Profile, ProfileLevelId};
pub use publisher::{BitrateSource, KeyframeKind, Publisher, PublisherConfig, PublisherEvent, Sent, crypto_available};
pub use url::Url;
pub use whip::{WhipClient, WhipOptions, WhipSession};

/// What can go wrong publishing.
#[derive(Debug)]
pub enum Error {
    /// A malformed argument: a URL, a header value, an MTU, a stream that isn't H.264.
    InvalidInput(String),
    /// The endpoint is plaintext `http://` to a host that isn't loopback, and
    /// [`WhipOptions::allow_insecure_http`] is off: the bearer token and the DTLS fingerprints
    /// would cross the network in the clear.
    Insecure(String),
    /// Something this build can't do: `https://` (no TLS client yet), or no DTLS backend on this
    /// target (Linux without the `rust-crypto` feature).
    Unsupported(String),
    /// The WHIP server answered with an unexpected status.
    Status {
        status: u16,
        method: &'static str,
        body: String,
    },
    /// The HTTP exchange broke the protocol: a bad status line, headers or chunking, or no
    /// `Location` on a `201`.
    Protocol(String),
    /// The stream's SPS doesn't match the `profile-level-id` the session negotiated: a viewer
    /// would refuse to decode it.
    ProfileMismatch {
        negotiated: ProfileLevelId,
        stream: ProfileLevelId,
    },
    /// An IDR with no SPS or PPS before it, in this access unit or any earlier one.
    MissingParameterSets,
    /// No answer, or no ICE/DTLS connection, within the deadline.
    Timeout(String),
    /// The WebRTC stack refused an operation (a bad answer, a DTLS failure).
    Rtc(String),
    /// The publisher's media thread has stopped (the peer went away, or it was closed).
    Closed,
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidInput(m) => write!(f, "invalid input: {m}"),
            Error::Insecure(m) => write!(f, "refusing plaintext WHIP: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::Status { status, method, body } if body.is_empty() => {
                write!(f, "WHIP {method} answered {status}")
            }
            Error::Status { status, method, body } => {
                write!(f, "WHIP {method} answered {status}: {body}")
            }
            Error::Protocol(m) => write!(f, "HTTP protocol error: {m}"),
            Error::ProfileMismatch { negotiated, stream } => write!(
                f,
                "the stream's SPS is H.264 {stream} ({}), but the session negotiated {negotiated} ({})",
                stream.profile(),
                negotiated.profile()
            ),
            Error::MissingParameterSets => write!(f, "an IDR arrived before any SPS and PPS"),
            Error::Timeout(m) => write!(f, "timed out: {m}"),
            Error::Rtc(m) => write!(f, "WebRTC: {m}"),
            Error::Closed => write!(f, "the publisher has stopped"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
