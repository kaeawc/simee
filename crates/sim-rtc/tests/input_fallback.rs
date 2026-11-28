//! `Publisher::connect_with_input_or_without` against a fake WHIP server (#381): a 406 that names
//! `application` is retried once without the data channel; no other answer is retried.
//!
//! Needs a DTLS backend (the publisher checks before it POSTs): Apple targets, or Linux with
//! `--features rust-crypto` (skipped otherwise).

mod common;

use common::{FakeServer, Reply, Request};
use simee_rtc::{Error, InputConfig, InputEvent, InputSession, Publisher, PublisherConfig};
use str0m::change::SdpOffer;

fn session() -> InputSession {
    InputSession::new(|_: &InputEvent| -> Result<(), String> { Ok(()) }, InputConfig::default())
}

fn has_channel(req: &Request) -> bool {
    req.body.lines().any(|l| l.starts_with("m=application "))
}

fn posts(server: &FakeServer) -> Vec<Request> {
    server.requests().into_iter().filter(|r| r.method == "POST").collect()
}

fn refuse_application() -> Reply {
    Reply::new(406).body(r#"{"status":"error","error":"unsupported media 'application'"}"#)
}

#[test]
fn a_406_naming_application_is_retried_without_the_channel() {
    if !simee_rtc::crypto_available() {
        common::skip("no DTLS backend in this build (Linux needs --features rust-crypto)");
        return;
    }
    let viewers = std::sync::Mutex::new(Vec::new());
    let server = FakeServer::start(move |req| match req.method.as_str() {
        "POST" if has_channel(req) => refuse_application(),
        "POST" => {
            let offer = SdpOffer::from_sdp_string(&req.body).expect("offer");
            let (answer, viewer) = common::receiver(offer);
            viewers.lock().expect("lock").push(viewer);
            Reply::new(201).header("Content-Type", "application/sdp").header("Location", "/whip/session/1").body(&answer)
        }
        "DELETE" => Reply::new(200),
        _ => Reply::new(405),
    });
    let config = PublisherConfig::new(server.url("/whip"));
    let (publisher, note) = Publisher::connect_with_input_or_without(config, session(), |_| {}).expect("falls back");
    let note = note.expect("a note");
    assert!(note.contains("refused the input data channel") && note.contains("application"), "{note}");
    let posts = posts(&server);
    assert_eq!(posts.len(), 2, "one offer with the channel, one without");
    assert!(has_channel(&posts[0]) && !has_channel(&posts[1]), "first {:?} second {:?}", posts[0].body, posts[1].body);
    assert!(posts[1].body.contains("m=video "), "{}", posts[1].body);
    publisher.close().expect("close");
}

#[test]
fn a_406_for_another_reason_is_not_retried() {
    if !simee_rtc::crypto_available() {
        common::skip("no DTLS backend in this build (Linux needs --features rust-crypto)");
        return;
    }
    let server = FakeServer::start(|_| Reply::new(406).body(r#"{"status":"error","error":"unsupported media 'audio'"}"#));
    let config = PublisherConfig::new(server.url("/whip"));
    match Publisher::connect_with_input_or_without(config, session(), |_| {}) {
        Err(Error::Status { status: 406, body, .. }) => assert!(body.contains("audio"), "{body}"),
        other => panic!("expected the 406, got {:?}", other.map(|(_, n)| n)),
    }
    assert_eq!(posts(&server).len(), 1);
}

#[test]
fn another_status_is_not_retried_either() {
    if !simee_rtc::crypto_available() {
        common::skip("no DTLS backend in this build (Linux needs --features rust-crypto)");
        return;
    }
    let server = FakeServer::start(|_| Reply::new(401).body("application token required"));
    let config = PublisherConfig::new(server.url("/whip"));
    assert!(matches!(Publisher::connect_with_input_or_without(config, session(), |_| {}), Err(Error::Status { status: 401, .. })));
    assert_eq!(posts(&server).len(), 1);
}

#[test]
fn only_one_retry_is_made_even_when_the_retry_is_refused_too() {
    if !simee_rtc::crypto_available() {
        common::skip("no DTLS backend in this build (Linux needs --features rust-crypto)");
        return;
    }
    let server = FakeServer::start(|_| refuse_application());
    let config = PublisherConfig::new(server.url("/whip"));
    match Publisher::connect_with_input_or_without(config, session(), |_| {}) {
        Err(Error::Status { status: 406, .. }) => {}
        other => panic!("expected the retry's 406, got {:?}", other.map(|(_, n)| n)),
    }
    let posts = posts(&server);
    assert_eq!(posts.len(), 2, "the offer and one retry, never a third");
    assert!(has_channel(&posts[0]) && !has_channel(&posts[1]));
}
