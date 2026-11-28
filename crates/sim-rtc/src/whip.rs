//! The WHIP client (RFC 9725): POST the SDP offer, read the answer and the session's resource URL
//! from `201 Created` + `Location`, PATCH trickle ICE fragments (RFC 8840) with the `ETag`, and
//! DELETE the resource to end the session.
//!
//! - setup, `201` and `Location`: §4.2; redirects (307/308) on the POST: §4.5
//! - trickle ICE and ICE restarts over PATCH: §4.3
//! - session termination by DELETE: §4.2
//! - bearer tokens: §4.7
//!
//! Plaintext `http://` is refused to any host that isn't loopback unless
//! [`WhipOptions::allow_insecure_http`] is set: the token and the DTLS fingerprints that
//! authenticate the media would cross the network in the clear. That check also applies to the
//! `Location` and to redirects, so a server can't send the token somewhere else in the clear.

use std::time::Duration;

use crate::http::{self, Response};
use crate::url::{Scheme, Url};
use crate::{Error, Result};

/// RFC 9725 §4.5: follow a few redirects on the POST, no more.
const MAX_REDIRECTS: usize = 3;

#[derive(Clone)]
pub struct WhipOptions {
    /// Sent as `Authorization: Bearer <token>` on every request.
    pub bearer_token: Option<String>,
    /// Allow plaintext `http://` to hosts that aren't loopback (a trusted LAN).
    pub allow_insecure_http: bool,
    /// The deadline for each request, from connect to the end of the body.
    pub timeout: Duration,
}

/// Debug that never prints the bearer token.
impl std::fmt::Debug for WhipOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhipOptions")
            .field("bearer_token", &self.bearer_token.as_ref().map(|_| "<redacted>"))
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Default for WhipOptions {
    fn default() -> Self {
        WhipOptions { bearer_token: None, allow_insecure_http: false, timeout: Duration::from_secs(15) }
    }
}

/// One ingest session on the server.
#[derive(Clone, Debug)]
pub struct WhipSession {
    /// The SDP answer.
    pub answer: String,
    /// The session's resource, from `Location`: PATCH and DELETE go here.
    pub resource: Url,
    /// The entity tag for conditional PATCH requests (`If-Match`).
    pub etag: Option<String>,
}

/// What a trickle PATCH did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trickle {
    /// `204` (or `200`): the server took the candidates.
    Accepted,
    /// `405` or `501`: the server doesn't trickle; the candidates in the offer are all it gets.
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct WhipClient {
    endpoint: Url,
    options: WhipOptions,
}

impl WhipClient {
    /// A client for this endpoint. Refuses, before any network traffic, a URL that isn't
    /// `http(s)`, `https` (no TLS client yet), and plaintext to a host that isn't loopback unless
    /// allowed.
    pub fn new(endpoint: &str, options: WhipOptions) -> Result<WhipClient> {
        let endpoint = Url::parse(endpoint)?;
        if let Some(token) = &options.bearer_token
            && (token.is_empty() || token.bytes().any(|b| b.is_ascii_control()))
        {
            return Err(Error::InvalidInput("the bearer token is empty or has control characters".into()));
        }
        check_transport(&endpoint, &options)?;
        Ok(WhipClient { endpoint, options })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// POST the offer; the answer and the resource on `201 Created`.
    pub fn publish(&self, offer: &str) -> Result<WhipSession> {
        let mut url = self.endpoint.clone();
        for _ in 0..=MAX_REDIRECTS {
            let r = self.send("POST", &url, &[("Content-Type", "application/sdp")], offer.as_bytes())?;
            if matches!(r.status, 307 | 308) {
                let to = r.header("location").ok_or_else(|| Error::Protocol(format!("{} redirect without a Location", r.status)))?;
                url = url.join(to)?;
                check_transport(&url, &self.options)?;
                continue;
            }
            if r.status != 201 {
                return Err(status_error("POST", &r));
            }
            let location = r.header("location").ok_or_else(|| Error::Protocol("the 201 Created has no Location header".into()))?;
            let resource = url.join(location)?;
            check_transport(&resource, &self.options)?;
            let answer = r.text();
            if !answer.trim_start().starts_with("v=0") {
                // The server made a session: don't leave it behind.
                let _ = self.delete(&resource);
                return Err(Error::Protocol("the 201 Created body is not an SDP answer".into()));
            }
            return Ok(WhipSession { answer, resource, etag: r.header("etag").map(str::to_owned) });
        }
        Err(Error::Protocol(format!("more than {MAX_REDIRECTS} redirects")))
    }

    /// PATCH a trickle ICE fragment (`application/trickle-ice-sdpfrag`, see
    /// [`crate::sdp::trickle_fragment`]) to the session, conditional on its `ETag` when the server
    /// gave one. A new `ETag` in the reply (an ICE restart) replaces the session's.
    pub fn patch(&self, session: &mut WhipSession, fragment: &str) -> Result<Trickle> {
        let mut headers = vec![("Content-Type", "application/trickle-ice-sdpfrag")];
        let etag = session.etag.clone();
        if let Some(etag) = &etag {
            headers.push(("If-Match", etag));
        }
        let r = self.send("PATCH", &session.resource, &headers, fragment.as_bytes())?;
        match r.status {
            200 | 204 => {
                if let Some(etag) = r.header("etag") {
                    session.etag = Some(etag.to_owned());
                }
                Ok(Trickle::Accepted)
            }
            405 | 501 => Ok(Trickle::Unsupported),
            _ => Err(status_error("PATCH", &r)),
        }
    }

    /// DELETE the session. A `404` counts as done: the server already dropped it.
    pub fn delete(&self, resource: &Url) -> Result<()> {
        check_transport(resource, &self.options)?;
        let r = self.send("DELETE", resource, &[], b"")?;
        match r.status {
            200..=299 | 404 => Ok(()),
            _ => Err(status_error("DELETE", &r)),
        }
    }

    fn send(&self, method: &str, url: &Url, headers: &[(&str, &str)], body: &[u8]) -> Result<Response> {
        let auth = self.options.bearer_token.as_ref().map(|t| format!("Bearer {t}"));
        let mut all: Vec<(&str, &str)> = headers.to_vec();
        if let Some(auth) = &auth {
            all.push(("Authorization", auth));
        }
        http::request(method, url, &all, body, self.options.timeout)
    }
}

/// The transport policy: no `https` yet, and plaintext only to loopback unless allowed.
pub fn check_transport(url: &Url, options: &WhipOptions) -> Result<()> {
    match url.scheme() {
        Scheme::Https => Err(Error::Unsupported(format!(
            "{url}: https WHIP endpoints need a TLS client, which isn't built yet; use http to a \
             loopback host (or a trusted LAN host with allow_insecure_http)"
        ))),
        Scheme::Http if url.is_loopback() || options.allow_insecure_http => Ok(()),
        Scheme::Http => Err(Error::Insecure(format!(
            "{url} is plaintext http to a host that isn't loopback; set allow_insecure_http for a \
             trusted LAN"
        ))),
    }
}

fn status_error(method: &'static str, r: &Response) -> Error {
    let mut body = r.text();
    body.truncate(body.char_indices().nth(300).map_or(body.len(), |(i, _)| i));
    Error::Status { status: r.status, method, body: body.trim().to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_insecure_and_tls_endpoints_before_connecting() {
        let opts = WhipOptions::default();
        for ok in ["http://127.0.0.1:8889/sim/whip", "http://localhost:8889/x", "http://[::1]:1/x"] {
            WhipClient::new(ok, opts.clone()).expect(ok);
        }
        for lan in ["http://192.168.1.20:8889/sim/whip", "http://media.example.com/whip"] {
            assert!(matches!(WhipClient::new(lan, opts.clone()), Err(Error::Insecure(_))), "{lan}");
            let allowed = WhipOptions { allow_insecure_http: true, ..opts.clone() };
            WhipClient::new(lan, allowed).expect(lan);
        }
        assert!(matches!(WhipClient::new("https://127.0.0.1/whip", opts.clone()), Err(Error::Unsupported(_))));
        assert!(matches!(WhipClient::new("ws://127.0.0.1/whip", opts.clone()), Err(Error::InvalidInput(_))));
        let bad_token = WhipOptions { bearer_token: Some("a\nb".into()), ..opts };
        assert!(matches!(WhipClient::new("http://127.0.0.1/w", bad_token), Err(Error::InvalidInput(_))));
    }
}
